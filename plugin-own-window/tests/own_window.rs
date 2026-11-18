//! The own window's moves, sizes and close button, on real AppKit windows
//! and plain views, with the deferred calls run by turning the run loop.
//! AppKit needs the process's main thread, which the Rust test harness does
//! not use.
#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;

    use objc2::rc::{Retained, autoreleasepool};
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSUserDefaults};
    use plugin_own_window::{Limits, OwnSize, OwnWindow};

    /// Not either plug-in's, so their remembered frames stay as they are.
    const NAME: &str = "plugin-own-window test";
    const LIMITS: Limits = Limits {
        min: (100, 100),
        max: (2000, 2000),
    };

    struct Fixture {
        host_window: Retained<NSWindow>,
        host: Retained<NSView>,
        view: Retained<NSView>,
        own: Arc<OwnWindow>,
        /// How often the host was told the window closed.
        closes: Rc<Cell<u32>>,
    }

    /// Forget the frame AppKit remembers under the test's name.
    fn forget() {
        let key = NSString::from_str(&format!("NSWindow Frame {NAME}"));
        NSUserDefaults::standardUserDefaults().removeObjectForKey(&key);
    }

    /// An editor's view, 300 by 200, opened in a host's window and handed to
    /// its own window, out of it at once if `out`. The own window opens at
    /// the view's size, as nothing is remembered.
    fn fixture(mtm: MainThreadMarker, out: bool) -> Fixture {
        forget();
        // SAFETY: a plain window, created and owned on the main thread.
        let host_window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(300.0, 200.0)),
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { host_window.setReleasedWhenClosed(false) };
        let host = host_window.contentView().unwrap();
        let view = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::ZERO, NSSize::new(300.0, 200.0)),
        );
        host.addSubview(&view);
        let own = OwnWindow::new(NAME);
        let closes = Rc::new(Cell::new(0));
        let counting = closes.clone();
        // SAFETY: both views are alive.
        unsafe {
            own.open(
                Retained::as_ptr(&host).cast_mut().cast(),
                Retained::as_ptr(&view).cast_mut().cast(),
                out,
                Some(Box::new(move || {
                    counting.set(counting.get() + 1);
                    true
                })),
            );
        }
        Fixture {
            host_window,
            host,
            view,
            own,
            closes,
        }
    }

    /// Run what was deferred to the run loop's next turns.
    fn turn() {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    }

    fn same<T>(a: Option<Retained<T>>, b: &T) -> bool {
        a.is_some_and(|a| std::ptr::eq(&*a, b))
    }

    fn superview(view: &NSView) -> Option<Retained<NSView>> {
        // SAFETY: read on the main thread, where views are changed.
        unsafe { view.superview() }
    }

    fn top_left(window: &NSWindow) -> (f64, f64) {
        let frame = window.frame();
        (frame.origin.x, frame.origin.y + frame.size.height)
    }

    fn close(fixture: Fixture) {
        fixture.own.close();
        fixture.host_window.close();
    }

    /// (a) Undock, then dock: the view goes out and back, and the host's
    /// view gets it back at the size it had in its own window.
    fn undock_then_dock(mtm: MainThreadMarker) {
        let f = fixture(mtm, false);
        assert!(f.own.offered());
        f.own.want(true);
        assert!(!f.own.is_out(), "a move is deferred to the run loop");
        turn();
        assert!(f.own.is_out());
        let own_window = f.view.window().unwrap();
        assert!(!std::ptr::eq(&*own_window, &*f.host_window));
        assert_eq!(
            f.host_window.alphaValue(),
            0.0,
            "the host's window is concealed"
        );
        assert_eq!(f.own.take_size(LIMITS), Some(OwnSize::Resized((300, 200))));
        assert_eq!(f.own.take_size(LIMITS), None);

        // The player resizes the window; the shell gives the view its size.
        own_window.setContentSize(NSSize::new(400.0, 250.0));
        assert_eq!(f.own.take_size(LIMITS), Some(OwnSize::Resized((400, 250))));
        f.view.setFrameSize(NSSize::new(400.0, 250.0));

        f.own.want(false);
        turn();
        assert!(!f.own.is_out());
        assert!(same(superview(&f.view), &*f.host));
        assert_eq!(f.view.frame().size, NSSize::new(400.0, 250.0));
        assert_eq!(
            f.host_window.alphaValue(),
            1.0,
            "the host's window is shown again"
        );
        assert_eq!(f.own.take_size(LIMITS), Some(OwnSize::Docked));
        assert_eq!(f.own.take_size(LIMITS), None);
        assert_eq!(f.closes.get(), 0);
        close(f);
        println!("own window: undock then dock moves the view out and back");
    }

    /// (b) A close click, then a dock in the same turn: the dock, asked from
    /// a frame before the click, is performed first and calls the close off.
    /// A click alone tells the host once, however often it is repeated.
    fn close_click(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        assert!(f.own.is_out(), "opened out of the host's window at once");
        let own_window = f.view.window().unwrap();
        f.own.want(false);
        own_window.performClose(None);
        turn();
        assert!(!f.own.is_out());
        assert_eq!(f.closes.get(), 0, "the docked editor was closed");

        f.own.want(true);
        turn();
        let own_window = f.view.window().unwrap();
        own_window.performClose(None);
        own_window.performClose(None);
        turn();
        assert_eq!(f.closes.get(), 1);
        own_window.performClose(None);
        turn();
        assert_eq!(f.closes.get(), 1, "the host was told twice");
        assert!(f.own.is_out(), "the close button closed the window itself");
        close(f);
        println!("own window: a close click tells the host once, and a dock calls it off");
    }

    /// (c) Dropping the object, out with a dock and a close click pending,
    /// and docked with an undock pending: nothing fires afterwards.
    fn drop_with_calls_pending(mtm: MainThreadMarker) {
        let Fixture {
            host_window,
            host,
            view,
            own,
            closes,
        } = fixture(mtm, true);
        let own_window = view.window().unwrap();
        own.want(false);
        own_window.performClose(None);
        let weak = Arc::downgrade(&own);
        drop(own);
        assert!(weak.upgrade().is_none());
        turn();
        assert_eq!(closes.get(), 0);
        assert!(!same(superview(&view), &*host), "a dock ran after the drop");
        assert_eq!(
            host_window.alphaValue(),
            1.0,
            "the host's window stayed concealed"
        );
        assert!(!own_window.isVisible());
        host_window.close();

        let Fixture {
            host_window,
            view,
            own,
            ..
        } = fixture(mtm, false);
        own.want(true);
        drop(own);
        turn();
        assert!(
            same(view.window(), &*host_window),
            "an undock ran after the drop"
        );
        host_window.close();
        println!("own window: nothing fires after the drop");
    }

    /// (d) A size the plug-in asks for keeps the window's top-left corner,
    /// and is not reported back to it.
    fn resize_keeps_top_left(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        assert_eq!(f.own.take_size(LIMITS), Some(OwnSize::Resized((300, 200))));
        let before = top_left(&own_window);
        assert!(f.own.resize((500, 320)));
        assert_eq!(top_left(&own_window), before);
        assert_eq!(
            own_window.contentView().unwrap().frame().size,
            NSSize::new(500.0, 320.0)
        );
        assert_eq!(f.own.take_size(LIMITS), None);
        close(f);
        println!("own window: a resize keeps the top-left corner");
    }

    /// (e) The limits a later frame gives replace the earlier ones.
    fn limits_follow(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        f.own.take_size(Limits {
            min: (200, 150),
            max: (800, 600),
        });
        assert_eq!(own_window.contentMinSize(), NSSize::new(200.0, 150.0));
        assert_eq!(own_window.contentMaxSize(), NSSize::new(800.0, 600.0));
        f.own.take_size(Limits {
            min: (250, 180),
            max: (900, 700),
        });
        assert_eq!(own_window.contentMinSize(), NSSize::new(250.0, 180.0));
        assert_eq!(own_window.contentMaxSize(), NSSize::new(900.0, 700.0));
        close(f);
        println!("own window: later limits replace earlier ones");
    }

    pub fn run() {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
            undock_then_dock(mtm);
            close_click(mtm);
            drop_with_calls_pending(mtm);
            resize_keeps_top_left(mtm);
            limits_follow(mtm);
            forget();
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
