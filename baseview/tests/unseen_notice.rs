//! AppKit needs the process's main thread, which the Rust test harness does not use.
//!
//! The unseen notice told once for each way a view stops being seen, and
//! never inside the window handler's borrow. A window reads as visible only
//! once the window server says so, which a test process may never see, so
//! the window's occlusion state is set through `baseview::testing` (the
//! `testing` feature); the view's own hiding, its ancestors' and its window
//! are AppKit's. Frames are `displayLinkFired:` sent by hand, as in
//! `unseen_frames.rs`.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use baseview::{
        Event, EventStatus, UnseenNotice, Window, WindowHandler, WindowOpenOptions,
    };
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::{msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSView, NSWindow,
        NSWindowDidChangeOcclusionStateNotification, NSWindowStyleMask,
    };
    use objc2_foundation::{NSNotificationCenter, NSPoint, NSRect, NSSize};
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

    #[derive(Default)]
    struct Shared {
        told: AtomicUsize,
        frames: AtomicUsize,
        /// The handler is running: a frame or an event.
        handling: AtomicBool,
        /// A notice arrived while it was.
        told_while_handling: AtomicBool,
        /// The next frame hides its own view, as an app's frame might.
        hide_in_frame: AtomicBool,
    }

    struct Handler {
        shared: Arc<Shared>,
    }

    impl WindowHandler for Handler {
        fn on_frame(&mut self, window: &mut Window) {
            self.shared.handling.store(true, Ordering::Relaxed);
            self.shared.frames.fetch_add(1, Ordering::Relaxed);
            if self.shared.hide_in_frame.swap(false, Ordering::Relaxed) {
                let RawWindowHandle::AppKit(raw) = window.raw_window_handle() else {
                    unreachable!()
                };
                let view = unsafe { &*raw.ns_view.cast::<NSView>() };
                view.setHidden(true);
            }
            self.shared.handling.store(false, Ordering::Relaxed);
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
            let content = window.contentView().unwrap();
            // An ancestor of the editor's view between it and the window.
            let host = NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0)),
            );
            content.addSubview(&host);

            baseview::testing::set_window_shows(Some(true));
            let shared = Arc::new(Shared::default());
            let handler = Handler { shared: shared.clone() };
            let noticed = shared.clone();
            let notice = UnseenNotice::new(move || {
                noticed.told.fetch_add(1, Ordering::Relaxed);
                if noticed.handling.load(Ordering::Relaxed) {
                    noticed.told_while_handling.store(true, Ordering::Relaxed);
                }
            });
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0).with_unseen_notice(notice),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            let link: Retained<CADisplayLink> = unsafe {
                msg_send![&*view, displayLinkWithTarget: &*view, selector: sel!(displayLinkFired:)]
            };
            let fire = || {
                let before = shared.frames.load(Ordering::Relaxed);
                let _: () = unsafe { msg_send![&*view, displayLinkFired: &*link] };
                shared.frames.load(Ordering::Relaxed) > before
            };
            let told = || shared.told.load(Ordering::Relaxed);
            let occlusion_changed = || unsafe {
                NSNotificationCenter::defaultCenter().postNotificationName_object(
                    NSWindowDidChangeOcclusionStateNotification,
                    Some(&window),
                )
            };

            assert!(fire(), "a seen view runs frames");
            assert_eq!(told(), 0, "opening seen tells nothing");

            view.setHidden(true);
            assert_eq!(told(), 1, "the view hidden");
            assert!(!fire(), "a hidden view runs no frame");
            view.setHidden(true);
            assert_eq!(told(), 1, "told again while it stays hidden");
            view.setHidden(false);
            assert_eq!(told(), 1, "a reveal is not told");

            host.setHidden(true);
            assert_eq!(told(), 2, "an ancestor hidden");
            host.setHidden(false);

            baseview::testing::set_window_shows(Some(false));
            occlusion_changed();
            assert_eq!(told(), 3, "the window ordered out or covered");
            occlusion_changed();
            assert!(!fire(), "an occluded window runs no frame");
            assert_eq!(told(), 3, "told again while it stays occluded");

            // A notification lost: the next link callback finds the loss.
            baseview::testing::set_window_shows(Some(true));
            assert!(fire(), "seen again");
            baseview::testing::set_window_shows(Some(false));
            assert!(!fire(), "occluded, with no notification");
            assert_eq!(told(), 4, "the link callback missed a loss no notification reported");

            baseview::testing::set_window_shows(Some(true));
            assert!(fire(), "seen again");
            view.removeFromSuperview();
            assert_eq!(told(), 5, "the view taken out of its window");
            host.addSubview(&view);
            assert!(fire(), "seen once back in a window");
            assert_eq!(told(), 5, "a move back is not told");

            shared.hide_in_frame.store(true, Ordering::Relaxed);
            assert!(fire(), "the frame that hides its view runs");
            assert_eq!(told(), 6, "a loss found inside a frame is told once the frame ends");
            assert!(
                !shared.told_while_handling.load(Ordering::Relaxed),
                "told inside the handler's borrow",
            );
            view.setHidden(false);

            link.invalidate();
            handle.close();
            assert_eq!(told(), 6, "closing the window is not told");
            baseview::testing::set_window_shows(None);
            window.close();
            println!("unseen notice: each loss told once, outside the handler, none at close");
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
