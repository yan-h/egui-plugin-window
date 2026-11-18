//! A key the editor leaves unclaimed reaches the host's view while the editor
//! is in its own window and after it is docked again, with the moves made by
//! this crate: it sets no next responder, and baseview's forward to the view
//! it opened in survives both moves. baseview's own `host_keys` test makes
//! the same moves by hand.
//! AppKit needs the process's main thread, which the Rust test harness does
//! not use.
#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions};
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::AnyObject;
    use objc2::{
        ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    };
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSView,
        NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSUserDefaults};
    use plugin_own_window::OwnWindow;
    use raw_window_handle::{AppKitWindowHandle, HasRawWindowHandle, RawWindowHandle};

    define_class!(
        #[unsafe(super = NSView)]
        #[thread_kind = MainThreadOnly]
        #[ivars = Cell<[usize; 3]>]
        struct HostView;

        impl HostView {
            #[unsafe(method(keyDown:))]
            fn key_down(&self, _: &NSEvent) {
                self.count(0);
            }

            #[unsafe(method(keyUp:))]
            fn key_up(&self, _: &NSEvent) {
                self.count(1);
            }

            #[unsafe(method(flagsChanged:))]
            fn flags_changed(&self, _: &NSEvent) {
                self.count(2);
            }
        }
    );

    impl HostView {
        fn count(&self, kind: usize) {
            let mut counts = self.ivars().get();
            counts[kind] += 1;
            self.ivars().set(counts);
        }
    }

    struct Parent(*mut c_void);

    unsafe impl HasRawWindowHandle for Parent {
        fn raw_window_handle(&self) -> RawWindowHandle {
            let mut handle = AppKitWindowHandle::empty();
            handle.ns_view = self.0;
            RawWindowHandle::AppKit(handle)
        }
    }

    struct Handler(Arc<AtomicBool>);

    impl WindowHandler for Handler {
        fn on_frame(&mut self, _: &mut Window) {}

        fn on_event(&mut self, _: &mut Window, _: Event) -> EventStatus {
            if self.0.load(Ordering::Relaxed) {
                EventStatus::Captured
            } else {
                EventStatus::Ignored
            }
        }
    }

    /// Cmd-S, pressed and released, sent to the view as AppKit dispatches it.
    fn send_save(view: &NSView) {
        let command = NSEventModifierFlags::Command;
        let text = NSString::from_str("s");
        for (kind, modifiers, code) in [
            (NSEventType::FlagsChanged, command, 55_u16),
            (NSEventType::KeyDown, command, 1),
            (NSEventType::KeyUp, command, 1),
            (NSEventType::FlagsChanged, NSEventModifierFlags::empty(), 55),
        ] {
            // The constructor's NSGraphicsContext argument is always nil.
            let event: Option<Retained<NSEvent>> = unsafe {
                msg_send![
                    NSEvent::class(), keyEventWithType: kind,
                    location: NSPoint::ZERO, modifierFlags: modifiers, timestamp: 0.0_f64,
                    windowNumber: view.window().unwrap().windowNumber(),
                    context: None::<&AnyObject>, characters: &*text,
                    charactersIgnoringModifiers: &*text, isARepeat: false, keyCode: code
                ]
            };
            let event = event.expect("construct a keyboard event");
            unsafe {
                match kind {
                    NSEventType::KeyDown => {
                        let _: () = msg_send![view, keyDown: &*event];
                    }
                    NSEventType::KeyUp => {
                        let _: () = msg_send![view, keyUp: &*event];
                    }
                    NSEventType::FlagsChanged => {
                        let _: () = msg_send![view, flagsChanged: &*event];
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn turn() {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    }

    pub fn run() {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
            // SAFETY: a plain window, created and owned on the main thread.
            let host_window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0)),
                    NSWindowStyleMask::Titled,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { host_window.setReleasedWhenClosed(false) };
            let allocated = HostView::alloc(mtm).set_ivars(Cell::new([0; 3]));
            let host: Retained<HostView> = unsafe {
                msg_send![super(allocated), initWithFrame: host_window.contentView().unwrap().frame()]
            };
            host_window.setContentView(Some(&host));
            let capture = Arc::new(AtomicBool::new(false));
            let handler = Handler(capture.clone());
            let host_ptr: *mut c_void = Retained::as_ptr(&host).cast_mut().cast();
            let mut handle = Window::open_parented(
                &Parent(host_ptr),
                WindowOpenOptions::new().with_size(100.0, 100.0),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else {
                unreachable!()
            };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            const NAME: &str = "plugin-own-window host keys test";
            let own = OwnWindow::new(NAME);
            // SAFETY: both views are alive.
            unsafe { own.open(host_ptr, raw.ns_view, false, None) };
            for (mode, out) in [("own window", true), ("redocked", false)] {
                own.want(out);
                turn();
                assert_eq!(own.is_out(), out, "{mode}");
                for captured in [false, true] {
                    capture.store(captured, Ordering::Relaxed);
                    let before = host.ivars().get();
                    send_save(&view);
                    let expected = if captured {
                        before
                    } else {
                        [before[0] + 1, before[1] + 1, before[2] + 2]
                    };
                    assert_eq!(host.ivars().get(), expected, "{mode}, captured={captured}");
                }
            }
            own.close();
            handle.close();
            host_window.close();
            let key = NSString::from_str(&format!("NSWindow Frame {NAME}"));
            NSUserDefaults::standardUserDefaults().removeObjectForKey(&key);
            println!("host keys through the own window: own window and redocked passed");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
