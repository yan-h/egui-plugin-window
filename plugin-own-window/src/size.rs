//! The editor's size: what the host is told, what the project saves, and
//! where the own window has the say over it. Platform-free; off macOS the own
//! window is never out, so every size is the host's to negotiate.
//!
//! Each frame's size comes from one sequence, run in the plug-in's size
//! source before the frame's input and layout ([`EditorSize::next_frame`]):
//! the own window's size first, then a size the editor or a restore asked for
//! (granted by the own window while the view is in it, negotiated with the
//! host otherwise), then the host's own `set_size`, which is ignored while
//! the own window owns the size ([`EditorSize::host_set_size`]).
//!
//! One lock-free word publishes accepted size, pending proposal and whether
//! the editor is open together, so a restore may reach it from any thread,
//! with no lock, allocation or host call. The word holds 15 bits per
//! dimension, so limits stop at 32767 logical pixels. Counters only the main
//! thread touches (`lifetime`, `host_revision`) keep a stale round trip from
//! committing over a close and reopen or a newer host acceptance. A frame
//! takes its proposal out of the word before calling the host. A new request
//! stays pending even at the same size, while an accepted identical follow-up
//! coalesces without another host request, and a proposal of the current
//! accepted size supersedes an older request. Closing installs a queued
//! request not yet taken, for saving and the next open; one already taken or
//! refused is not revived. Only the proposal in flight is what the host is
//! told the editor's size is, and only while the host is being asked;
//! [`EditorSize::size`] and the saved state read the accepted size.
//!
//! Taken from otonal's shell (`crates/otonal-clap/src/editor/resize.rs` and
//! its `next_size`), where it was written and tested first.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::{Limits, Move, OwnSize, OwnWindow};

/// The largest side the size word holds, in logical pixels.
const MAX_SIDE: u32 = 32767;

const SIZE_MASK: u64 = (1 << 30) - 1;
const REQUEST_MASK: u64 = SIZE_MASK << 30;
const OPEN: u64 = 1 << 60;
const PENDING: u64 = 1 << 61;

fn encode_size(size: (u32, u32)) -> u64 {
    u64::from(size.0.min(MAX_SIDE)) | (u64::from(size.1.min(MAX_SIDE)) << 15)
}

fn decode_size(word: u64) -> (u32, u32) {
    ((word & 32767) as u32, ((word >> 15) & 32767) as u32)
}

impl Limits {
    /// Within what the size word holds: every side from 1 to 32767, each
    /// maximum at least its minimum. A plug-in's limits are already; this
    /// keeps others from panicking or wrapping rather than refusing them.
    fn held(self) -> Self {
        let side = |side: u32| side.clamp(1, MAX_SIDE);
        let min = (side(self.min.0), side(self.min.1));
        let max = (side(self.max.0).max(min.0), side(self.max.1).max(min.1));
        Self { min, max }
    }

    fn encode(self) -> u64 {
        let held = self.held();
        encode_size(held.min) | (encode_size(held.max) << 30)
    }

    fn decode(word: u64) -> Self {
        Self {
            min: decode_size(word),
            max: decode_size(word >> 30),
        }
    }
}

/// The host's negotiation: accepted size, proposal and lifetime.
#[derive(Debug, Default)]
struct NativeResize {
    state: AtomicU64,
    // Main-thread callbacks may close and reopen, or accept an intervening
    // host size, during `request_resize`. These counters protect that
    // synchronous round trip.
    lifetime: AtomicU64,
    host_revision: AtomicU64,
    in_flight: AtomicU64,
    applied: AtomicU64,
}

impl NativeResize {
    fn initialize(&self, size: (u32, u32)) {
        self.state.store(encode_size(size), Ordering::Release);
    }

    fn size(&self) -> (u32, u32) {
        decode_size(self.state.load(Ordering::Acquire))
    }

    fn reported_size(&self) -> (u32, u32) {
        let proposal = self.in_flight.load(Ordering::Acquire);
        if proposal != 0 {
            decode_size(proposal)
        } else {
            self.size()
        }
    }

    fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) & OPEN != 0
    }

    fn open(&self) -> u64 {
        let lifetime = self.lifetime.fetch_add(1, Ordering::AcqRel) + 1;
        // A closed restore has already installed its accepted dimensions.
        self.state.fetch_or(OPEN, Ordering::AcqRel);
        self.applied.store(0, Ordering::Release);
        lifetime
    }

    fn close(&self) {
        self.lifetime.fetch_add(1, Ordering::AcqRel);
        self.in_flight.store(0, Ordering::Release);
        // A queued request can install directly once closed. A proposal
        // already dequeued for negotiation is absent here and invalidated by
        // the lifetime.
        self.state
            .update(Ordering::AcqRel, Ordering::Acquire, |word| {
                if word & PENDING != 0 {
                    (word >> 30) & SIZE_MASK
                } else {
                    word & SIZE_MASK
                }
            });
    }

    fn restore(&self, size: (u32, u32)) {
        let size = encode_size(size);
        self.state
            .update(Ordering::AcqRel, Ordering::Acquire, |word| {
                if word & OPEN == 0 {
                    size
                } else {
                    (word & !(REQUEST_MASK | PENDING)) | (size << 30) | PENDING
                }
            });
    }

    fn accept(&self, size: (u32, u32)) {
        self.host_revision.fetch_add(1, Ordering::AcqRel);
        self.in_flight.store(0, Ordering::Release);
        self.state
            .update(Ordering::AcqRel, Ordering::Acquire, |word| {
                (word & !SIZE_MASK) | encode_size(size)
            });
    }

    /// Whether `lifetime` is the open editor's.
    fn current(&self, lifetime: u64) -> bool {
        self.lifetime.load(Ordering::Acquire) == lifetime && self.is_open()
    }

    /// Called before input and layout, with no application lock held. Only
    /// the processed proposal is consumed; an intervening restore remains
    /// queued. The proposal is held to `limits` as it is taken, since a
    /// queued request outlives a change of limits: the host is told, and the
    /// own window given, the same size.
    fn next_frame(
        &self,
        lifetime: u64,
        limits: Limits,
        request: impl FnOnce((u32, u32)) -> bool,
    ) -> Option<(u32, u32)> {
        if !self.current(lifetime) {
            return None;
        }
        // Dequeue before negotiating: `in_flight` owns the proposal during
        // the host call, while any newer restore can fill the mailbox.
        let before = self
            .state
            .fetch_and(!(REQUEST_MASK | PENDING), Ordering::AcqRel);
        if before & PENDING != 0 {
            let proposed = limits.clamp(decode_size(before >> 30));
            let revision = self.host_revision.load(Ordering::Acquire);
            let accepted = if proposed == decode_size(before) {
                false // Already at the requested size: consumed without a host call.
            } else {
                self.in_flight
                    .store(encode_size(proposed), Ordering::Release);
                request(proposed)
            };
            self.in_flight.store(0, Ordering::Release);
            if !self.current(lifetime) {
                return None;
            }
            let host_changed = self.host_revision.load(Ordering::Acquire) != revision;
            if accepted && !host_changed {
                self.state
                    .update(Ordering::AcqRel, Ordering::Acquire, |word| {
                        (word & !SIZE_MASK) | encode_size(proposed)
                    });
            }
        }
        let size = encode_size(self.size());
        (self.applied.swap(size, Ordering::AcqRel) != size).then(|| decode_size(size))
    }
}

/// An open editor, from [`EditorSize::open`] to [`EditorSize::close`]: a
/// frame's size source hands it back, so a source left from an earlier open
/// gives nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened(u64);

