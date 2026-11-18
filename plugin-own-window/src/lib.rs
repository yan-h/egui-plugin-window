//! A plug-in editor's own window: the editor's view moved out of the host's
//! window into one the plug-in makes, so the picture stays on screen when the
//! host is not the frontmost app, and back.
//!
//! Bitwig takes every plug-in window off screen while another app is in
//! front: the window it gives the editor sits at the floating level and is
//! ordered out on deactivation, while the plug-in host process stays
//! un-hidden. A window the plug-in creates itself is one Bitwig does not know
//! about, and it stays up (checked in Bitwig, harmonigraph#1580). So on macOS
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
//! The view's size is the plug-in's size source's to set, as for a host
//! resize: the size source asks [`OwnWindow::take_size`] first, at the top of
//! the frame it will be laid out in, and hands a size the editor asks for to
//! [`OwnWindow::resize`] before the host. Every call here is on the main
//! thread, where egui-baseview runs its size source and its frames and where
//! a CLAP host creates and destroys the editor; one from any other thread
//! panics.
//!
//! Off macOS nothing moves: [`OwnWindow::offered`] is false and every other
//! call does nothing.
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

/// The sizes the own window may be resized to by the player, in logical
/// points, as the plug-in's shell bounds the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub min: (u32, u32),
    pub max: (u32, u32),
}

/// What the size source owes the view this frame, from
/// [`OwnWindow::take_size`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnSize {
    /// The view is in its own window, which is this size: the editor takes
    /// it. Reported once after each move into the window and after each
    /// change, whoever made it.
    Resized((u32, u32)),
    /// The view is back in the host's window, which the plug-in asks for the
    /// editor's size, as for a resize the editor asks for itself.
    Docked,
}
