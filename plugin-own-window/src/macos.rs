use std::cell::RefCell;
use std::ffi::{CStr, CString, c_void};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, OnceLock, Weak};

use dispatch2::MainThreadBound;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Ivar, Sel};
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowLevel,
    NSWindowOrderingMode, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSNumber, NSObject, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};

use crate::{Limits, Move, OwnSize};

/// The level Bitwig gives its plug-in windows, measured (CG layer 3), and
/// `kCGFloatingWindowLevel`: above the host's own windows.
const FLOATING: NSWindowLevel = 3;
/// `kCGNormalWindowLevel`.
const NORMAL: NSWindowLevel = 0;

/// The editor's own window, one per open editor: see the crate
/// documentation.
///
/// The plug-in's editor handle owns it, and drops or [closes](Self::close) it
/// as the host destroys the editor; everything else, the size source and the
/// frame among them, holds a [`Weak`], so a view that outlives its editor
/// keeps nothing alive. The handle the shell requires is sendable, but every
/// method reaches the AppKit state only on the main thread; a final drop off
/// it waits for the main thread to run the cleanup.
pub struct OwnWindow(MainThreadBound<Local>);

struct Local {
    /// The window's title, and the name AppKit keeps its frame under between
    /// opens, in the defaults of the process the plug-in runs in.
    name: Retained<NSString>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    /// The host's view, which the editor's view lives in while docked.
    host: Option<Retained<NSView>>,
    /// baseview's view, the one that moves.
    view: Option<Retained<NSView>>,
    /// Hears which app comes to the front, performs deferred calls, and
    /// answers the close button.
    agent: Option<Retained<AnyObject>>,
    /// Whom the close button tells; without it the window has none.
    closed: Option<Rc<dyn Fn() -> bool>>,
    /// Whom each move is told, before the view moves.
    moving: Option<Rc<dyn Fn(Move)>>,
    /// The sizes the shell allows, as last given: a window opened between
    /// two frames takes them, and opens at the editor's size within them,
    /// and the window does not exist when the size source gives them.
    limits: Option<Limits>,
    /// The editor's own window, from just before the view moves into it to
    /// just before it moves out.
    window: Option<Retained<NSWindow>>,
    /// The content size the editor last had from this window, in logical
    /// points: the view's as it moved in, reported to the size source since,
    /// or applied for the editor's own ask.
    /// Kept rather than read back from the view, which the editor may size
    /// otherwise (egui-baseview fits a size to the GPU's texture limit): a
    /// mismatch read every frame would report, and run a pass, every frame.
    size: (u32, u32),
    /// Docked since the size source last asked, which asks the host for the
    /// editor's size. A move out before it asked undoes it.
    redocked: bool,
    /// A move asked for and not yet performed: out if true.
    wanted: Option<bool>,
    /// An app that came forward and has not yet raised its windows, and
    /// how many more times to look ([`SETTLE_TRIES`]). Kept at zero once
    /// the looks run out with the window still raised over everything,
    /// so the next app to come forward tucks it instead.
    settling: Option<(i32, u8)>,
    /// Already told the host to destroy this editor. The callback may return
    /// before destruction; docking does not make it a new editor.
    closing: bool,
}

fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("the editor's own window is only reached from the main thread")
}

fn logical(size: NSSize) -> (u32, u32) {
    (
        size.width.round().max(1.0) as u32,
        size.height.round().max(1.0) as u32,
    )
}

fn ns_size((width, height): (u32, u32)) -> NSSize {
    NSSize::new(f64::from(width), f64::from(height))
}

fn as_object(window: &NSWindow) -> &AnyObject {
    window.as_ref()
}

/// The shell's limits, as the bounds of the player's resizing.
fn set_limits(window: &NSWindow, limits: Limits) {
    window.setContentMinSize(ns_size(limits.min));
    window.setContentMaxSize(ns_size(limits.max));
}