/// The editor's size and its own window, one per plug-in instance, made
/// wherever the plug-in keeps its saved state and read from any thread.
///
/// The shell calls, on the main thread: [`Self::open`] as the host attaches
/// the editor, before its window opens; [`Self::attach`] once the view has
/// opened in the host's, outside any frame; [`Self::next_frame`] from the
/// size source; [`Self::host_set_size`], [`Self::reported_size`] and
/// [`Self::limits`] for the host's size calls; and [`Self::close`] before
/// baseview closes the view. A frame calls [`Self::want`] and
/// [`Self::offered`]. A restore or the editor calls [`Self::request_size`]
/// from any thread.
pub struct EditorSize {
    /// The own window's title, and the name AppKit remembers its frame under.
    name: String,
    /// [`Limits`] in one word, which the plug-in may replace while open.
    limits: AtomicU64,
    native: NativeResize,
    /// The own window, from [`Self::attach`] to [`Self::close`]. Locked only
    /// to read or swap it, never across a call into the window or a drop of
    /// one: those can draw a frame on the spot, whose size source comes back
    /// here.
    own: Mutex<Option<Arc<OwnWindow>>>,
}

impl std::fmt::Debug for EditorSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorSize")
            .field("name", &self.name)
            .field("limits", &self.limits())
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

impl EditorSize {
    /// An editor of `size` logical pixels, clamped to `limits`, whose own
    /// window is titled and remembered under `name`. Touches no AppKit
    /// state, so it may be made on any thread.
    pub fn new(name: &str, size: (u32, u32), limits: Limits) -> Arc<Self> {
        let state = Self {
            name: name.to_owned(),
            limits: AtomicU64::new(limits.encode()),
            native: NativeResize::default(),
            own: Mutex::new(None),
        };
        state.native.initialize(limits.held().clamp(size));
        Arc::new(state)
    }

    fn slot(&self) -> MutexGuard<'_, Option<Arc<OwnWindow>>> {
        self.own.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn own_window(&self) -> Option<Arc<OwnWindow>> {
        self.slot().clone()
    }

    /// The size the host last accepted, or the own window gave, in logical
    /// pixels: what a save writes.
    pub fn size(&self) -> (u32, u32) {
        self.native.size()
    }

    /// The size the host is told: a proposal while the host is being asked
    /// for it, the accepted size otherwise.
    pub fn reported_size(&self) -> (u32, u32) {
        self.native.reported_size()
    }

    /// Whether the host has the editor attached.
    pub fn is_open(&self) -> bool {
        self.native.is_open()
    }

    /// The sizes the editor allows now.
    pub fn limits(&self) -> Limits {
        Limits::decode(self.limits.load(Ordering::Acquire))
    }

    /// Replace the limits, as for a minimum that follows the editor's own
    /// zoom. The accepted size stays until the host, the own window or a
    /// request changes it; later ones are held to these.
    pub fn set_limits(&self, limits: Limits) {
        self.limits.store(limits.encode(), Ordering::Release);
    }

    /// Ask for a size in logical pixels, within the limits: a restore's, or
    /// one the editor chose. An open editor stages it as a proposal its next
    /// frame negotiates, keeping the accepted size until then; a closed one
    /// installs it at once.
    pub fn request_size(&self, size: (u32, u32)) {
        self.native.restore(self.limits().clamp(size));
    }

    /// The host sized the editor's window: refused outside the limits, and
    /// otherwise taken, unless the own window owns the size: the view is in
    /// it, which leaves the host's empty, or the editor is not attached and
    /// `opens_out`, asked only then, says it will open there. Then the editor
    /// keeps its size, the one its project saved (Yan decided,
    /// egui-plugin-window#12), and the call succeeds, as the host's window
    /// may be sized as the host likes.
    pub fn host_set_size(&self, size: (u32, u32), opens_out: impl FnOnce() -> bool) -> bool {
        if self.limits().clamp(size) != size {
            return false;
        }
        let owns = if self.is_open() {
            self.is_out()
        } else {
            cfg!(target_os = "macos") && opens_out()
        };
        if !owns {
            self.native.accept(size);
        }
        true
    }

    /// The host attached the editor: open, before its window opens, whose
    /// size source hands the result to [`Self::next_frame`].
    pub fn open(&self) -> Opened {
        Opened(self.native.open())
    }

