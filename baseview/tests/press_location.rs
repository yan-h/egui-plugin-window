//! AppKit needs the process's main thread, which the Rust test harness does not use.
#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Arc, Mutex};
    use std::ffi::c_void;

    use baseview::{
        Event, EventStatus, MouseButton, MouseEvent, Point, Window, WindowHandler,
        WindowOpenOptions,
    };
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, ClassType, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSView,
        NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use raw_window_handle::{AppKitWindowHandle, HasRawWindowHandle, RawWindowHandle};

    struct Parent(*mut c_void);

    unsafe impl HasRawWindowHandle for Parent {
        fn raw_window_handle(&self) -> RawWindowHandle {
            let mut handle = AppKitWindowHandle::empty();
            handle.ns_view = self.0;
            RawWindowHandle::AppKit(handle)
        }
    }

    /// The pointer events the handler sees, as the position or button each carries.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Moved(Point),
        Pressed(MouseButton),
        Released(MouseButton),
    }

    struct Handler(Arc<Mutex<Vec<Seen>>>);

    impl WindowHandler for Handler {
        fn on_frame(&mut self, _: &mut Window) {}

        fn on_event(&mut self, _: &mut Window, event: Event) -> EventStatus {
            let seen = match event {
                Event::Mouse(MouseEvent::CursorMoved { position, .. }) => Seen::Moved(position),
                Event::Mouse(MouseEvent::ButtonPressed { button, .. }) => Seen::Pressed(button),
                Event::Mouse(MouseEvent::ButtonReleased { button, .. }) => Seen::Released(button),
                _ => return EventStatus::Ignored,
            };
            self.0.lock().unwrap().push(seen);
            EventStatus::Captured
        }
    }

    /// A mouse event at `location` in the window's coordinates, sent straight
    /// to the view as AppKit dispatches it.
    fn send_mouse(view: &NSView, kind: NSEventType, location: NSPoint) {
        // The constructor's NSGraphicsContext argument is always nil.
        let event: Option<Retained<NSEvent>> = unsafe {
            msg_send![
                NSEvent::class(), mouseEventWithType: kind,
                location: location, modifierFlags: NSEventModifierFlags::empty(),
                timestamp: 0.0_f64, windowNumber: view.window().unwrap().windowNumber(),
                context: None::<&AnyObject>, eventNumber: 0_isize, clickCount: 1_isize,
                pressure: 1.0_f32
            ]
        };
        let event = event.expect("construct a mouse event");
        unsafe {
            match kind {
                NSEventType::LeftMouseDown => {
                    let _: () = msg_send![view, mouseDown: &*event];
                }
                NSEventType::LeftMouseUp => {
                    let _: () = msg_send![view, mouseUp: &*event];
                }
                _ => unreachable!(),
            }
        }
    }

    pub fn run() {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
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
            let host = host_window.contentView().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let handler = Handler(seen.clone());
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };

            // No move before either: the press is the first the handler hears
            // of the pointer, and the release lands outside the view, where a
            // drag out of the window ends. The view is flipped, so a window
            // point (x, y) is (x, 100 - y) in it.
            send_mouse(&view, NSEventType::LeftMouseDown, NSPoint::new(30.0, 70.0));
            send_mouse(&view, NSEventType::LeftMouseUp, NSPoint::new(150.0, 20.0));
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    Seen::Moved(Point::new(30.0, 30.0)),
                    Seen::Pressed(MouseButton::Left),
                    Seen::Moved(Point::new(150.0, 80.0)),
                    Seen::Released(MouseButton::Left),
                ]
            );

            handle.close();
            host_window.close();
            println!("press location: a press and a release report where they happened");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