/// Content at `size`, keeping the window's top-left corner, as a host's
/// window grows, so the title bar stays where it was; returns the content
/// size applied. The frame is computed whole: AppKit's `setContentSize:`
/// kept the top-left too where measured (macOS 26 and 27), but nothing
/// documents which corner it keeps.
fn set_content_size(window: &NSWindow, size: NSSize) -> (u32, u32) {
    let frame = window.frame();
    let mut resized = window.frameRectForContentRect(NSRect::new(NSPoint::ZERO, size));
    resized.origin = NSPoint::new(
        frame.origin.x,
        frame.origin.y + frame.size.height - resized.size.height,
    );
    window.setFrame_display(resized, true);
    logical(window.contentRectForFrameRect(window.frame()).size)
}

fn top_left(window: &NSWindow) -> NSPoint {
    let frame = window.frame();
    NSPoint::new(frame.origin.x, frame.origin.y + frame.size.height)
}

/// Whether the frontmost app is the host: the app whose bundle this process
/// runs from. That is the DAW itself for a host that loads plug-ins
/// in-process, and still the DAW for Bitwig's sandboxed plug-in host, which
/// runs from inside `Bitwig Studio.app`; the editor's own process counts too,
/// since it is in front while its window is clicked.
///
/// `None` when it is the host, or else the frontmost app's process.
fn other_in_front() -> Option<i32> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    // SAFETY: `processIdentifier` is a `pid_t` property of every running
    // application, read by hand to spare the binding its `libc` feature.
    let pid: i32 = unsafe { msg_send![&*app, processIdentifier] };
    // This process by its pid, since a plug-in host need not be a bundle.
    if pid as u32 == std::process::id() {
        return None;
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let bundle = app.bundleURL().and_then(|url| url.path());
    if bundle.is_some_and(|bundle| exe.starts_with(PathBuf::from(bundle.to_string()))) {
        return None;
    }
    Some(pid)
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Returns a +1 `CFArrayRef` of `CFDictionaryRef`, toll-free bridged to
    /// their Foundation counterparts.
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *mut NSArray;
}

/// The window to put the editor's just below when `pid` comes to the front:
/// the last of the run of its windows at the front of the normal level (this
/// one's own excepted), so the app that came forward covers it and nothing
/// that was behind it does. `None` while that app's windows are not at the
/// front yet.
///
/// Only the owner, layer, alpha and number of each window are read, which
/// need no screen-recording permission.
fn below_front_of(pid: i32, own: isize) -> Option<isize> {
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    // SAFETY: a Copy-rule CoreFoundation array, owned from here.
    let list = unsafe {
        Retained::from_raw(CGWindowListCopyWindowInfo(
            ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS,
            0,
        ))
    }?;
    let value = |info: &AnyObject, key: &str| -> Option<Retained<NSNumber>> {
        let info = info.downcast_ref::<NSDictionary>()?;
        info.objectForKey(&*NSString::from_str(key))?
            .downcast::<NSNumber>()
            .ok()
    };
    let number = |info: &AnyObject, key: &str| value(info, key).map(|n| n.as_isize());
    let mut front_run = None;
    for info in list.iter() {
        if number(&info, "kCGWindowLayer") != Some(NORMAL) {
            continue;
        }
        // An invisible window (an overlay, or the host's concealed one) is
        // in front of nothing.
        if value(&info, "kCGWindowAlpha").is_some_and(|alpha| alpha.as_f64() == 0.0) {
            continue;
        }
        let Some(window) = number(&info, "kCGWindowNumber") else {
            continue;
        };
        if window == own {
            continue;
        }
        if number(&info, "kCGWindowOwnerPID") != Some(pid as isize) {
            break;
        }
        front_run = Some(window);
    }
    front_run
}

/// Put the window just behind the windows `pid` brought forward, if they are
/// at the front yet. Returns whether they were.
fn tuck(window: &NSWindow, pid: i32) -> bool {
    let Some(other) = below_front_of(pid, window.windowNumber()) else {
        return false;
    };
    window.orderWindow_relativeTo(NSWindowOrderingMode::Below, other);
    true
}

