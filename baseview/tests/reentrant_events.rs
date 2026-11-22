//! AppKit needs the process's main thread, which the Rust test harness does not use.
//!
//! An event delivered while the handler is already handling one, as AppKit
//! can from inside a call the handler makes, is queued rather than borrowed
//! again, which would panic: it reaches the handler once the outer event
//! ends, in the order the events arrived and ahead of the next one, and its
//! key goes to the host, since its answer is not known in time.
#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions};
    use keyboard_types::{Key, KeyState};
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
        #[ivars = RefCell<Vec<(bool, String)>>]
        struct HostView;

        impl HostView {
            #[unsafe(method(keyDown:))]
            fn key_down(&self, event: &NSEvent) {
                self.ivars().borrow_mut().push((true, characters(event)));
            }

            #[unsafe(method(keyUp:))]
            fn key_up(&self, event: &NSEvent) {
                self.ivars().borrow_mut().push((false, characters(event)));
            }
        }
    );

    fn characters(event: &NSEvent) -> String {
        event.characters().map(|text| text.to_string()).unwrap_or_default()
    }

    struct Parent(*mut c_void);

    unsafe impl HasRawWindowHandle for Parent {
        fn raw_window_handle(&self) -> RawWindowHandle {
            let mut handle = AppKitWindowHandle::empty();
            handle.ns_view = self.0;
            RawWindowHandle::AppKit(handle)
        }
    }

    /// What the handler received, in order: (pressed, key).
    type Received = Arc<Mutex<Vec<(bool, String)>>>;

    /// Keys to send the view from inside the next event.
    type Inject = Arc<Mutex<Option<&'static [(bool, &'static str)]>>>;

    struct Handler {
        received: Received,
        inject: Inject,
    }

    impl WindowHandler for Handler {
        fn on_frame(&mut self, _: &mut Window) {}

        fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
            let Event::Keyboard(key) = event else { return EventStatus::Ignored };
            let Key::Character(written) = key.key else { return EventStatus::Ignored };
            self.received.lock().unwrap().push((key.state == KeyState::Down, written));
            let inject = self.inject.lock().unwrap().take();
            if let Some(keys) = inject {
                let RawWindowHandle::AppKit(raw) = window.raw_window_handle() else {
                    unreachable!()
                };
                let view = unsafe { &*raw.ns_view.cast::<NSView>() };
                for &(pressed, written) in keys {
                    send_key(view, pressed, written);
                }
            }
            EventStatus::Captured
        }
    }

    fn send_key(view: &NSView, pressed: bool, written: &str) {
        let kind = if pressed { NSEventType::KeyDown } else { NSEventType::KeyUp };
        let text = NSString::from_str(written);
        // The constructor's NSGraphicsContext argument is always nil.
        let event: Option<Retained<NSEvent>> = unsafe {
            msg_send![
                NSEvent::class(), keyEventWithType: kind,
                location: NSPoint::ZERO, modifierFlags: NSEventModifierFlags::empty(),
                timestamp: 0.0_f64, windowNumber: view.window().unwrap().windowNumber(),
                context: None::<&AnyObject>, characters: &*text,
                charactersIgnoringModifiers: &*text, isARepeat: false, keyCode: 0_u16
            ]
        };
        let event = event.expect("construct a keyboard event");
        unsafe {
            if pressed {
                let _: () = msg_send![view, keyDown: &*event];
            } else {
                let _: () = msg_send![view, keyUp: &*event];
            }
        }
    }

    pub fn run() {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
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
            let allocated = HostView::alloc(mtm).set_ivars(RefCell::default());
            let host: Retained<HostView> = unsafe {
                msg_send![super(allocated), initWithFrame: window.contentView().unwrap().frame()]
            };
            window.setContentView(Some(&host));
            let received = Received::default();
            let inject = Inject::default();
            let handler = Handler { received: received.clone(), inject: inject.clone() };
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };

            *inject.lock().unwrap() = Some(&[(true, "a"), (false, "a")]);
            send_key(&view, true, "s");
            send_key(&view, false, "s");
            let keys = |list: &[(bool, &str)]| {
                list.iter().map(|&(pressed, key)| (pressed, key.to_owned())).collect::<Vec<_>>()
            };
            assert_eq!(
                *received.lock().unwrap(),
                keys(&[(true, "s"), (true, "a"), (false, "a"), (false, "s")]),
                "queued events reach the handler in order, ahead of the next one",
            );
            assert_eq!(
                *host.ivars().borrow(),
                keys(&[(true, "a"), (false, "a")]),
                "a queued key goes to the host; a handled one the handler captured does not",
            );

            handle.close();
            window.close();
            println!("re-entrant events: queued in order, keys to the host");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
