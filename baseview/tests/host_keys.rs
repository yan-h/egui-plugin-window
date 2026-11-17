//! AppKit needs the process's main thread, which the Rust test harness does not use.
#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions};
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{
        define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSView,
        NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
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

    struct Handler {
        capture: Arc<AtomicBool>,
        received: Arc<AtomicUsize>,
    }

    impl WindowHandler for Handler {
        fn on_frame(&mut self, _: &mut Window) {}

        fn on_event(&mut self, _: &mut Window, event: Event) -> EventStatus {
            if matches!(event, Event::Keyboard(_)) {
                self.received.fetch_add(1, Ordering::Relaxed);
            }
            if self.capture.load(Ordering::Relaxed) {
                EventStatus::Captured
            } else {
                EventStatus::Ignored
            }
        }
    }

    fn window(mtm: MainThreadMarker) -> Retained<NSWindow> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0)),
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window
    }

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

    pub fn run() {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
            let host_window = window(mtm);
            let allocated = HostView::alloc(mtm).set_ivars(Cell::new([0; 3]));
            let host: Retained<HostView> = unsafe {
                msg_send![super(allocated), initWithFrame: host_window.contentView().unwrap().frame()]
            };
            host_window.setContentView(Some(&host));
            let capture = Arc::new(AtomicBool::new(false));
            let received = Arc::new(AtomicUsize::new(0));
            let handler = Handler { capture: capture.clone(), received: received.clone() };
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            let own_window = window(mtm);
            for mode in ["docked", "own window", "redocked"] {
                if mode == "own window" {
                    view.removeFromSuperview();
                    own_window.contentView().unwrap().addSubview(&view);
                } else if mode == "redocked" {
                    view.removeFromSuperview();
                    host.addSubview(&view);
                }
                for captured in [false, true] {
                    capture.store(captured, Ordering::Relaxed);
                    let before = host.ivars().get();
                    let received_before = received.load(Ordering::Relaxed);
                    send_save(&view);
                    let expected = if captured {
                        before
                    } else {
                        [before[0] + 1, before[1] + 1, before[2] + 2]
                    };
                    assert_eq!(host.ivars().get(), expected, "{mode}, captured={captured}");
                    assert_eq!(received.load(Ordering::Relaxed), received_before + 4);
                }
            }
            handle.close();
            own_window.close();
            host_window.close();
            println!("host key forwarding: docked, own window and redocked passed");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