/// How often, and how many times, to look again for the windows of an app
/// that came forward. The activation is announced before that app has raised
/// them, at least when it was clicked in the Dock (seen in Bitwig,
/// harmonigraph#1580): looked for at once, they are still wherever they were,
/// and the editor tucked behind them would land behind whatever covered them.
const SETTLE_DELAY: f64 = 0.05;
const SETTLE_TRIES: u8 = 10;

/// Keep the host's window out of sight and out of the way, but open: closing
/// or hiding it is the host's to undo, and Bitwig does.
fn conceal(window: &NSWindow) {
    if window.alphaValue() != 0.0 {
        window.setAlphaValue(0.0);
    }
    if !window.ignoresMouseEvents() {
        window.setIgnoresMouseEvents(true);
    }
}

fn reveal(window: &NSWindow) {
    window.setIgnoresMouseEvents(false);
    window.setAlphaValue(1.0);
}

/// The view behind an AppKit pointer, retained.
///
/// # Safety
///
/// `view` is null or an `NSView` alive for the call.
unsafe fn retain_view(view: *mut c_void) -> Option<Retained<NSView>> {
    // SAFETY: as the caller promises; retaining it keeps it alive after.
    unsafe { Retained::retain(view.cast::<NSView>()) }
}

impl OwnWindow {
    /// An editor's own window, not yet open. `name` is its title, and the
    /// name AppKit remembers its frame under: a new name starts from where
    /// the host's window is.
    pub fn new(name: &str) -> Arc<Self> {
        let local = Local {
            name: NSString::from_str(name),
            state: RefCell::default(),
        };
        Arc::new(Self(MainThreadBound::new(local, main_thread())))
    }

    fn local(&self) -> &Local {
        self.0.get(main_thread())
    }

    fn state(&self) -> &RefCell<State> {
        &self.local().state
    }

