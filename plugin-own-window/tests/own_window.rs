//! The own window's moves, sizes and close button, on real AppKit windows
//! and plain views, with the deferred calls run by turning the run loop.
//! AppKit needs the process's main thread, which the Rust test harness does
//! not use.
#[cfg(target_os = "macos")]
mod macos {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;

    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::AnyObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{
        NSDate, NSObject, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSUserDefaults,
    };
    use plugin_own_window::{Limits, Move, OwnSize, OwnWindow};

    /// Not either plug-in's, so their remembered frames stay as they are,
    /// and this process's alone: every checkout's test binary at one commit
    /// has the same name, and so the same defaults domain, and two runs at
    /// once would read and forget each other's frame.
    fn name() -> String {
        format!("plugin-own-window test {}", std::process::id())
    }
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
        /// The moves the shell was told of, in order.
        moves: Rc<RefCell<Vec<Move>>>,
    }

    /// Forget the frame AppKit remembers under the test's name.
    fn forget() {
        let key = NSString::from_str(&format!("NSWindow Frame {}", name()));
        NSUserDefaults::standardUserDefaults().removeObjectForKey(&key);
    }

    /// An editor's view of `size`, opened in a host's window and handed to
    /// its own window, out of it at once if `out`. Whatever frame is
    /// remembered under the test's name stays.
    fn opened(mtm: MainThreadMarker, out: bool, size: (f64, f64), limits: Limits) -> Fixture {
        // SAFETY: a plain window, created and owned on the main thread.
        let host_window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(size.0, size.1)),
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { host_window.setReleasedWhenClosed(false) };
        let host = host_window.contentView().unwrap();
        let view = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::ZERO, NSSize::new(size.0, size.1)),
        );
        host.addSubview(&view);
        let own = OwnWindow::new(&name());
        let closes = Rc::new(Cell::new(0));
        let counting = closes.clone();
        let moves = Rc::new(RefCell::new(Vec::new()));
        let told = moves.clone();
        // SAFETY: both views are alive.
        unsafe {
            own.open(
                Retained::as_ptr(&host).cast_mut().cast(),
                Retained::as_ptr(&view).cast_mut().cast(),
                out,
                limits,
                Some(Box::new(move || {
                    counting.set(counting.get() + 1);
                    true
                })),
                Box::new(move |moved| told.borrow_mut().push(moved)),
            );
        }
        Fixture {
            host_window,
            host,
            view,
            own,
            closes,
            moves,
        }
    }

    /// An editor's view, 300 by 200, with nothing remembered: the own
    /// window opens at the view's size.
    fn fixture(mtm: MainThreadMarker, out: bool) -> Fixture {
        forget();
        opened(mtm, out, (300.0, 200.0), LIMITS)
    }

    /// Run what was deferred to the run loop's next turns.
    fn turn() {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    }

    /// Turn the run loop until `done`, for at most two seconds: a loaded
    /// machine can take longer than one turn's worth to run a move.
    fn turn_until(done: impl Fn() -> bool) -> bool {
        for _ in 0..40 {
            if done() {
                return true;
            }
            turn();
        }
        done()
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

    fn content(window: &NSWindow) -> NSSize {
        window.contentView().unwrap().frame().size
    }

    /// Where AppKit restores the remembered frame's top-left corner now, on
    /// a probe window. Not the corner saved: with several displays AppKit may
    /// restore a frame onto another display than the one it was on, by the
    /// desktop's state at that moment (seen on a Mac with three).
    fn restored(mtm: MainThreadMarker) -> (f64, f64) {
        // SAFETY: a plain window, created and owned on the main thread.
        let probe = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0)),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { probe.setReleasedWhenClosed(false) };
        assert!(
            probe.setFrameUsingName(&NSString::from_str(&name())),
            "nothing remembered"
        );
        let corner = top_left(&probe);
        probe.close();
        corner
    }

    /// Forgets the frame when dropped, on a failing run's unwind as well.
    struct Forget;

    impl Drop for Forget {
        fn drop(&mut self) {
            autoreleasepool(|_| {
                turn();
                forget();
            });
        }
    }

    fn close(fixture: Fixture) {
        fixture.own.close();
        fixture.host_window.close();
    }

    /// (a) Undock, then dock: the shell is told of each move, the view goes
    /// out and back, and the host's view gets it back at the size it had in
    /// its own window.
    fn undock_then_dock(mtm: MainThreadMarker) {
        let f = fixture(mtm, false);
        assert!(f.own.offered());
        f.own.want(true);
        assert!(!f.own.is_out(), "a move is deferred to the run loop");
        turn();
        assert!(f.own.is_out());
        assert_eq!(*f.moves.borrow(), [Move::Out((300, 200))]);
        let own_window = f.view.window().unwrap();
        assert!(!std::ptr::eq(&*own_window, &*f.host_window));
        assert_eq!(
            f.host_window.alphaValue(),
            0.0,
            "the host's window is concealed"
        );
        assert_eq!(
            f.own.take_size(LIMITS),
            None,
            "the window is the view's size"
        );

        // The player resizes the window; the shell gives the view its size.
        own_window.setContentSize(NSSize::new(400.0, 250.0));
        assert_eq!(f.own.take_size(LIMITS), Some(OwnSize::Resized((400, 250))));
        f.view.setFrameSize(NSSize::new(400.0, 250.0));

        f.own.want(false);
        turn();
        assert!(!f.own.is_out());
        assert_eq!(*f.moves.borrow(), [Move::Out((300, 200)), Move::Back]);
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

    /// (b) A dock asked for from a frame, then a close click in the same
    /// turn: the click is queued behind the dock, which leaves its window
    /// gone, and the docked editor stays open. A click alone tells the host
    /// once, however often it is repeated.
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
        println!("own window: a click tells the host once, and a dock ahead of it drops it");
    }

    define_class!(
        /// A frame of the plug-in's, asking for the own window from a turn
        /// of the run loop queued among the crate's own.
        #[unsafe(super = NSObject)]
        #[thread_kind = MainThreadOnly]
        #[ivars = Arc<OwnWindow>]
        struct Asker;

        impl Asker {
            #[unsafe(method(wantOut:))]
            fn want_out(&self, _: Option<&AnyObject>) {
                self.ivars().want(true);
            }
        }
    );

    /// (f) A click, then a dock and an undock again within the turn: the
    /// click is told only about the window it was made in. A dock is asked
    /// for, withdrawn and asked for again, with an undock asked for from a
    /// frame queued between: a withdrawn move left queued would dock, then
    /// open a replacement window ahead of the click, whose telling would
    /// close the replacement's editor.
    fn click_dock_undock(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let clicked = f.view.window().unwrap();
        let asker: Retained<Asker> = {
            let allocated = Asker::alloc(mtm).set_ivars(f.own.clone());
            unsafe { msg_send![super(allocated), init] }
        };
        f.own.want(false);
        f.own.want(true);
        // SAFETY: the asker's own method; the run loop retains it until then.
        let () = unsafe {
            msg_send![
                &*asker,
                performSelector: sel!(wantOut:),
                withObject: None::<&AnyObject>,
                afterDelay: 0.0f64
            ]
        };
        f.own.want(false);
        clicked.performClose(None);
        turn();
        let now = f.view.window().unwrap();
        assert!(
            f.closes.get() == 0 || std::ptr::eq(&*now, &*clicked),
            "a replacement window's editor was closed"
        );
        assert!(
            std::ptr::eq(&*now, &*clicked),
            "a withdrawn move was performed"
        );
        assert_eq!(
            f.closes.get(),
            1,
            "the click on the editor's window was dropped"
        );
        close(f);
        println!("own window: a click is told only about the window clicked");
    }

    /// (g) A close the agent hears from a window that is no longer the
    /// editor's, as a click deferred across a dock and an undock would be:
    /// dropped.
    fn stale_click(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let first = f.view.window().unwrap();
        f.own.want(false);
        assert!(
            turn_until(|| !f.own.is_out()),
            "no dock: {:?}",
            f.moves.borrow()
        );
        f.own.want(true);
        assert!(
            turn_until(|| f.own.is_out()),
            "no undock: {:?}",
            f.moves.borrow()
        );
        let second = f.view.window().unwrap();
        assert!(
            !std::ptr::eq(&*first, &*second),
            "the same window after a dock and an undock: {:?}",
            f.moves.borrow()
        );
        // SAFETY: the window's delegate, the crate's agent, answers it.
        let agent: Option<Retained<AnyObject>> = unsafe { msg_send![&*second, delegate] };
        let agent = agent.unwrap();
        let _: bool = unsafe { msg_send![&*agent, windowShouldClose: &*first] };
        turn();
        assert_eq!(
            f.closes.get(),
            0,
            "a click on a replaced window closed the editor"
        );
        second.performClose(None);
        turn();
        assert_eq!(f.closes.get(), 1);
        close(f);
        println!("own window: a click from a replaced window is dropped");
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
            moves,
        } = fixture(mtm, true);
        let own_window = view.window().unwrap();
        own.want(false);
        own_window.performClose(None);
        let weak = Arc::downgrade(&own);
        drop(own);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            moves.borrow().last(),
            Some(&Move::Back),
            "the close was not told"
        );
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
    /// is returned as applied, and is not reported back.
    fn resize_keeps_top_left(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        assert_eq!(f.own.take_size(LIMITS), None);
        let before = top_left(&own_window);
        assert_eq!(f.own.resize((500, 320)), Some((500, 320)));
        assert_eq!(top_left(&own_window), before);
        assert_eq!(content(&own_window), NSSize::new(500.0, 320.0));
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

    /// (h) Reopened over a remembered frame of another size: the window
    /// opens where it was, at the editor's size, which the shell is told at
    /// the move, and the next frame has nothing to report. With limits the
    /// editor's size is outside, at the size within them, which the view
    /// takes at the next frame.
    fn reopen_remembered(mtm: MainThreadMarker) {
        // Each open in a pool of its own, so its window is gone, and the
        // autosave name free, before the next.
        autoreleasepool(|_| {
            let f = fixture(mtm, true);
            let own_window = f.view.window().unwrap();
            assert_eq!(f.own.resize((500, 320)), Some((500, 320)));
            // Moved, so the remembered corner is not where a first open puts it.
            own_window.setFrameTopLeftPoint(NSPoint::new(160.0, 600.0));
            close(f);
        });
        turn();

        autoreleasepool(|_| {
            let remembered = restored(mtm);
            let f = opened(mtm, true, (350.0, 250.0), LIMITS);
            let own_window = f.view.window().unwrap();
            assert_eq!(top_left(&own_window), remembered, "not where it was");
            assert_eq!(
                content(&own_window),
                NSSize::new(350.0, 250.0),
                "not the editor's size"
            );
            assert_eq!(*f.moves.borrow(), [Move::Out((350, 250))]);
            assert_eq!(f.own.take_size(LIMITS), None);
            close(f);
        });
        turn();

        autoreleasepool(|_| {
            let limits = Limits {
                min: (600, 400),
                max: (2000, 2000),
            };
            let remembered = restored(mtm);
            let f = opened(mtm, true, (350.0, 250.0), limits);
            let own_window = f.view.window().unwrap();
            assert_eq!(top_left(&own_window), remembered, "not where it was");
            assert_eq!(own_window.contentMinSize(), NSSize::new(600.0, 400.0));
            assert_eq!(
                content(&own_window),
                NSSize::new(600.0, 400.0),
                "outside the limits"
            );
            assert_eq!(*f.moves.borrow(), [Move::Out((600, 400))]);
            assert_eq!(f.own.take_size(limits), Some(OwnSize::Resized((600, 400))));
            assert_eq!(f.own.take_size(limits), None);
            close(f);
        });
        println!("own window: it reopens where it was, at the editor's size");
    }

    pub fn run() {
        // Dropped last, once every window is gone: a window saves its frame
        // again as it goes, so forgetting earlier would leave one behind.
        let _forget = Forget;
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("native test runs on the main thread");
            let _app = NSApplication::sharedApplication(mtm);
            // Each in a pool of its own, so its windows, and their hold on
            // the autosave name, are gone before the next.
            let tests: [fn(MainThreadMarker); 8] = [
                undock_then_dock,
                close_click,
                click_dock_undock,
                stale_click,
                drop_with_calls_pending,
                resize_keeps_top_left,
                limits_follow,
                reopen_remembered,
            ];
            for test in tests {
                autoreleasepool(|_| test(mtm));
            }
        });
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}
