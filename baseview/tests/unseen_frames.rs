//! AppKit needs the process's main thread, which the Rust test harness does not use.
//!
//! The display link is never let run: each frame is one `displayLinkFired:`
//! sent by hand, so the test needs no display and waits on nothing.
//!
//! Only the unseen half can be checked this way. A window reads as visible
//! only once it is on screen and the window server has said so, which comes
//! later, through the run loop, and not reliably on a CI runner, so a frame
//! running for a view that can be seen is left to the plug-ins.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions};
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::{msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowOcclusionState,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use objc2_quartz_core::CADisplayLink;
    use raw_window_handle::{AppKitWindowHandle, HasRawWindowHandle, RawWindowHandle};

    struct Parent(*mut c_void);

    unsafe impl HasRawWindowHandle for Parent {
        fn raw_window_handle(&self) -> RawWindowHandle {
            let mut handle = AppKitWindowHandle::empty();
            handle.ns_view = self.0;
            RawWindowHandle::AppKit(handle)
        }
    }

    struct Handler {
        frames: Arc<AtomicUsize>,
    }

    impl WindowHandler for Handler {
        fn on_frame(&mut self, _: &mut Window) {
            self.frames.fetch_add(1, Ordering::Relaxed);
        }

        fn on_event(&mut self, _: &mut Window, _: Event) -> EventStatus {
            EventStatus::Ignored
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
            assert!(
                !window.occlusionState().contains(NSWindowOcclusionState::Visible),
                "a window never ordered in is not visible",
            );
            let host = window.contentView().unwrap();
            let frames = Arc::new(AtomicUsize::new(0));
            let handler = Handler { frames: frames.clone() };
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            // A link of the test's own, never added to a run loop: only an
            // argument for the callback.
            let link: Retained<CADisplayLink> = unsafe {
                msg_send![&*view, displayLinkWithTarget: &*view, selector: sel!(displayLinkFired:)]
            };
            let fire = || {
                let before = frames.load(Ordering::Relaxed);
                let _: () = unsafe { msg_send![&*view, displayLinkFired: &*link] };
                frames.load(Ordering::Relaxed) > before
            };

            assert!(!fire(), "a view in a window that is not on screen runs no frame");
            view.removeFromSuperview();
            assert!(!fire(), "nor does one in no window");

            link.invalidate();
            handle.close();
            window.close();
            println!("unseen frames: a window off screen and no window passed");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