    /// Learn the editor's views once it has opened in the host's, and move it
    /// into its own window if it is to be `out`. Called once, outside any
    /// frame (see the crate documentation). `closed` tells the host the user
    /// closed the window, and returns whether it could; it may destroy the
    /// editor during the call, and is let go at [`Self::close`]. With `out`,
    /// `moving` is called inside this call, so it is not made under a lock
    /// `moving` takes.
    ///
    /// # Safety
    ///
    /// `host` and `view` are each null or an `NSView` alive for the call: the
    /// host's view the editor opened in, and the editor's own, inside it.
    pub unsafe fn open(
        self: &Arc<Self>,
        host: *mut c_void,
        view: *mut c_void,
        out: bool,
        limits: Limits,
        closed: Option<Box<dyn Fn() -> bool>>,
        moving: Box<dyn Fn(Move)>,
    ) {
        let mtm = main_thread();
        // SAFETY: as the caller promises.
        let (Some(host), Some(view)) = (unsafe { retain_view(host) }, unsafe { retain_view(view) })
        else {
            return;
        };
        let agent = Agent::answering(Arc::downgrade(self));
        // SAFETY: `activated:` takes the notification; the agent is removed
        // in `close`, before it can be freed.
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .addObserver_selector_name_object(
                    &agent,
                    sel!(activated:),
                    Some(NSWorkspaceDidActivateApplicationNotification),
                    None,
                );
        }
        *self.state().borrow_mut() = State {
            host: Some(host),
            view: Some(view),
            agent: Some(agent),
            closed: closed.map(Rc::from),
            moving: Some(Rc::from(moving)),
            limits: Some(limits),
            ..State::default()
        };
        if out {
            self.pop(mtm);
        }
    }

    /// Whether the editor can have a window of its own here: it opened in an
    /// AppKit view.
    pub fn offered(&self) -> bool {
        self.state().borrow().agent.is_some()
    }

    /// Whether the view is in its own window: true from just before it moves
    /// in to just before it moves out, so a host `set_size` arriving while it
    /// is there, which sizes the host's now empty window, can be told apart.
    pub fn is_out(&self) -> bool {
        self.state().borrow().window.is_some()
    }

    /// Ask for the editor in its own window or in the host's, performed on
    /// the run loop's next turn if it is not there already: asked from a
    /// frame, which is no place to move it (see the crate documentation).
    pub fn want(&self, out: bool) {
        let (agent, withdrawn) = {
            let mut state = self.state().borrow_mut();
            if state.window.is_some() == out {
                // There already, which calls off a move the other way that
                // has not happened yet.
                if state.wanted.take().is_none() {
                    return;
                }
                (state.agent.clone(), true)
            } else {
                if state.wanted.replace(out).is_some() {
                    return;
                }
                (state.agent.clone(), false)
            }
        };
        let Some(agent) = agent else { return };
        // At most one `apply:` is ever queued: one left behind by a withdrawn
        // move would perform a move asked for after it, ahead of whatever was
        // queued in between, a close click among them.
        if withdrawn {
            Agent::cancel(&agent, sel!(apply:), None);
        } else {
            Agent::perform(&agent, sel!(apply:), None, 0.0);
        }
    }

    /// The size source's question, asked before the host's, with the sizes
    /// the shell allows now: what the view's size owes to where it is.
    ///
    /// Also where the host's window is kept concealed while the editor is out
    /// of it, should the host have put it back, or have shown it only after
    /// the editor opened, and where the own window's limits follow the
    /// shell's.
    pub fn take_size(&self, limits: Limits) -> Option<OwnSize> {
        let (window, host, changed) = {
            let mut state = self.state().borrow_mut();
            let changed = state.limits.replace(limits) != Some(limits);
            let Some(window) = state.window.clone() else {
                return std::mem::take(&mut state.redocked).then_some(OwnSize::Docked);
            };
            (window, state.host.clone(), changed)
        };
        if let Some(host_window) = host.and_then(|host| host.window()) {
            conceal(&host_window);
        }
        if changed {
            set_limits(&window, limits);
        }
        let size = logical(window.contentView()?.frame().size);
        let mut state = self.state().borrow_mut();
        (size != state.size).then(|| {
            state.size = size;
            OwnSize::Resized(size)
        })
    }

    /// A size the editor asked for itself (a pane folding sideways, a
    /// restore), given to its own window instead of to the host, from the
    /// window's top-left corner, within the limits as last given: AppKit
    /// holds a frame set in code to none. Returns the content size applied,
    /// which the editor takes, or `None` while the view is in the host's
    /// window.
    pub fn resize(&self, size: (u32, u32)) -> Option<(u32, u32)> {
        let (window, limits) = {
            let state = self.state().borrow();
            (state.window.clone()?, state.limits)
        };
        let size = limits.map_or(size, |limits| limits.clamp(size));
        let applied = set_content_size(&window, ns_size(size));
        self.state().borrow_mut().size = applied;
        Some(applied)
    }

    /// The editor closing, called before baseview closes the view, which
    /// takes it out of whichever window it is in: show the host's window
    /// again, should the host reuse it, close the editor's own, call off
    /// everything pending, and let go of everything. Repeated calls are
    /// harmless, and dropping the last reference does the same. With the
    /// view out, both call `moving` inside them, and `close` can itself run
    /// inside `closed`, so neither is made under a lock `moving` takes.
    pub fn close(&self) {
        self.local().close();
    }

    /// The move [`Self::want`] deferred, off the run loop. None once the
    /// host has been told the window closed: the editor is on its way out,
    /// and a dock would ask the host for its size as it goes.
    fn apply(&self) {
        let mtm = main_thread();
        let wanted = {
            let mut state = self.state().borrow_mut();
            let wanted = state.wanted.take();
            wanted.filter(|_| !state.closing)
        };
        match wanted {
            Some(true) => self.pop(mtm),
            Some(false) => self.dock(),
            None => {}
        }
    }

    fn pop(&self, mtm: MainThreadMarker) {
        let (host, view, agent, closable, limits, moving) = {
            let state = self.state().borrow();
            let (None, Some(host), Some(view), Some(agent)) =
                (&state.window, &state.host, &state.view, &state.agent)
            else {
                return;
            };
            (
                host.clone(),
                view.clone(),
                agent.clone(),
                state.closed.is_some(),
                state.limits,
                state.moving.clone(),
            )
        };

        let content = view.frame().size;
        let mut style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        if closable {
            style |= NSWindowStyleMask::Closable;
        }
        // SAFETY: a plain window, created and owned on the main thread.
        let own = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, content),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: `State` owns the window, so AppKit must not release it on
        // close as well.
        unsafe { own.setReleasedWhenClosed(false) };
        // Already NO for a plain NSWindow; said anyway, since it is the whole
        // difference from the host's window.
        own.setHidesOnDeactivate(false);
        // Left to AppKit's default, a window that floats joins the Spaces it
        // is carried through (measured in otonal: in both the desktop's and a
        // fullscreen game's, shown over the game as it came forward).
        own.setCollectionBehavior(NSWindowCollectionBehavior::Managed);
        // The close button asks the agent, which refuses it and tells the
        // host. The window keeps the delegate weakly; `State` owns both.
        // SAFETY: the agent answers `windowShouldClose:`, the one delegate
        // method it implements; AppKit asks before sending any other.
        let () = unsafe { msg_send![&*own, setDelegate: &*agent] };
        let name = &self.local().name;
        own.setTitle(name);
        own.setLevel(if other_in_front().is_some() {
            NORMAL
        } else {
            FLOATING
        });

        // Where it was last time; the first time, where the host's window is.
        if !own.setFrameUsingName(name) {
            match host.window() {
                Some(host_window) => own.setFrameTopLeftPoint(top_left(&host_window)),
                None => own.center(),
            }
        }
        // Refused for a second open editor with the same name in the same
        // process, which then just goes unremembered.
        own.setFrameAutosaveName(name);
        // At the editor's size, within the limits, which also bound the
        // player's resizing from here, keeping the remembered top-left: the
        // frame remembers where the window was, and the project how large
        // its editor is (decided by Yan, egui-plugin-window#12). Set after
        // the naming, which restores the remembered frame again (measured)
        // and would undo a size set before it. Nothing keeps the larger
        // window on screen beyond what AppKit does as it orders it in.
        let wanted = match limits {
            Some(limits) => {
                set_limits(&own, limits);
                limits.clamp(logical(content))
            }
            None => logical(content),
        };
        let size = set_content_size(&own, ns_size(wanted));

        let Some(own_content) = own.contentView() else {
            return;
        };
        // The shell first, with no borrow held: from here the editor's size
        // is this window's, which a save must find at once rather than at the
        // next frame, since none runs while the editor cannot be seen.
        if let Some(moving) = moving {
            moving(Move::Out(size));
        }
        // Out from here, before the view moves: attaching it can draw a frame
        // on the spot, whose size source must find it out, and a host
        // `set_size` arriving from now on is not the editor's. The view's
        // size is what the editor has; one the limits changed is reported at
        // the next frame. A dock no frame has seen yet is undone, and owes the
        // host nothing.
        {
            let mut state = self.state().borrow_mut();
            state.window = Some(own.clone());
            state.size = logical(view.frame().size);
            state.redocked = false;
        }
        // The move. Made without a state borrow, as every native call here
        // is: the frame drawn on the spot borrows the state.
        view.removeFromSuperview();
        own_content.addSubview(&view);
        view.setFrameOrigin(NSPoint::ZERO);
        if let Some(host_window) = host.window() {
            conceal(&host_window);
        }
        own.makeKeyAndOrderFront(None);
    }

    /// Back into the host's window, shown again, and the editor's own closed.
    /// The view keeps its size; the size source asks the host for the same
    /// ([`OwnSize::Docked`]).
    fn dock(&self) {
        let (window, host, view, moving) = {
            let mut state = self.state().borrow_mut();
            let Some(window) = state.window.take() else {
                return;
            };
            // Before the move, which can draw a frame on the spot: that
            // frame's size source must already find it docked.
            state.redocked = true;
            state.settling = None;
            (
                window,
                state.host.clone(),
                state.view.clone(),
                state.moving.clone(),
            )
        };
        if let Some(moving) = moving {
            moving(Move::Back);
        }
        // SAFETY: clearing the delegate this window was given.
        let () = unsafe { msg_send![&*window, setDelegate: None::<&AnyObject>] };
        if let (Some(host), Some(view)) = (host, view) {
            view.removeFromSuperview();
            host.addSubview(&view);
            view.setFrameOrigin(NSPoint::ZERO);
            if let Some(host_window) = host.window() {
                reveal(&host_window);
            }
        }
        window.close();
    }

    /// The close button, deferred from the window's asking: the host is told
    /// once for this editor, even when destruction comes on a later turn. No
    /// state borrow is held across the call, which may run [`Self::close`].
    fn ask_host_close(&self, clicked: Option<&AnyObject>) {
        let (closed, window) = {
            let mut state = self.state().borrow_mut();
            if state.closing {
                return;
            }
            // A click belongs to the window it was made in, which must still
            // be the editor's: docked since, or in a window opened since, the
            // editor stays open.
            let (Some(closed), Some(window)) = (state.closed.clone(), state.window.clone()) else {
                return;
            };
            if !clicked.is_some_and(|clicked| std::ptr::eq(clicked, as_object(&window))) {
                return;
            }
            state.closing = true;
            (closed, window)
        };
        if closed() {
            return;
        }
        // The host could not be told: minimizing is reversible, so a later
        // click may try again. A delivered close stays latched.
        self.state().borrow_mut().closing = false;
        window.miniaturize(None);
    }

    /// Another app came to the front, or the host did: float above the
    /// host's windows while the host is. Leaving the host, sit among the
    /// normal windows, just behind the app that came forward and above
    /// everything else; and from one other app to the next, stay put, as any
    /// window would, under whatever had already covered it.
    fn front_changed(&self) {
        let (window, agent, host, unplaced) = {
            let mut state = self.state().borrow_mut();
            let unplaced = state.settling.take().is_some();
            (
                state.window.clone(),
                state.agent.clone(),
                state.host.clone(),
                unplaced,
            )
        };
        let Some(window) = window else { return };
        let Some(pid) = other_in_front() else {
            // Bitwig puts its window back in front as it comes forward:
            // hidden again now rather than at the next frame.
            if let Some(host_window) = host.and_then(|host| host.window()) {
                conceal(&host_window);
            }
            window.setLevel(FLOATING);
            return;
        };
        // Minimized: nothing to stack, which ordering it would undo.
        if !window.isVisible() {
            window.setLevel(NORMAL);
            return;
        }
        // Still raised from leaving the host, and never tucked: placed behind
        // this app instead, as it would have been behind that one.
        if window.level() != FLOATING && !unplaced {
            return;
        }
        window.setLevel(NORMAL);
        if tuck(&window, pid) {
            return;
        }
        // Above everything it covered, until that app's windows arrive in
        // front of it.
        window.orderFront(None);
        self.state().borrow_mut().settling = Some((pid, SETTLE_TRIES));
        if let Some(agent) = agent {
            Agent::settle_later(&agent);
        }
    }

    /// Look again for the windows of the app that came forward.
    fn settle(&self) {
        let (window, agent, pid, tries) = {
            let mut state = self.state().borrow_mut();
            let (Some((pid, tries @ 1..)), Some(window)) = (state.settling, state.window.clone())
            else {
                return;
            };
            state.settling = None;
            (window, state.agent.clone(), pid, tries)
        };
        if !window.isVisible() {
            return;
        }
        if tuck(&window, pid) {
            return;
        }
        self.state().borrow_mut().settling = Some((pid, tries - 1));
        if tries == 1 {
            return;
        }
        if let Some(agent) = agent {
            Agent::settle_later(&agent);
        }
    }
}