    /// The editor's view has opened in the host's AppKit view `host`, and
    /// moves into its own window at once if `out`. Called once per open,
    /// after [`Self::open`], on the main thread, outside any frame, holding
    /// no lock a frame takes. `closed` tells the host the user closed the
    /// window, and returns whether it could; without it the window has no
    /// close button.
    ///
    /// # Safety
    ///
    /// `host` and `view` must be live `NSView`s, `view` inside `host`.
    pub unsafe fn attach(
        self: &Arc<Self>,
        host: *mut c_void,
        view: *mut c_void,
        out: bool,
        closed: Option<Box<dyn Fn() -> bool>>,
    ) {
        if !self.is_open() {
            return;
        }
        let own = OwnWindow::new(&self.name);
        // In the slot before the move out, which can draw a frame on the
        // spot whose size source asks it. One already there, from a shell
        // that attached twice, goes once the lock is let go.
        let previous = self.slot().replace(own.clone());
        drop(previous);
        let editor: Weak<Self> = Arc::downgrade(self);
        // From a move out the editor's size is the own window's, which a
        // save must find at once rather than at the next frame, since none
        // runs while the editor cannot be seen. Atomics only, as `moving`
        // runs inside the moves.
        let moving = Box::new(move |moved| {
            if let (Move::Out(size), Some(editor)) = (moved, editor.upgrade()) {
                editor.native.accept(size);
            }
        });
        // SAFETY: the caller's.
        unsafe { own.open(host, view, out, self.limits(), closed, moving) };
    }

    /// The frame's size, from the size source of the open editor `opened`:
    /// the own window's say first, then a pending proposal, which the own
    /// window grants in the host's place while the view is in it and
    /// `request_resize` asks the host for otherwise, and then any size the
    /// host set. `None` when the size is the one the frame last had.
    pub fn next_frame(
        &self,
        opened: Opened,
        request_resize: impl Fn() -> bool,
    ) -> Option<(u32, u32)> {
        if !self.native.current(opened.0) {
            return None;
        }
        let own = self.own_window();
        if let Some(own) = &own {
            let limits = self.limits();
            match own.take_size(limits) {
                Some(OwnSize::Resized(size)) => self.native.accept(limits.clamp(size)),
                // The host's window kept whatever size it last had;
                // `reported_size` gives the editor's, which it is asked for.
                Some(OwnSize::Docked) => {
                    request_resize();
                }
                None => {}
            }
        }
        self.native.next_frame(opened.0, self.limits(), |size| {
            match own.as_ref().and_then(|own| own.resize(size)) {
                // Taken as a host's `set_size` during the request is: the
                // size the window applied, which the proposal then does not
                // overwrite. As it is: the window has recorded that size,
                // and another here would lay the view out at one size in a
                // window of another.
                Some(applied) => {
                    self.native.accept(applied);
                    true
                }
                None => request_resize(),
            }
        })
    }

    /// Whether the editor can have a window of its own here: it opened in an
    /// AppKit host's view.
    pub fn offered(&self) -> bool {
        self.own_window().is_some_and(|own| own.offered())
    }

    /// Whether the view is in its own window now.
    pub fn is_out(&self) -> bool {
        self.own_window().is_some_and(|own| own.is_out())
    }

    /// Ask for the editor in its own window or in the host's, from a frame:
    /// performed on the run loop's next turn.
    pub fn want(&self, out: bool) {
        if let Some(own) = self.own_window() {
            own.want(out);
        }
    }

