//! A plug-in editor's own window: the editor's view moved out of the host's
//! window into one the plug-in makes, so the picture stays on screen when the
//! host is not the frontmost app, and back.
//!
//! Bitwig takes every plug-in window off screen while another app is in
//! front: the window it gives the editor sits at the floating level and is
//! ordered out on deactivation, while the plug-in host process stays
//! un-hidden. A window the plug-in creates itself is one Bitwig does not order
//! out, and it stays up (checked in Bitwig, harmonigraph#1580). Bitwig does
//! give it, as every window in its process, a collection behavior that joins
//! all Spaces, fullscreen games' among them; the window's class holds it in
//! the one Space it is in, and out of fullscreen. So on macOS
//! the editor lives in one unless the player docks it, and the host's window
//! is then left open but invisible and click-through. Hiding it outright
//! would be undone: Bitwig orders it back to the front whenever it is itself
//! brought forward.
//!
//! The live view moves rather than a second one opening, which keeps the
//! renderer and the egui context; baseview follows its view to a new window
//! (tracking area, first responder, focus, its display link and its
//! visibility check). A move is never made inside a frame: making a window
//! key sends baseview a focus event on the spot, and baseview's handler is
//! borrowed for the whole frame, so a move from a frame panics across AppKit
//! and takes the host process down. The one at open is made from
//! [`OwnWindow::open`], which the plug-in calls outside any frame; one the
//! player asks for from a frame is performed on the run loop's next turn
//! ([`OwnWindow::want`]).
//!
//! What the host's window did for the editor, this one does in its place. It
//! floats above the host's windows while the host is in front, as Bitwig's
//! did, and drops to the normal level while another app is, which is where
//! Bitwig's would have vanished.
//!
//! It opens where it was last, remembered by AppKit under the name the
//! plug-in gives it, at the editor's size within the shell's limits: the
//! project keeps the editor's size, and the window only its place. The first
//! time, it opens where the host's window is.
//!
//! A key the editor leaves unclaimed goes to the host's view from either
//! window: baseview sends it to the view it was opened in, held weakly,
//! rather than up the responder chain, which in this window ends at the
//! window itself. Nothing here sets a next responder.
//!
//! Its close button does not close it: the editor is the host's to destroy,
//! and the host's show/hide control would otherwise still think it open. The
//! button calls the plug-in's `closed` callback, which tells the host the
//! user closed it (CLAP's `clap_host_gui::closed(true)`), for the host to
//! destroy the editor as its own control does. Asking Bitwig to hide it
//! instead (`request_hide`) hid it, but left Bitwig's control showing it open
//! (harmonigraph#1609). The telling waits for the run loop's next turn, since
//! the host may destroy the editor during it, and the window must not go
//! while AppKit is still closing it. Without a callback the window has no
//! close button; with one that reports failure, the window is minimized.
//!
//! A click is told only while the window clicked is still the editor's when
//! the telling runs. So a click queued before a dock is honoured, the
//! player's last act having been the close: the host is told, and the dock
//! never happens. A click queued behind a pending dock is dropped: by the
//! time it runs, its window is gone, and the docked editor stays open. At
//! most one move is queued at a time, so no window opens again between a
//! click and its telling.
//!
//! The view's size is the plug-in's size source's to set, as for a host
//! resize: the size source asks [`OwnWindow::take_size`] first, at the top of
//! the frame it will be laid out in, and hands a size the editor asks for to
//! [`OwnWindow::resize`] before the host. Each move is told to the shell as
//! it happens ([`Move`]), before the view moves. Every call here is on the main
//! thread, where egui-baseview runs its size source and its frames and where
//! a CLAP host creates and destroys the editor; one from any other thread
//! panics.
//!
//! Off macOS nothing moves: [`OwnWindow::offered`] is false and every other
//! call does nothing.
//!
//! [`EditorSize`] runs that sequence for a shell: the editor's size, its
//! negotiation with the host, and the own window, so a shell supplies the
//! window's name, the limits, the saved choice of window, the close callback
//! and the host's `request_resize`, and calls [`OwnWindow`] through it.
//!
//! Where it came from, and what it took from each plug-in's copy, is in
//! `PATCHES.md` at the repository's root.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::OwnWindow;

#[cfg(not(target_os = "macos"))]
mod elsewhere;
#[cfg(not(target_os = "macos"))]
pub use elsewhere::OwnWindow;

mod size;
pub use size::{EditorSize, Opened};

/// The sizes the own window may be resized to by the player, in logical
/// points, as the plug-in's shell bounds the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub min: (u32, u32),
    pub max: (u32, u32),
}

impl Limits {
    /// `size` within the limits, on each axis. Where a maximum is below its
    /// minimum, the minimum wins, rather than `u32::clamp` panicking on the
    /// main thread.
    pub fn clamp(self, (width, height): (u32, u32)) -> (u32, u32) {
        (
            width.clamp(self.min.0, self.max.0.max(self.min.0)),
            height.clamp(self.min.1, self.max.1.max(self.min.1)),
        )
    }
}

/// What the size source owes the view this frame, from
/// [`OwnWindow::take_size`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnSize {
    /// The view is in its own window, which is this size: the editor takes
    /// it. Reported when the window's size differs from the one the editor
    /// last had from it: after the player resizes it, or after a move out
    /// whose limits the editor's size was outside. A size the editor asked for
    /// through [`OwnWindow::resize`] is returned there, not reported again.
    Resized((u32, u32)),
    /// The view is back in the host's window, which the plug-in asks for the
    /// editor's size, as for a resize the editor asks for itself.
    Docked,
}

/// A move of the editor's view, told to the shell as it happens
/// ([`OwnWindow::open`]'s `moving`), before the view moves, so that what
/// the shell keeps of the editor's size, a save's among it, is right at once
/// rather than at the next frame, which does not come while the editor
/// cannot be seen. It is called on the main thread inside the call that
/// moves the view: `open` with `out`, the run loop's deferred move,
/// `close`, and the last drop. A lock it takes must not be held across any
/// of those, or the main thread deadlocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// Into its own window, whose content is this size, in logical points:
    /// from now on the editor's size is the window's, and a host `set_size`
    /// sizes the host's empty window.
    Out((u32, u32)),
    /// Out of its own window: docked in the host's, or the editor closing.
    Back,
}