impl Local {
    fn close(&self) {
        // Out of the borrow before native calls, which may reenter a size
        // source or frame. Those find the empty state.
        let state = std::mem::take(&mut *self.state.borrow_mut());
        if let (Some(_), Some(moving)) = (&state.window, &state.moving) {
            moving(Move::Back);
        }
        if let Some(agent) = &state.agent {
            // Every deferred move, close and look: none is owed to an editor
            // that has closed.
            Agent::cancel_all(agent);
            // SAFETY: removing an observer this added, still alive.
            unsafe {
                NSWorkspace::sharedWorkspace()
                    .notificationCenter()
                    .removeObserver(agent)
            };
        }
        if let Some(window) = &state.window {
            if let Some(host_window) = state.host.as_ref().and_then(|host| host.window()) {
                reveal(&host_window);
            }
            // SAFETY: clearing the delegate this window was given.
            let () = unsafe { msg_send![&**window, setDelegate: None::<&AnyObject>] };
            window.close();
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.close();
    }
}

/// The Objective-C object AppKit calls back: the workspace's activation
/// notifications, the run loop's deferred performs, and the window's
/// `windowShouldClose:`. It holds its [`OwnWindow`] weakly, so a call that
/// arrives after the editor has gone finds nothing to do.
///
/// Its class is built at run time under a name unique to this load of the
/// crate. Both plug-ins link this crate into one host process, where
/// Objective-C class names are shared: a class named at compile time, as
/// `define_class!` names one, would be registered by whichever plug-in loads
/// first and then used by the other with that one's code and instance
/// layout, which differ between builds.
struct Agent;

struct AgentClass {
    class: &'static AnyClass,
    /// The `Weak<OwnWindow>` the agent answers for, as a raw pointer.
    own: &'static Ivar,
}

const OWN: &CStr = c"ownWindow";

impl Agent {
    fn class() -> &'static AgentClass {
        static CLASS: OnceLock<AgentClass> = OnceLock::new();
        CLASS.get_or_init(|| {
            let unique = NSProcessInfo::processInfo().globallyUniqueString();
            let name = CString::new(format!("PluginOwnWindowAgent_{unique}"))
                .expect("a process-unique string holds no NUL");
            let mut builder = ClassBuilder::new(&name, NSObject::class())
                .expect("a process-unique class name is free");
            builder.add_ivar::<*const c_void>(OWN);
            // SAFETY: each function's signature is the selector's: the
            // receiver, the selector, then one object argument, returning
            // nothing or, for `windowShouldClose:`, a BOOL.
            unsafe {
                builder.add_method(sel!(activated:), activated as extern "C-unwind" fn(_, _, _));
                builder.add_method(sel!(apply:), apply as extern "C-unwind" fn(_, _, _));
                builder.add_method(
                    sel!(hostClose:),
                    host_close as extern "C-unwind" fn(_, _, _),
                );
                builder.add_method(sel!(settle:), settle as extern "C-unwind" fn(_, _, _));
                builder.add_method(
                    sel!(windowShouldClose:),
                    window_should_close as extern "C-unwind" fn(_, _, _) -> _,
                );
                builder.add_method(sel!(dealloc), dealloc as extern "C-unwind" fn(_, _));
            }
            let class = builder.register();
            let own = class.instance_variable(OWN).expect("the ivar just added");
            AgentClass { class, own }
        })
    }

