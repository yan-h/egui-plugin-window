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
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowCollectionBehavior,
        NSWindowStyleMask,
    };
    use objc2_foundation::{
        NSDate, NSObject, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSUserDefaults,
    };
    use plugin_own_window::{EditorSize, Limits, Opened};

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
        editor: Arc<EditorSize>,
        opened: Opened,
        /// How often the host was told the window closed.
        closes: Rc<Cell<u32>>,
        /// The sizes the host was asked for, in order.
        asked: RefCell<Vec<(u32, u32)>>,
    }

    impl Fixture {
        /// A frame's size source: the size the frame is laid out at, if it
        /// changed. The host takes every size it is asked for, and is told
        /// the size it is asked for while it is asked.
        fn frame(&self) -> Option<(u32, u32)> {
            self.editor.next_frame(self.opened, |asked| {
                assert_eq!(self.editor.reported_size(), asked);
                self.asked.borrow_mut().push(asked);
                true
            })
        }

        fn asked(&self) -> Vec<(u32, u32)> {
            self.asked.borrow().clone()
        }
    }

    /// Forget the frame AppKit remembers under the test's name.
    fn forget() {
        let key = NSString::from_str(&format!("NSWindow Frame {}", name()));
        NSUserDefaults::standardUserDefaults().removeObjectForKey(&key);
    }

    /// An editor of `size`, its view opened in a host's window and attached,
    /// out of it at once if `out`, with `limits` set since the size was
    /// accepted. Whatever frame is remembered under the test's name stays.
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
        let editor = EditorSize::new(&name(), (size.0 as u32, size.1 as u32), LIMITS);
        editor.set_limits(limits);
        let closes = Rc::new(Cell::new(0));
        let counting = closes.clone();
        let opened = editor.open();
        // SAFETY: both views are alive, the one inside the other.
        unsafe {
            editor.attach(
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
            editor,
            opened,
            closes,
            asked: RefCell::default(),
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
        fixture.editor.close();
        fixture.host_window.close();
    }

    /// (a) Undock, then dock: the view goes out and back, the host's view
    /// gets it back at the size it had in its own window, and the next frame
    /// asks the host for that size, once. While out, each frame keeps the
    /// host's window concealed, should the host have shown it again.
    fn undock_then_dock(mtm: MainThreadMarker) {
        let f = fixture(mtm, false);
        assert_eq!(f.frame(), Some((300, 200)));
        assert!(f.editor.offered());
        f.editor.want(true);
        assert!(!f.editor.is_out(), "a move is deferred to the run loop");
        turn_until(|| f.editor.is_out());
        assert!(f.editor.is_out());
        assert_eq!(f.editor.size(), (300, 200));
        let own_window = f.view.window().unwrap();
        assert!(!std::ptr::eq(&*own_window, &*f.host_window));
        // As Bitwig's plug-in host asks of every window in its process.
        own_window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Managed
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        assert_eq!(
            own_window.collectionBehavior(),
            NSWindowCollectionBehavior::Managed | NSWindowCollectionBehavior::FullScreenNone,
            "the window stays in its one Space, and is never fullscreen"
        );
        assert_eq!(
            f.host_window.alphaValue(),
            0.0,
            "the host's window is concealed"
        );
        assert_eq!(f.frame(), None, "the window is the view's size");

        // The host shows its window again, as Bitwig does as it comes
        // forward: the next frame conceals it.
        f.host_window.setAlphaValue(1.0);
        f.host_window.setIgnoresMouseEvents(false);
        assert_eq!(f.frame(), None);
        assert_eq!(f.host_window.alphaValue(), 0.0, "shown while out");
        assert!(f.host_window.ignoresMouseEvents(), "clickable while out");

        // The player resizes the window; the frame gives the view its size.
        own_window.setContentSize(NSSize::new(400.0, 250.0));
        assert_eq!(f.frame(), Some((400, 250)));
        f.view.setFrameSize(NSSize::new(400.0, 250.0));
        assert!(f.asked().is_empty(), "the host was asked while out");

        f.editor.want(false);
        turn_until(|| !f.editor.is_out());
        assert!(!f.editor.is_out());
        assert!(same(superview(&f.view), &*f.host));
        assert_eq!(f.view.frame().size, NSSize::new(400.0, 250.0));
        assert_eq!(
            f.host_window.alphaValue(),
            1.0,
            "the host's window is shown again"
        );
        assert_eq!(f.frame(), None);
        assert_eq!(f.asked(), [(400, 250)], "docked, the host is asked once");
        assert_eq!(f.frame(), None);
        assert_eq!(f.asked(), [(400, 250)], "the host is asked again");
        assert_eq!(f.closes.get(), 0);
        close(f);
        println!("own window: undock then dock moves the view out and back");
    }

    /// (j) A dock no frame has seen yet, undone by a move out: the host is
    /// owed nothing.
    fn dock_undone(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        assert_eq!(f.frame(), Some((300, 200)));
        f.editor.want(false);
        assert!(turn_until(|| !f.editor.is_out()), "no dock");
        f.editor.want(true);
        assert!(turn_until(|| f.editor.is_out()), "no undock");
        assert_eq!(f.frame(), None);
        assert!(f.asked().is_empty(), "an undone dock asked the host");
        close(f);
        println!("own window: a dock undone before a frame owes the host nothing");
    }

    /// (b) A dock asked for from a frame, then a close click in the same
    /// turn: the click is queued behind the dock, which leaves its window
    /// gone, and the docked editor stays open. A click alone tells the host
    /// once, however often it is repeated, and no dock is made after it.
    fn close_click(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        assert!(f.editor.is_out(), "opened out of the host's window at once");
        let own_window = f.view.window().unwrap();
        f.editor.want(false);
        own_window.performClose(None);
        turn_until(|| !f.editor.is_out());
        // The click's telling, queued behind the dock.
        turn();
        assert!(!f.editor.is_out());
        assert_eq!(f.closes.get(), 0, "the docked editor was closed");

        f.editor.want(true);
        turn_until(|| f.editor.is_out());
        let own_window = f.view.window().unwrap();
        own_window.performClose(None);
        own_window.performClose(None);
        turn_until(|| f.closes.get() == 1);
        assert_eq!(f.closes.get(), 1);
        own_window.performClose(None);
        turn();
        assert_eq!(f.closes.get(), 1, "the host was told twice");
        assert!(
            f.editor.is_out(),
            "the close button closed the window itself"
        );
        // The host destroys the editor on a later turn; a dock asked for
        // meanwhile is not made.
        f.editor.want(false);
        turn();
        assert!(f.editor.is_out(), "docked after the host was told");
        close(f);
        println!(
            "own window: a click tells the host once, and a dock ahead of it drops it, not one after"
        );
    }

    define_class!(
        /// A frame of the plug-in's, asking for the own window from a turn
        /// of the run loop queued among the crate's own.
        #[unsafe(super = NSObject)]
        #[thread_kind = MainThreadOnly]
        #[ivars = Arc<EditorSize>]
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
            let allocated = Asker::alloc(mtm).set_ivars(f.editor.clone());
            unsafe { msg_send![super(allocated), init] }
        };
        f.editor.want(false);
        f.editor.want(true);
        // SAFETY: the asker's own method; the run loop retains it until then.
        let () = unsafe {
            msg_send![
                &*asker,
                performSelector: sel!(wantOut:),
                withObject: None::<&AnyObject>,
                afterDelay: 0.0f64
            ]
        };
        f.editor.want(false);
        clicked.performClose(None);
        turn_until(|| f.closes.get() > 0);
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
        f.editor.want(false);
        assert!(turn_until(|| !f.editor.is_out()), "no dock");
        f.editor.want(true);
        assert!(turn_until(|| f.editor.is_out()), "no undock");
        let second = f.view.window().unwrap();
        assert!(
            !std::ptr::eq(&*first, &*second),
            "the same window after a dock and an undock"
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
        turn_until(|| f.closes.get() == 1);
        assert_eq!(f.closes.get(), 1);
        close(f);
        println!("own window: a click from a replaced window is dropped");
    }

    /// (c) Dropping the editor without closing it, out with a dock and a
    /// close click pending, and docked with an undock pending: the own
    /// window closes as it goes, and nothing fires afterwards.
    fn drop_with_calls_pending(mtm: MainThreadMarker) {
        let Fixture {
            host_window,
            host,
            view,
            editor,
            closes,
            ..
        } = fixture(mtm, true);
        let own_window = view.window().unwrap();
        editor.want(false);
        own_window.performClose(None);
        let weak = Arc::downgrade(&editor);
        drop(editor);
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
            editor,
            ..
        } = fixture(mtm, false);
        editor.want(true);
        drop(editor);
        turn();
        assert!(
            same(view.window(), &*host_window),
            "an undock ran after the drop"
        );
        host_window.close();
        println!("own window: nothing fires after the drop");
    }

    /// (d) A size the editor asks for while out is the own window's to
    /// grant, without the host: it keeps the window's top-left corner, is
    /// taken as applied, and is not reported back.
    fn resize_keeps_top_left(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        assert_eq!(f.frame(), Some((300, 200)));
        let before = top_left(&own_window);
        f.editor.request_size((500, 320));
        assert_eq!(f.frame(), Some((500, 320)));
        assert_eq!(top_left(&own_window), before);
        assert_eq!(content(&own_window), NSSize::new(500.0, 320.0));
        assert_eq!(f.editor.size(), (500, 320));
        assert_eq!(f.frame(), None);
        assert!(f.asked().is_empty(), "the host was asked while out");
        close(f);
        println!("own window: a resize keeps the top-left corner");
    }

    /// (e) The own window's bounds follow the limits at the next frame, and
    /// a move out between frames opens it at the editor's size within the
    /// limits as they are then, which is the editor's size at once.
    fn limits_follow(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        f.frame();
        f.editor.set_limits(Limits {
            min: (200, 150),
            max: (800, 600),
        });
        f.frame();
        assert_eq!(own_window.contentMinSize(), NSSize::new(200.0, 150.0));
        assert_eq!(own_window.contentMaxSize(), NSSize::new(800.0, 600.0));
        f.editor.set_limits(Limits {
            min: (250, 180),
            max: (900, 700),
        });
        f.frame();
        assert_eq!(own_window.contentMinSize(), NSSize::new(250.0, 180.0));
        assert_eq!(own_window.contentMaxSize(), NSSize::new(900.0, 700.0));
        close(f);

        let f = fixture(mtm, false);
        assert_eq!(f.frame(), Some((300, 200)));
        let limits = Limits {
            min: (400, 300),
            max: (900, 700),
        };
        f.editor.set_limits(limits);
        f.editor.want(true);
        assert!(turn_until(|| f.editor.is_out()), "no undock");
        let own_window = f.view.window().unwrap();
        assert_eq!(own_window.contentMinSize(), NSSize::new(400.0, 300.0));
        assert_eq!(own_window.contentMaxSize(), NSSize::new(900.0, 700.0));
        assert_eq!(content(&own_window), NSSize::new(400.0, 300.0));
        assert_eq!(f.editor.size(), (400, 300), "the move out's size, at once");
        assert_eq!(f.frame(), Some((400, 300)));
        assert_eq!(f.frame(), None);
        close(f);
        println!("own window: later limits replace earlier ones");
    }

    /// (k) A window whose size durably differs from the editor's gives the
    /// frame nothing after the once: limits moved past a window left as it
    /// was, a view the editor sized otherwise (egui-baseview fits one to the
    /// GPU's texture limit), and a window sized past its limits in code. A
    /// size every frame would run a pass every frame.
    fn no_size_every_frame(mtm: MainThreadMarker) {
        let f = fixture(mtm, true);
        let own_window = f.view.window().unwrap();
        assert_eq!(f.frame(), Some((300, 200)));
        let limits = Limits {
            min: (320, 240),
            max: (2000, 2000),
        };
        f.editor.set_limits(limits);
        for _ in 0..3 {
            assert_eq!(f.frame(), None, "the limits moved past the window");
        }
        assert_eq!(f.editor.size(), (300, 200), "the limits sized the editor");

        f.view.setFrameSize(NSSize::new(200.0, 150.0));
        for _ in 0..3 {
            assert_eq!(f.frame(), None, "the view sized otherwise");
        }
        assert_eq!(f.editor.size(), (300, 200), "the view sized the editor");

        own_window.setContentSize(NSSize::new(2400.0, 100.0));
        assert_eq!(f.frame(), Some((2000, 240)));
        for _ in 0..3 {
            assert_eq!(f.frame(), None, "the window past its limits");
        }
        assert_eq!(f.editor.size(), (2000, 240));
        close(f);
        println!("own window: a lasting difference gives the frame one size");
    }

    /// (h) Reopened over a remembered frame of another size: the window
    /// opens where it was, at the editor's size, which the editor keeps.
    /// With limits the editor's size is outside, at the size within them,
    /// which is the editor's at once.
    fn reopen_remembered(mtm: MainThreadMarker) {
        // Each open in a pool of its own, so its window is gone, and the
        // autosave name free, before the next.
        autoreleasepool(|_| {
            let f = fixture(mtm, true);
            let own_window = f.view.window().unwrap();
            f.frame();
            f.editor.request_size((500, 320));
            assert_eq!(f.frame(), Some((500, 320)));
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
            assert_eq!(f.editor.size(), (350, 250));
            assert_eq!(f.frame(), Some((350, 250)));
            assert_eq!(f.frame(), None);
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
            assert_eq!(f.editor.size(), (600, 400));
            assert_eq!(f.frame(), Some((600, 400)));
            assert_eq!(f.frame(), None);
            close(f);
        });
        println!("own window: it reopens where it was, at the editor's size");
    }

    /// (i) The shared size sequence on a real window. Opened out with an
    /// editor size below the limits, the own window's size is the editor's at
    /// once; while out, the host's `set_size` changes nothing, outside the
    /// limits too, and the editor's own ask is the window's to grant, without
    /// the host; the player's resize reaches the next frame; docked again,
    /// the host is asked for the editor's size.
    fn editor_size(mtm: MainThreadMarker) {
        forget();
        let limits = Limits {
            min: (320, 240),
            max: (2000, 2000),
        };
        let f = opened(mtm, true, (300.0, 200.0), limits);
        let editor = &f.editor;
        assert!(editor.offered() && editor.is_out());
        assert_eq!(editor.size(), (320, 240), "the move out's size, at once");
        assert_eq!(f.frame(), Some((320, 240)));

        assert!(editor.host_set_size((900, 900), || panic!("asked while open")));
        assert!(editor.host_set_size((9000, 1), || panic!("asked while open")));
        assert_eq!(editor.size(), (320, 240), "the host sized the editor");
        editor.request_size((500, 320));
        assert_eq!(f.frame(), Some((500, 320)));
        let own_window = f.view.window().unwrap();
        assert_eq!(content(&own_window), NSSize::new(500.0, 320.0));

        own_window.setContentSize(NSSize::new(640.0, 400.0));
        assert_eq!(f.frame(), Some((640, 400)));
        assert_eq!(editor.size(), (640, 400));
        assert!(f.asked().is_empty(), "the host was asked while out");

        editor.want(false);
        assert!(turn_until(|| !editor.is_out()), "no dock");
        assert_eq!(f.frame(), None);
        assert_eq!(
            f.asked(),
            [(640, 400)],
            "the host was not asked for the size"
        );
        assert!(editor.host_set_size((700, 450), || panic!("asked while open")));
        assert_eq!(f.frame(), Some((700, 450)));

        editor.close();
        assert!(!editor.is_open() && !editor.offered());
        assert!(same(f.view.window(), &*f.host_window));
        f.host_window.close();
        println!("own window: the shared size sequence");
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
            let tests: [fn(MainThreadMarker); 11] = [
                undock_then_dock,
                dock_undone,
                close_click,
                click_dock_undock,
                stale_click,
                drop_with_calls_pending,
                resize_keeps_top_left,
                limits_follow,
                no_size_every_frame,
                reopen_remembered,
                editor_size,
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