    /// The editor is closing, before baseview closes the view: the own
    /// window closes, giving the view back to the host's, and a request not
    /// yet negotiated is installed for saving and the next open. The window
    /// is let go here, so it does not keep the editor's views alive.
    pub fn close(&self) {
        let own = self.slot().take();
        if let Some(own) = own {
            own.close();
        }
        self.native.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const INITIAL: (u32, u32) = (1120, 640);
    const LIMITS: Limits = Limits {
        min: (640, 360),
        max: (4096, 4096),
    };
    /// Limits no test's size reaches, for the negotiation alone.
    const WIDE: Limits = Limits {
        min: (1, 1),
        max: (32767, 32767),
    };

    fn state() -> (NativeResize, u64) {
        let state = NativeResize::default();
        state.initialize(INITIAL);
        let lifetime = state.open();
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| panic!()),
            Some(INITIAL)
        );
        (state, lifetime)
    }

    #[test]
    fn native_resize_acceptance_refusal_and_save_during_round_trip() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        assert_eq!(state.reported_size(), INITIAL);
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                assert_eq!(state.reported_size(), (800, 500));
                assert_eq!(state.size(), INITIAL);
                false
            }),
            None
        );
        assert_eq!(state.size(), INITIAL);
        state.close();
        let lifetime = state.open();
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| panic!(
                "refused request must not return"
            )),
            Some(INITIAL)
        );
        state.restore((800, 500));
        assert_eq!(state.next_frame(lifetime, WIDE, |_| true), Some((800, 500)));
        assert_eq!(state.reported_size(), (800, 500));
    }

    #[test]
    fn native_resize_identical_restore_during_refusal_remains_pending() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                state.restore((800, 500));
                false
            }),
            None
        );
        assert_eq!(state.size(), INITIAL);
        let mut requests = 0;
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                requests += 1;
                true
            }),
            Some((800, 500))
        );
        assert_eq!(requests, 1);
    }

    #[test]
    fn native_resize_identical_restore_during_acceptance_coalesces_next_frame() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                state.restore((800, 500));
                true
            }),
            Some((800, 500))
        );
        assert_ne!(state.state.load(Ordering::Acquire) & PENDING, 0);
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| panic!("already accepted size")),
            None
        );
        assert_eq!(state.state.load(Ordering::Acquire) & PENDING, 0);
    }

    #[test]
    fn native_resize_echo_newer_host_and_newer_restore() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                state.accept((800, 500));
                state.accept((900, 600));
                state.restore((1000, 700));
                true
            }),
            Some((900, 600))
        );
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| true),
            Some((1000, 700))
        );
        state.accept((1000, 700));
        assert_eq!(state.next_frame(lifetime, WIDE, |_| panic!()), None);
    }

    #[test]
    fn native_resize_same_size_restore_cancels_stale_and_closed_requests_do_not_replay() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        state.restore(INITIAL);
        assert_eq!(state.next_frame(lifetime, WIDE, |_| panic!()), None);
        state.restore((800, 500));
        state.close();
        state.restore((900, 600));
        let next_lifetime = state.open();
        assert_eq!(state.next_frame(lifetime, WIDE, |_| panic!()), None);
        assert_eq!(
            state.next_frame(next_lifetime, WIDE, |_| panic!()),
            Some((900, 600))
        );
        state.restore((800, 500));
        let mut reopened_lifetime = 0;
        state.next_frame(next_lifetime, WIDE, |_| {
            state.close();
            reopened_lifetime = state.open();
            state.restore((1000, 700));
            true
        });
        assert_eq!(state.size(), (900, 600));
        assert_eq!(
            state.next_frame(reopened_lifetime, WIDE, |_| true),
            Some((1000, 700))
        );
    }

    #[test]
    fn native_resize_new_restore_of_accepted_size_survives_old_acceptance() {
        let (state, lifetime) = state();
        state.restore((800, 500));
        assert_eq!(
            state.next_frame(lifetime, WIDE, |_| {
                state.restore(INITIAL);
                true
            }),
            Some((800, 500))
        );
        assert_eq!(state.next_frame(lifetime, WIDE, |_| true), Some(INITIAL));
    }

    /// A request queued before the limits rose is held to the new ones as
    /// it is taken: the host is told, and the editor takes, a size within
    /// them.
    #[test]
    fn a_queued_request_is_held_to_limits_raised_since() {
        let editor = EditorSize::new("test", INITIAL, LIMITS);
        let opened = editor.open();
        editor.next_frame(opened, || panic!());
        editor.request_size((700, 400));
        editor.set_limits(Limits {
            min: (800, 450),
            max: (4096, 4096),
        });
        assert_eq!(
            editor.next_frame(opened, || {
                assert_eq!(editor.reported_size(), (800, 450));
                true
            }),
            Some((800, 450))
        );
    }

    /// Limits outside what the word holds are held to it rather than
    /// wrapped or panicking.
    #[test]
    fn limits_are_held_to_the_word() {
        let editor = EditorSize::new(
            "test",
            (u32::MAX, 0),
            Limits {
                min: (0, 900),
                max: (99_999, 10),
            },
        );
        assert_eq!(
            editor.limits(),
            Limits {
                min: (1, 900),
                max: (32767, 900),
            }
        );
        assert_eq!(editor.size(), (32767, 900));
    }

    #[test]
    fn host_sizes_are_held_to_the_limits_and_persist() {
        let editor = EditorSize::new("test", INITIAL, LIMITS);
        assert_eq!(editor.limits().clamp((1, 9000)), (640, 4096));
        assert!(!editor.host_set_size((639, 360), || false));
        assert!(!editor.host_set_size((4097, 360), || false));
        assert_eq!(editor.size(), INITIAL);
        assert!(editor.host_set_size((801, 503), || false));
        assert_eq!(editor.size(), (801, 503));
        assert_eq!(editor.reported_size(), (801, 503));
        let opened = editor.open();
        assert_eq!(editor.next_frame(opened, || panic!()), Some((801, 503)));
        editor.request_size((1500, 900));
        assert_eq!(editor.size(), (801, 503));
        assert_eq!(
            editor.next_frame(opened, || {
                assert_eq!(editor.reported_size(), (1500, 900));
                assert_eq!(editor.size(), (801, 503));
                true
            }),
            Some((1500, 900))
        );
        editor.close();
        editor.request_size((u32::MAX, 0));
        assert_eq!(editor.size(), (4096, 360));
    }

    #[test]
    fn limits_follow_set_limits_while_open() {
        let editor = EditorSize::new(
            "test",
            (1400, 800),
            Limits {
                min: (800, 450),
                max: (4096, 4096),
            },
        );
        let opened = editor.open();
        editor.next_frame(opened, || panic!());
        editor.set_limits(Limits {
            min: (1280, 720),
            max: (4096, 4096),
        });
        assert_eq!(editor.size(), (1400, 800));
        assert!(!editor.host_set_size((900, 500), || false));
        editor.request_size((900, 500));
        assert_eq!(editor.next_frame(opened, || true), Some((1280, 720)));
    }

    #[test]
    fn an_unprocessed_request_survives_close_save_and_reopen() {
        let editor = EditorSize::new("test", INITIAL, LIMITS);
        editor.open();
        editor.request_size((800, 500));
        assert_eq!(editor.size(), INITIAL);
        editor.close();
        assert_eq!(editor.size(), (800, 500));
        let opened = editor.open();
        assert_eq!(
            editor.next_frame(opened, || panic!("a closed request needs no host request")),
            Some((800, 500))
        );
    }

    /// Before the host attaches the editor, the saved choice of window
    /// decides whether a host size is the editor's; attached without an own
    /// window, every host size is. A source from an earlier open gives
    /// nothing.
    #[test]
    fn the_saved_choice_owns_the_size_until_attach() {
        let editor = EditorSize::new("test", INITIAL, LIMITS);
        let asked = Cell::new(0);
        let opens_out = || {
            asked.set(asked.get() + 1);
            true
        };
        assert!(editor.host_set_size((800, 500), opens_out));
        let expected = if cfg!(target_os = "macos") {
            INITIAL
        } else {
            (800, 500)
        };
        assert_eq!(editor.size(), expected);
        assert_eq!(asked.get(), u32::from(cfg!(target_os = "macos")));

        let earlier = editor.open();
        editor.close();
        let opened = editor.open();
        assert!(editor.host_set_size((900, 600), || panic!("asked while open")));
        assert_eq!(editor.size(), (900, 600));
        assert!(!editor.offered());
        assert_eq!(editor.next_frame(earlier, || panic!()), None);
        assert_eq!(editor.next_frame(opened, || panic!()), Some((900, 600)));
    }
}