    fn answering(own: Weak<OwnWindow>) -> Retained<AnyObject> {
        let class = Self::class();
        // SAFETY: NSObject's `new` on a class derived from it.
        let agent: Retained<AnyObject> = unsafe { msg_send![class.class, new] };
        // SAFETY: the ivar is this class's, of this type; `dealloc` takes the
        // reference back.
        unsafe { *class.own.load_ptr::<*const c_void>(&agent) = Weak::into_raw(own).cast() };
        agent
    }

    /// The agent's window, while anyone else still holds it.
    fn own(agent: &AnyObject) -> Option<Arc<OwnWindow>> {
        // SAFETY: an agent's ivar, set by `Self::answering` from `Weak::into_raw`,
        // alive until `dealloc`; borrowed here without taking it.
        let raw = unsafe { *Self::class().own.load::<*const c_void>(agent) };
        if raw.is_null() {
            return None;
        }
        let own = ManuallyDrop::new(unsafe { Weak::from_raw(raw.cast::<OwnWindow>()) });
        own.upgrade()
    }

    /// `selector` with `object` on the run loop's turn `delay` seconds from
    /// now. The run loop retains the agent and the object until then, so the
    /// host destroying the editor cannot free either mid-call.
    fn perform(agent: &AnyObject, selector: Sel, object: Option<&AnyObject>, delay: f64) {
        // SAFETY: the agent's own method, taking one object.
        let () = unsafe {
            msg_send![
                agent,
                performSelector: selector,
                withObject: object,
                afterDelay: delay
            ]
        };
    }

