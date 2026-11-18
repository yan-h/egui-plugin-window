//! AppKit needs the process's main thread, which the Rust test harness does not use.
//!
//! The unseen notice told once for each way a view stops being seen, and
//! never inside the window handler's borrow: the notice sends the view an
//! event through `trigger_event`, whose borrow is infallible and panics if
//! the handler is still borrowed, and a loss found inside a frame, inside an
//! event `trigger_event` delivers and inside one `trigger_deferrable_event`
//! delivers is each told as that borrow ends. A window reads as visible only
//! once the window server says so, which a test process may never see, so
//! the window's occlusion state is set through `baseview::testing` (the
//! `testing` feature); the view's own hiding, its ancestors' and its window
//! are AppKit's. Frames are `displayLinkFired:` sent by hand, as in
//! `unseen_frames.rs`.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    use std::sync::Arc;

    use baseview::{
        Event, EventStatus, UnseenNotice, Window, WindowEvent, WindowHandler, WindowOpenOptions,
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
        /// The last `Occluded` the handler was told: 0 none, 1 seen, 2 not.
        occluded: AtomicUsize,
        /// The handler is running: a frame or an event.
        handling: AtomicBool,
        /// A notice arrived while it was.
        told_while_handling: AtomicBool,
        /// The next frame hides its own view, as an app's frame might.
        hide_in_frame: AtomicBool,
        /// The next event hides its own view: one `trigger_event` delivers
        /// (a pointer entering) or one `trigger_deferrable_event` delivers
        /// (the view giving up the keyboard).
        hide_on_event: AtomicBool,
        /// The next frame hides its view and then closes its window.
        close_in_frame: AtomicBool,
        /// The view, for the notice to send an event to.
        view: AtomicPtr<NSView>,
    }

    fn hide(window: &mut Window) {
        let RawWindowHandle::AppKit(raw) = window.raw_window_handle() else { unreachable!() };
        unsafe { &*raw.ns_view.cast::<NSView>() }.setHidden(true);
    }

    struct Handler {
        shared: Arc<Shared>,
    }

    impl WindowHandler for Handler {
        fn on_frame(&mut self, window: &mut Window) {
            self.shared.handling.store(true, Ordering::Relaxed);
            self.shared.frames.fetch_add(1, Ordering::Relaxed);
            if self.shared.hide_in_frame.swap(false, Ordering::Relaxed) {
                hide(window);
            }
            if self.shared.close_in_frame.swap(false, Ordering::Relaxed) {
                hide(window);
                window.close();
            }
            self.shared.handling.store(false, Ordering::Relaxed);
        }

        fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
            if let Event::Window(WindowEvent::Occluded(occluded)) = event {
                self.shared.occluded.store(1 + usize::from(occluded), Ordering::Relaxed);
            }
            if self.shared.hide_on_event.swap(false, Ordering::Relaxed) {
                hide(window);
            }
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
                // What a host call might do: an event through `trigger_event`,
                // which panics if the handler is still borrowed.
                let view = noticed.view.load(Ordering::Relaxed);
                if let Some(view) = unsafe { view.as_ref() } {
                    let _: () = unsafe { msg_send![view, mouseEntered: view] };
                }
            });
            let mut handle = Window::open_parented(
                &Parent(Retained::as_ptr(&host).cast_mut().cast()),
                WindowOpenOptions::new().with_size(100.0, 100.0).with_unseen_notice(notice),
                move |_| handler,
            );
            let RawWindowHandle::AppKit(raw) = handle.raw_window_handle() else { unreachable!() };
            let view = unsafe { Retained::retain(raw.ns_view.cast::<NSView>()).unwrap() };
            shared.view.store(Retained::as_ptr(&view).cast_mut(), Ordering::Relaxed);
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

            let occluded = || match shared.occluded.load(Ordering::Relaxed) {
                0 => None,
                told => Some(told == 2),
            };
            let frames = || shared.frames.load(Ordering::Relaxed);
            assert_eq!(occluded(), Some(false), "opening seen tells the handler");

            view.setHidden(true);
            assert_eq!(told(), 1, "the view hidden");
            assert_eq!(occluded(), Some(true), "a hidden view's handler is not told");
            assert!(!fire(), "a hidden view runs no frame");
            view.setHidden(true);
            assert_eq!(told(), 1, "told again while it stays hidden");
            let before = frames();
            view.setHidden(false);
            assert_eq!(told(), 1, "a reveal is not told");
            assert_eq!(occluded(), Some(false), "an un-hidden view's handler is not told");
            assert_eq!(frames(), before + 1, "no reveal frame at un-hide");

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
            assert_eq!(occluded(), Some(false), "a gain the link found was not told the handler");
            baseview::testing::set_window_shows(Some(false));
            assert!(!fire(), "occluded, with no notification");
            assert_eq!(told(), 4, "the link callback missed a loss no notification reported");
            assert_eq!(occluded(), Some(true), "a loss the link found was not told the handler");

            baseview::testing::set_window_shows(Some(true));
            assert!(fire(), "seen again");
            view.removeFromSuperview();
            assert_eq!(told(), 5, "the view taken out of its window");
            assert_eq!(occluded(), Some(true), "a view out of its window was not told the handler");
            host.addSubview(&view);
            assert!(fire(), "seen once back in a window");
            assert_eq!(occluded(), Some(false), "a view back in a window was not told the handler");
            assert_eq!(told(), 5, "a move back is not told");

            shared.hide_in_frame.store(true, Ordering::Relaxed);
            assert!(fire(), "the frame that hides its view runs");
            assert_eq!(told(), 6, "a loss found inside a frame is told once the frame ends");
            assert!(
                !shared.told_while_handling.load(Ordering::Relaxed),
                "told inside the handler's borrow",
            );
            view.setHidden(false);

            // Inside an event `trigger_event` delivers: the pointer entering.
            shared.hide_on_event.store(true, Ordering::Relaxed);
            let _: () = unsafe { msg_send![&*view, mouseEntered: &*view] };
            assert_eq!(told(), 7, "a loss found inside trigger_event is told as it ends");
            view.setHidden(false);

            // Inside one `trigger_deferrable_event` delivers: the keyboard lost.
            shared.hide_on_event.store(true, Ordering::Relaxed);
            let _: bool = unsafe { msg_send![&*view, resignFirstResponder] };
            assert_eq!(told(), 8, "a loss found inside a deferrable event is told as it ends");
            view.setHidden(false);

            // A loss owed inside the frame that closes the window.
            shared.close_in_frame.store(true, Ordering::Relaxed);
            assert!(fire(), "the frame that closes its window runs");
            shared.view.store(std::ptr::null_mut(), Ordering::Relaxed);
            assert_eq!(told(), 8, "a loss owed inside the closing frame was told after the close");
            link.invalidate();
            handle.close();
            assert_eq!(told(), 8, "closing the window is not told");
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