    /// Call off this agent's pending `selector` with `object`.
    fn cancel(agent: &AnyObject, selector: Sel, object: Option<&AnyObject>) {
        // SAFETY: cancelling this object's own pending performs.
        let () = unsafe {
            msg_send![
                NSObject::class(),
                cancelPreviousPerformRequestsWithTarget: agent,
                selector: selector,
                object: object
            ]
        };
    }

    /// Call off every pending perform of this agent.
    fn cancel_all(agent: &AnyObject) {
        // SAFETY: cancelling this object's own pending performs.
        let () =
            unsafe { msg_send![NSObject::class(), cancelPreviousPerformRequestsWithTarget: agent] };
    }

    /// One chain of looks at a time: a switch mid-chain starts afresh.
    fn settle_later(agent: &AnyObject) {
        Self::cancel(agent, sel!(settle:), None);
        Self::perform(agent, sel!(settle:), None, SETTLE_DELAY);
    }
}

/// Another app came to the front, or the host did.
extern "C-unwind" fn activated(this: &AnyObject, _: Sel, _: Option<&AnyObject>) {
    if let Some(own) = Agent::own(this) {
        own.front_changed();
    }
}

/// A move [`OwnWindow::want`] deferred to here.
extern "C-unwind" fn apply(this: &AnyObject, _: Sel, _: Option<&AnyObject>) {
    if let Some(own) = Agent::own(this) {
        own.apply();
    }
}

/// A click [`window_should_close`] deferred to here, with the window clicked.
extern "C-unwind" fn host_close(this: &AnyObject, _: Sel, clicked: Option<&AnyObject>) {
    if let Some(own) = Agent::own(this) {
        own.ask_host_close(clicked);
    }
}

extern "C-unwind" fn settle(this: &AnyObject, _: Sel, _: Option<&AnyObject>) {
    if let Some(own) = Agent::own(this) {
        own.settle();
    }
}

/// The close button: refused, and the host told instead on the run loop's
/// next turn, once however often it is clicked before then. The window
/// clicked goes with it, for the telling to check it is still the editor's.
extern "C-unwind" fn window_should_close(
    this: &AnyObject,
    _: Sel,
    window: Option<&AnyObject>,
) -> Bool {
    Agent::cancel(this, sel!(hostClose:), window);
    Agent::perform(this, sel!(hostClose:), window, 0.0);
    Bool::NO
}

extern "C-unwind" fn dealloc(this: &mut AnyObject, _: Sel) {
    let class = Agent::class();
    // SAFETY: the ivar `Agent::answering` set, taken back once, as the object goes.
    unsafe {
        let raw = std::mem::replace(
            &mut *class.own.load_ptr::<*const c_void>(this),
            std::ptr::null(),
        );
        if !raw.is_null() {
            drop(Weak::from_raw(raw.cast::<OwnWindow>()));
        }
        let () = msg_send![super(this, NSObject::class()), dealloc];
    }
}
