#![allow(deprecated)] // Allow use of NSFilenamesPboardType for now

use super::cursor::Cursor;
use super::keyboard::{make_modifiers, KeyboardState};
use super::window::WindowSharedState;
use crate::wrappers::appkit::*;
use crate::MouseEvent::{ButtonPressed, ButtonReleased};
use crate::{
    DropData, DropEffect, Event, EventStatus, MouseButton, MouseEvent, Point, ScrollDelta, Size,
    UnseenNotice, WindowEvent, WindowHandler, WindowInfo, WindowOpenOptions,
};
use objc2::__framework_prelude::Retained;
use objc2::rc::Weak;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{msg_send, AllocAnyThread};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingInfo, NSEvent, NSFilenamesPboardType, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSWindow, NSWindowOcclusionState,
};
use objc2_foundation::{NSArray, NSNotification, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CACurrentMediaTime, CADisplayLink};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

pub enum ViewParentingType {
    Parented { parent_view: Weak<NSView> },
    Windowed { owned_window: Weak<NSWindow>, running_app: Weak<NSApplication> },
}

pub(crate) struct BaseviewView {
    pub(crate) state: Rc<WindowSharedState>,
    window_handler: RefCell<Option<Box<dyn WindowHandler>>>,

    /// Events that will be triggered at the end of `window_handler`'s borrow.
    deferred_events: RefCell<VecDeque<Event>>,

    /// A `CursorLeft` held back because the pointer left with a button down
    /// (see `mouse_exited`), owed to the handler once the button comes up.
    exit_withheld: Cell<bool>,

    /// Buttons this view has reported pressed and not yet reported released,
    /// in `NSEvent::pressedMouseButtons`' bit order (see
    /// `buttons_owed_release`). One bit is one release owed, and whoever pays
    /// it — AppKit's `mouseUp:`, or `settle_stuck_buttons` — the other does not
    /// pay it again.
    ///
    /// A bit, which is not the same as a press: every button past the first two
    /// shares bit 2, so two of them held at once are one bit between them. The
    /// second release is swallowed as a duplicate, and a button still held
    /// after the first has been released is no longer recorded to be repaired.
    /// Neither reaches the consumer, because a button is a bool at that end too
    /// (egui's `down[Middle]`): the first release already puts it up, so nothing
    /// is left held and no scroll area stays shut. Counting presses per bit
    /// would buy a truer number that nothing reads.
    buttons_reported_down: Cell<u8>,

    /// Of those, the ones the OS already showed up at the previous SETTLE — a
    /// release is synthesised only for a button stuck across two of them. A
    /// `mouseUp:` sitting in the queue while the hardware is already up is an
    /// ordinary fast click, and it deserves the gap it takes to arrive.
    ///
    /// A settle rather than a frame interval, because a settle is what this
    /// counts: `trigger_frame` runs it, and the display link is not
    /// `trigger_frame`'s only caller — `handle_occlusion_notification` paints
    /// from inside a notification. Two settles can land microseconds apart
    /// there, leaving a queued release almost no gap at all. What that costs is
    /// a release sent early, carrying `last_mods()` rather than the event's
    /// modifiers, on a window coming back from occlusion.
    buttons_seen_up: Cell<u8>,

    /// What runs frames: one call per refresh of the view's screen, created
    /// once the view is parented and invalidated by `close`. It retains the
    /// view, so a link that is never invalidated leaks it.
    display_link: Cell<Option<Retained<CADisplayLink>>>,
    /// Draw on every Nth refresh; 1 is every refresh. See `link_paces_frame`.
    frame_divisor: Cell<u32>,
    /// The earliest target timestamp the next divided frame may aim at.
    next_frame_due: Cell<Option<f64>>,
    /// The link callback running the current frame, if one is.
    frame_timing: Cell<Option<crate::FrameTiming>>,
    notification_center_observer: Cell<Option<NotificationCenterObserver>>,
    occlusion_observer: Cell<Option<NotificationCenterObserver>>,

    /// Whether the handler exists to be told: the first `viewDidMoveToWindow`
    /// runs during parenting, before it does.
    occlusion_ready: Cell<bool>,

    /// Whom to tell that the view stopped being seen. See `UnseenNotice`.
    unseen_notice: Option<UnseenNotice>,
    /// `is_visible`'s last answer, as `check_seen` last asked it. False until
    /// the first ask, so the view's opening tells nothing.
    seen: Cell<bool>,
    /// A loss found while the handler was borrowed, told once it is not.
    unseen_owed: Cell<bool>,

    keyboard_state: KeyboardState,

    parenting: ViewParentingType,
}

impl BaseviewView {
    pub fn new<H: WindowHandler + 'static>(
        options: WindowOpenOptions, builder: impl FnOnce(&mut crate::Window) -> H,
        parenting: ViewParentingType,
    ) -> (Retained<View<Self>>, Rc<WindowSharedState>) {
        let view_rect =
            NSRect::new(NSPoint::ZERO, NSSize::new(options.size.width, options.size.height));

        let state = Rc::new(WindowSharedState::new(&options));
        let unseen_notice = options.unseen_notice.clone();

        let inner = BaseviewView {
            state: state.clone(),

            deferred_events: RefCell::default(),
            exit_withheld: false.into(),
            buttons_reported_down: 0.into(),
            buttons_seen_up: 0.into(),
            keyboard_state: KeyboardState::new(),
            display_link: None.into(),
            frame_divisor: 1.into(),
            next_frame_due: None.into(),
            frame_timing: None.into(),
            window_handler: None.into(),
            notification_center_observer: None.into(),
            occlusion_observer: None.into(),
            occlusion_ready: false.into(),
            unseen_notice,
            seen: false.into(),
            unseen_owed: false.into(),
            parenting,
        };

        let view = View::new(view_rect, inner, |view| {
            // Set up parenting before handler setup
            match &view.parenting {
                ViewParentingType::Parented { parent_view } => {
                    let parent_view = parent_view.load().unwrap();
                    parent_view.addSubview(view.view);
                }
                ViewParentingType::Windowed { owned_window, .. } => {
                    let owned_window = owned_window.load().unwrap();
                    owned_window.setContentView(Some(view.view));
                    set_delegate(&owned_window, view.view);
                }
            }

            // Initialize handler
            view.window_handler.replace(Some(Box::new(builder(&mut view.into()))));

            // Set up anything that might trigger events to the handler

            // SAFETY: This static is a read-only constant
            let ns_filenames_pboard_type = unsafe { NSFilenamesPboardType };
            view.view.registerForDraggedTypes(&NSArray::from_slice(&[ns_filenames_pboard_type]));

            // Parented by now, so the link starts on the screen the view is
            // on; it follows the view to another one by itself.
            view.display_link.set(Some(view.view.setup_display_link()));

            let notifier_view = Weak::new(view.view);
            let observer = NotificationCenterObserver::register_window_key_change(move |n| {
                if let Some(view) = notifier_view.load() {
                    BaseviewView::handle_notification(view.inner_ref(), n);
                }
            });
            view.notification_center_observer.set(Some(observer));

            let occlusion_view = Weak::new(view.view);
            let observer = NotificationCenterObserver::register_window_occlusion_change(move |n| {
                if let Some(view) = occlusion_view.load() {
                    BaseviewView::handle_occlusion_notification(view.inner_ref(), n);
                }
            });
            view.occlusion_observer.set(Some(observer));

            // Send an initial Resized event so users get the correct scale factor and physical size.
            Self::trigger_event(
                view,
                Event::Window(WindowEvent::Resized(Self::fetch_view_size(view.view))),
            );

            // The first viewDidMoveToWindow ran during parenting, before the
            // handler existed. Sample only now, after its initial size event;
            // an unseen first window must be told before its reveal. The
            // first answer tells the app nothing: `seen` was false.
            view.occlusion_ready.set(true);
            Self::check_seen(view, Sample::Attached { reveal: false });
        });

        (view, state)
    }

    pub fn close(this: ViewRef<Self>) {
        this.state.closed.set(true);
        // Invalidating releases the link's retain on the view. Safe from
        // inside a link callback, which holds a reference of its own.
        if let Some(link) = this.display_link.take() {
            link.invalidate();
        }
        this.view.removeFromSuperview();

        if let ViewParentingType::Windowed { owned_window: parent_window, running_app } =
            &this.parenting
        {
            if let Some(parent_window) = parent_window.load() {
                parent_window.close();
            }

            if let Some(app) = running_app.load() {
                app.stop(Some(&app));
            }
        }
    }

    pub fn resize(this: ViewRef<Self>, size: Size) {
        // NOTE: macOS gives you a personal rave if you pass in fractional pixels here. Even
        // though the size is in fractional pixels.
        let size = NSSize::new(size.width.round(), size.height.round());

        this.view.setFrameSize(size);
        this.view.setNeedsDisplay(true);

        // If this is a standalone window then we'll also need to resize the window itself
        if let ViewParentingType::Windowed { owned_window, .. } = &this.parenting {
            if let Some(owned_window) = owned_window.load() {
                owned_window.setContentSize(size);
            }
        }

        Self::view_did_change_backing_properties(this);
    }

    /// Hand the handler an event and return its answer, or, while the handler
    /// is borrowed (the event arrived from inside a frame or another event),
    /// queue it and answer `Ignored`.
    ///
    /// Queued rather than borrowed again, which would panic in Bitwig's
    /// process. The queue holds events only while a borrow is held: every
    /// holder, this and `trigger_frame`, drains it before letting go, and an
    /// event arriving during the drain joins its end. So queued events reach
    /// the handler in the order they arrived, ahead of any event after the
    /// borrow ends.
    ///
    /// A queued event's answer is not known in time, and `Ignored` is the one
    /// that loses nothing: a key goes to the host, so the host never misses a
    /// release, and a drop is refused. What gives way is that a queued key
    /// press the editor would have owned acts in both places.
    fn trigger_event(this: ViewRef<Self>, event: Event) -> EventStatus {
        let status = {
            let Ok(mut handler) = this.window_handler.try_borrow_mut() else {
                this.deferred_events.borrow_mut().push_back(event);
                return EventStatus::Ignored;
            };
            let Some(handler) = handler.as_mut() else {
                return EventStatus::Ignored;
            };

            let status = handler.on_event(&mut this.into(), event);
            Self::send_deferred_events(this, handler.as_mut());
            status
        };
        Self::pay_unseen(this);
        status
    }

    /// The host still owns unhandled keys when its editor view moves to
    /// another window. That window's responder chain cannot reach the host.
    fn host_view(this: ViewRef<Self>) -> Option<Retained<NSView>> {
        match &this.parenting {
            ViewParentingType::Parented { parent_view } => parent_view.load(),
            ViewParentingType::Windowed { .. } => None,
        }
    }

    /// Ask `is_visible` again and keep the answer, the view's one reading of
    /// whether it can be seen, and tell what changed. Returns the answer.
    ///
    /// - The handler is told `Occluded` on a change, and on every answer for
    ///   a window the view has just attached to, whose renderer state starts
    ///   afresh.
    /// - The app is told a loss (`UnseenNotice`).
    /// - A gain runs the reveal frame at once where `sample` asks for it.
    ///
    /// Called at each event that can change it: the window's occlusion
    /// notification, `viewDidHide` and `viewDidUnhide`, a move to another
    /// window or to none, and the frame gate, which asks on every callback the
    /// frame divisor lets through and so repairs a lost notification, up to
    /// `divisor` refreshes late, while the link fires.
    fn check_seen(this: ViewRef<Self>, sample: Sample) -> bool {
        let seen = Self::is_visible(this);
        let was = this.seen.replace(seen);
        let attached = matches!(sample, Sample::Attached { .. });
        // Not after the window is closed: its own removal from the window is
        // the app's doing, and nothing is left to tell.
        if this.state.closed.get() {
            return seen;
        }
        if was != seen || attached {
            Self::trigger_event(this, Event::Window(WindowEvent::Occluded(!seen)));
        }
        if was && !seen {
            this.unseen_owed.set(true);
            Self::pay_unseen(this);
        }
        let reveal = match sample {
            Sample::Attached { reveal } => reveal,
            Sample::Changed { reveal } => reveal && !was,
        };
        if seen && reveal {
            // Seen again, and what is on screen is the drawable from before
            // it stopped being seen: the compositor keeps showing it until
            // something presents over it, and nothing presents while the view
            // is unseen (`trigger_frame` runs no frame for it, and a drawable
            // would be refused if one ran).
            //
            // So the ghost lives exactly as long as it takes to get one frame
            // out, and the event that reported the change is the earliest
            // that can happen: for a window, the occlusion state it reports
            // is the same state the drawable request checks, so no earlier
            // signal — activation, key window — buys a frame that would be
            // allowed to present.
            //
            // Painting HERE rather than leaving it to the next link callback
            // is therefore the whole of the fix. The event above marks the
            // handler for repaint; the callback that would act on it is up to
            // a frame away, and that wait IS the ghost: measured at 11-14 ms
            // under the ~15 ms frame timer the link replaced, and it scales
            // with the frame divisor.
            //
            // The dirty mark is for whatever AppKit draws itself, which for a
            // surface-backed window is nothing; the frame is what puts new
            // pixels up. It asks `is_visible` again, which answers as this
            // just did, so it runs.
            this.view.setNeedsDisplay(true);
            Self::trigger_frame(this);
        }
        seen
    }

    /// Tell a loss `check_seen` found, unless the window handler is borrowed
    /// (a frame or an event is running, and the loss was found inside it):
    /// then whoever holds the borrow pays it as the borrow ends, so the app
    /// is never called inside it and may call its host, whatever the host
    /// does to this window in return. A loss found and undone within one
    /// borrow (the view taken out of its window and put back in a visible one
    /// during a frame) is still told: it was owed when found.
    ///
    /// Nothing is told once the window is closed, including a loss owed from
    /// inside the borrow that closed it.
    fn pay_unseen(this: ViewRef<Self>) {
        if !this.unseen_owed.get() || this.window_handler.try_borrow_mut().is_err() {
            return;
        }
        this.unseen_owed.set(false);
        if this.state.closed.get() {
            return;
        }
        // A clone, so a host that destroys the editor from inside the call,
        // and with it this view, cannot free the closure while it runs.
        if let Some(notice) = this.unseen_notice.clone() {
            notice.tell();
        }
    }

    /// Draw on every `divisor`th refresh from here on; 0 and 1 are every one.
    pub(crate) fn set_frame_divisor(this: ViewRef<Self>, divisor: u32) {
        this.frame_divisor.set(divisor.max(1));
    }

    /// When the display asked for the frame now running, if the link did.
    pub(crate) fn frame_timing(this: ViewRef<Self>) -> Option<crate::FrameTiming> {
        this.frame_timing.get()
    }

    /// Run one frame, unless the view cannot be seen or a frame is already
    /// running.
    ///
    /// The link is not the only caller — `handle_occlusion_notification`
    /// paints from inside a notification, and a notification can in principle
    /// be delivered while a frame holds the handler. Taking the borrow
    /// fallibly rather than with `borrow_mut` is what makes that safe: a
    /// panicking borrow would turn a re-entrant delivery into a crash.
    ///
    /// What the dropped frame costs in that case is a tick, not more: the
    /// frame in flight paints, and the event queued behind it is drained when
    /// it finishes, as `trigger_event` queues any event delivered inside a
    /// borrow.
    fn trigger_frame(this: ViewRef<Self>) {
        // A view nobody can see runs no frame: one hidden, or in a window
        // covered, minimized, ordered out or on another Space. Asked of AppKit
        // on every frame rather than kept from the occlusion notification, so
        // it cannot go stale when a notification is lost. Its events still
        // arrive meanwhile, and the first frame once it shows draws what they
        // changed: the reveal's, run from `check_seen` by the event that
        // reports the view seen again.
        //
        // Ahead of the settles as well: an unseen view has no gesture to
        // settle. Whatever is owed then is paid once it shows, before anything
        // is drawn from it: a withheld exit on the first frame, a stuck button
        // on the second, since a release needs two settles to be confirmed.
        if !Self::check_seen(this, Sample::Changed { reveal: false }) {
            return;
        }

        // Before the frame, so a pointer that has quietly stopped being ours is
        // gone by the time anything is drawn from where it was. The button
        // first: a withheld exit waits for every release still owed
        // (`exit_waits`), so a stuck release repaired here lets the exit be
        // paid in the same settle rather than the next one.
        Self::settle_stuck_buttons(this);
        Self::settle_pointer_exit(this);

        {
            let Ok(mut handler) = this.window_handler.try_borrow_mut() else { return };
            let Some(handler) = handler.as_mut() else { return };

            handler.on_frame(&mut this.into());
            Self::send_deferred_events(this, handler.as_mut());
        }
        Self::pay_unseen(this);
    }

    /// Whether any of the view can be on screen. AppKit's occlusion state is
    /// conservative: a window with any part showing is visible.
    ///
    /// Three AppKit calls on every frame the link runs, which is every refresh
    /// while the window is covered (a covered window's link keeps firing), so
    /// nothing more is asked here.
    fn is_visible(this: ViewRef<Self>) -> bool {
        !this.view.isHiddenOrHasHiddenAncestor()
            && this.view.window().is_some_and(|window| window_shows(&window))
    }

    /// Pay an exit `mouse_exited` held back, once no button is down and no
    /// release is owed (`exit_waits`).
    ///
    /// Called from every mouse-up, which is the ordinary way a drag off the
    /// window ends, and from the frame tick, which is the only way the ones
    /// that never reach us do. A release goes missing whenever the press was
    /// not ours to begin with — the pointer crossing the view mid-drag gets
    /// enter and exit from the tracking area but no buttons at all — and
    /// whenever the host takes the release out of our hands. Either way the
    /// button is up and the OS says so, so the tick catches it within a frame.
    ///
    /// The owed release is what keeps "release, then exit" in order: a frame
    /// can run between the hardware button going up and its queued `mouseUp:`
    /// being dispatched, and the OS alone would pay the exit there, ahead of
    /// the release. A release that never comes is repaired by
    /// `settle_stuck_buttons` within two settles, which `trigger_frame` runs
    /// first, so the exit is paid in the settle that repairs it.
    fn settle_pointer_exit(this: ViewRef<Self>) {
        if !this.exit_withheld.get()
            || exit_waits(NSEvent::pressedMouseButtons(), this.buttons_reported_down.get())
        {
            return;
        }
        this.exit_withheld.set(false);
        Self::trigger_event(this, Event::Mouse(MouseEvent::CursorLeft));
    }

    /// Report the release of a button the OS says is no longer held.
    ///
    /// A press this view reported is owed a release, and AppKit does not always
    /// pay: a `mouseUp:` handed to a host popup, a modal that opens under the
    /// finger, a gesture the host takes out of our hands — the press is ours,
    /// the release is delivered somewhere else, and nothing ever tells the view.
    /// The handler is then left believing a button is down forever.
    ///
    /// What that costs is out of all proportion to how obscure it sounds,
    /// because a consumer's drag state is global: egui gates EVERY `ScrollArea`
    /// on `dragged_id().is_none()`, so one unreleased press silently stops the
    /// wheel in every scrollable pane at once, with the window focused and the
    /// pointer sitting right over the pane that will not move.
    ///
    /// The two guards already here do not reach it. `settle_pointer_exit` waits
    /// on an exit that never happens if the pointer never leaves, and a consumer
    /// that ends its drags on lost focus never sees focus go. Both are about the
    /// POINTER; this is about the BUTTON, and a stuck button is invisible in
    /// terms of either.
    ///
    /// So ask the OS, which is the same authority `settle_pointer_exit` already
    /// trusts over any count of the presses this view has seen. Ordinary
    /// gestures are untouched: through a real drag, however far it wanders, the
    /// button reads down and nothing is synthesised.
    fn settle_stuck_buttons(this: ViewRef<Self>) {
        // Nothing owed and nothing pending is nothing to settle, which is the
        // state all but a handful of settles are in. Read locally first, the
        // way `settle_pointer_exit` reads `exit_withheld` before it asks AppKit
        // anything — a matched shape rather than a measured cost. Both masks,
        // because a bit left in `buttons_seen_up` by a press that has since
        // been released properly still has to be cleared below.
        if (this.buttons_reported_down.get() | this.buttons_seen_up.get()) == 0 {
            return;
        }

        let owed =
            buttons_owed_release(this.buttons_reported_down.get(), NSEvent::pressedMouseButtons());
        // Stuck at this settle AND the one before it. A single one is not
        // evidence: the hardware goes up before the `mouseUp:` queued behind it
        // is dispatched, so a fast enough click reads exactly like a stuck
        // button once.
        let confirmed = owed & this.buttons_seen_up.replace(owed);
        if confirmed == 0 {
            return;
        }

        // Where the pointer is now, since there is no event to say where the
        // button went up. A settle runs only for a visible view, which is in a
        // window, so this does not return; if it did, the release stays owed
        // to the next settle rather than being sent from a made-up position.
        let Some(window) = this.view.window() else { return };
        let position = Self::view_point(this, window.mouseLocationOutsideOfEventStream());
        this.buttons_reported_down.set(this.buttons_reported_down.get() & !confirmed);
        let modifiers = make_modifiers(this.keyboard_state.last_mods());
        // Which bit stands for which button is `button_bit`'s to answer, asked
        // rather than restated. A table written out here would be an inverse
        // with nothing holding it to the original, and one entry out of step
        // reports the wrong button going up: a stuck RIGHT button released as
        // Middle leaves the right one held, and every scroll area shut — the
        // bug this whole path exists to repair, silently back.
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            if confirmed & button_bit(button) != 0 {
                Self::trigger_event(
                    this,
                    Event::Mouse(ButtonReleased { button, position, modifiers }),
                );
            }
        }
    }

    /// Report a press, and record the release it now owes the handler.
    fn report_press(this: ViewRef<Self>, button: MouseButton, event: &NSEvent) {
        this.buttons_reported_down.set(this.buttons_reported_down.get() | button_bit(button));
        Self::trigger_event(
            this,
            Event::Mouse(ButtonPressed {
                button,
                position: Self::view_point(this, event.locationInWindow()),
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    /// Report a release, unless the debt is already paid.
    ///
    /// A `mouseUp:` for a bit that is no longer set is dropped rather than
    /// passed on — the release `settle_stuck_buttons` gave up on and sent
    /// itself, or the second of two buttons sharing bit 2. Sending it twice
    /// would end the NEXT gesture as well: a second release against a fresh
    /// press is how a click lands on whatever the pointer moved to since.
    fn report_release(this: ViewRef<Self>, button: MouseButton, event: &NSEvent) {
        let owed = this.buttons_reported_down.get();
        if owed & button_bit(button) != 0 {
            this.buttons_reported_down.set(owed & !button_bit(button));
            Self::trigger_event(
                this,
                Event::Mouse(ButtonReleased {
                    button,
                    // Outside the view for a drag let go past the window's
                    // edge, where the gesture ends.
                    position: Self::view_point(this, event.locationInWindow()),
                    modifiers: make_modifiers(event.modifierFlags()),
                }),
            );
        }
        // After the release, never before it: the gesture ends where the
        // pointer still is, and only then is the pointer gone.
        Self::settle_pointer_exit(this);
    }

    /// A point in the window's coordinates, in the view's: the one conversion
    /// every pointer position this view reports goes through.
    fn view_point(this: ViewRef<Self>, in_window: NSPoint) -> Point {
        let point = this.view.convertPoint_fromView(in_window, None);
        Point { x: point.x, y: point.y }
    }

    fn send_deferred_events(this: ViewRef<Self>, window_handler: &mut dyn WindowHandler) {
        let mut window = this.into();
        loop {
            let next_event = { this.deferred_events.borrow_mut().pop_front() };
            if let Some(event) = next_event {
                window_handler.on_event(&mut window, event);
            } else {
                break;
            }
        }
    }

    fn handle_occlusion_notification(this: ViewRef<Self>, notification: &NSNotification) {
        let Some(window) = this.view.window() else { return };
        let Some(notification_object) = notification.object().and_then(|o| o.downcast().ok())
        else {
            return;
        };

        // Unlike focus, occlusion is a property of the whole window: no
        // first-responder check — every embedded view wants to know.
        if window != notification_object {
            return;
        }

        Self::check_seen(this, Sample::Changed { reveal: true });
    }

    fn fetch_view_size(view: &NSView) -> WindowInfo {
        let ns_window = view.window();

        let scale_factor: f64 = ns_window.map(|w| w.backingScaleFactor()).unwrap_or(1.0);

        let bounds = view.bounds();

        WindowInfo::from_logical_size(
            Size::new(bounds.size.width, bounds.size.height),
            scale_factor,
        )
    }
}

/// What a call to `check_seen` samples for, and whether a gain runs the
/// reveal frame at once.
#[derive(Clone, Copy)]
enum Sample {
    /// A possible change of the same attachment.
    Changed { reveal: bool },
    /// The view attached to a window, or left one: the handler is told the
    /// answer whatever it was before.
    Attached { reveal: bool },
}

/// Whether any of `window` can be on screen, by its occlusion state: the
/// window half of `is_visible`, and what an occlusion notification reports.
fn window_shows(window: &NSWindow) -> bool {
    #[cfg(feature = "testing")]
    if let Some(shows) = crate::testing::WINDOW_SHOWS.with(std::cell::Cell::get) {
        return shows;
    }
    window.occlusionState().contains(NSWindowOcclusionState::Visible)
}

/// Whether a link callback aimed at `target` runs a frame under `divisor`, and
/// if so the earliest target the next one may aim at (`None` when undivided).
///
/// Paced on the link's target timestamp rather than by counting callbacks: a
/// callback the host's run loop delays or coalesces would shift a count, while
/// the timestamps stay on the display's grid. The next frame is due half a
/// refresh short of `divisor` refreshes ahead, so jitter in the timestamps
/// cannot push it a whole refresh late. After a move to a slower display the
/// due time is at most `divisor` of the old refreshes away, so the cadence
/// recovers by itself.
fn link_paces_frame(
    divisor: u32, due: Option<f64>, target: f64, refresh: f64,
) -> Option<Option<f64>> {
    if divisor <= 1 {
        return Some(None);
    }
    if due.is_some_and(|due| target < due) {
        return None;
    }
    Some(Some(target + (f64::from(divisor) - 0.5) * refresh))
}

impl Drop for BaseviewView {
    fn drop(&mut self) {
        self.state.closed.set(true);
    }
}

impl ViewImpl for BaseviewView {
    fn become_first_responder(this: ViewRef<Self>) -> bool {
        let Some(window) = this.view.window() else {
            return true;
        };

        if window.isKeyWindow() {
            Self::trigger_event(this, Event::Window(WindowEvent::Focused));
        }

        true
    }

    fn resign_first_responder(this: ViewRef<Self>) -> bool {
        Self::trigger_event(this, Event::Window(WindowEvent::Unfocused));
        true
    }

    fn window_should_close(this: ViewRef<Self>) -> bool {
        Self::trigger_event(this, Event::Window(WindowEvent::WillClose));
        Self::close(this);

        true
    }

    fn view_did_change_backing_properties(this: ViewRef<Self>) {
        let new_window_info = Self::fetch_view_size(this.view);
        let window_info = this.state.window_info.get();

        // Only send the event when the window's size has actually changed to be in line with the
        // other platform implementations
        if new_window_info.physical_size() != window_info.physical_size() {
            this.state.window_info.set(new_window_info);
            Self::trigger_event(
                this,
                Event::Window(WindowEvent::Resized(new_window_info)),
            );
        }
    }

    fn view_will_move_to_window(this: ViewRef<Self>, new_window: Option<&NSWindow>) {
        let tracking_areas = this.view.trackingAreas();

        match new_window {
            None => {
                if tracking_areas.count() > 0 {
                    let tracking_area = tracking_areas.objectAtIndex(0);
                    this.view.removeTrackingArea(&tracking_area);
                }
            }
            Some(new_window) => {
                if tracking_areas.is_empty() {
                    let tracking_area = new_tracking_area(this.view);
                    this.view.addTrackingArea(&tracking_area);
                }

                new_window.setAcceptsMouseMovedEvents(true);
                new_window.makeFirstResponder(Some(this.view));
            }
        }

        unsafe {
            let superclass = msg_send![this.view, superclass];

            let () = msg_send![super(this.view, superclass), viewWillMoveToWindow: new_window];
        }
    }

    fn display_link_fired(this: ViewRef<Self>, link: &CADisplayLink) {
        let fired = CACurrentMediaTime();
        let fired_at = std::time::Instant::now();
        let target = link.targetTimestamp();
        let refresh = link.duration();
        let divisor = this.frame_divisor.get();
        let Some(due) = link_paces_frame(divisor, this.next_frame_due.get(), target, refresh)
        else {
            return;
        };
        this.next_frame_due.set(due);
        this.frame_timing.set(Some(crate::FrameTiming {
            fired,
            fired_at,
            timestamp: link.timestamp(),
            target_timestamp: target,
            refresh,
            divisor,
        }));
        Self::trigger_frame(this);
        this.frame_timing.set(None);
    }

    fn view_did_move_to_window(this: ViewRef<Self>) {
        unsafe {
            let superclass = msg_send![this.view, superclass];
            let () = msg_send![super(this.view, superclass), viewDidMoveToWindow];
        }

        // Once AppKit has attached the view, sample the new window even if
        // both windows report the same visibility. A view taken out of its
        // window, or moved into one not on screen, has stopped being seen.
        if !this.occlusion_ready.get() {
            return;
        }
        Self::check_seen(this, Sample::Attached { reveal: true });
    }

    fn view_did_hide(this: ViewRef<Self>) {
        unsafe {
            let superclass = msg_send![this.view, superclass];
            let () = msg_send![super(this.view, superclass), viewDidHide];
        }
        Self::check_seen(this, Sample::Changed { reveal: false });
    }

    fn view_did_unhide(this: ViewRef<Self>) {
        unsafe {
            let superclass = msg_send![this.view, superclass];
            let () = msg_send![super(this.view, superclass), viewDidUnhide];
        }
        Self::check_seen(this, Sample::Changed { reveal: true });
    }

    fn update_tracking_areas(this: ViewRef<Self>) {
        let tracking_areas = this.view.trackingAreas();
        if tracking_areas.count() > 0 {
            let tracking_area = tracking_areas.objectAtIndex(0);
            this.view.removeTrackingArea(&tracking_area);
        }

        let tracking_area = new_tracking_area(this.view);

        this.view.addTrackingArea(&tracking_area);
    }

    fn cursor_update(this: ViewRef<Self>, _event: &NSEvent) {
        // Sent via the tracking area (CursorUpdate | ActiveInActiveApp),
        // which works even while the window is NOT key — cursor RECTS only
        // activate once the window becomes key (i.e. after a click), so
        // this path covers the freshly-opened, not-yet-clicked window.
        Cursor::from(this.state.mouse_cursor.get()).load().set();
    }

    fn reset_cursor_rects(this: ViewRef<Self>) {
        // Re-assert the desired cursor over the whole view whenever AppKit
        // rebuilds cursor rects; without this, the host window's own rects
        // take over after any invalidation.
        let cursor = Cursor::from(this.state.mouse_cursor.get());
        this.view.addCursorRect_cursor(this.view.bounds(), &cursor.load());
    }

    fn mouse_moved(this: ViewRef<Self>, event: &NSEvent) {
        Self::trigger_event(
            this,
            Event::Mouse(MouseEvent::CursorMoved {
                position: Self::view_point(this, event.locationInWindow()),
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn scroll_wheel(this: ViewRef<Self>, event: &NSEvent) {
        // Report where the wheel happened before reporting the wheel itself.
        //
        // `scrollWheel:` carries a location but no CursorMoved, and consumers
        // route a wheel by where they last saw the pointer — egui hands it to
        // whatever its pointer position is over, and its wheel event has no
        // position of its own. That position can be absent or stale, because
        // `mouseMoved:` only arrives through the tracking area
        // (`ActiveInActiveApp`) and `mouseExited:` makes egui forget it
        // (`PointerGone`): a window opened with the cursor already inside it, or
        // one whose app was not frontmost while the cursor moved in, has never
        // been told where the pointer is. The wheel then did nothing at all, or
        // landed on whatever was under the LAST known position — scrolling one
        // pane while the cursor sat over another.
        //
        // The event's own location is always right, so send it first and the
        // wheel is applied where the cursor actually is. Gated on the location
        // being inside the view so inertial scrolling that outlives the pointer
        // leaving cannot resurrect it.
        let point = Self::view_point(this, event.locationInWindow());
        let bounds = this.view.bounds();
        let inside = point.x >= bounds.origin.x
            && point.y >= bounds.origin.y
            && point.x <= bounds.origin.x + bounds.size.width
            && point.y <= bounds.origin.y + bounds.size.height;
        if inside {
            Self::trigger_event(
                this,
                Event::Mouse(MouseEvent::CursorMoved {
                    position: point,
                    modifiers: make_modifiers(event.modifierFlags()),
                }),
            );
        }

        let x = event.scrollingDeltaX() as f32;
        let y = event.scrollingDeltaY() as f32;

        let delta = if event.hasPreciseScrollingDeltas() {
            ScrollDelta::Pixels { x, y }
        } else {
            ScrollDelta::Lines { x, y }
        };

        Self::trigger_event(
            this,
            Event::Mouse(MouseEvent::WheelScrolled {
                delta,
                modifiers: make_modifiers(event.modifierFlags()),
            }),
        );
    }

    fn dragging_entered(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> NSDragOperation {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragEntered {
            position: get_drag_position(sender),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        on_event(this, event)
    }

    fn dragging_updated(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> NSDragOperation {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragMoved {
            position: get_drag_position(sender),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        on_event(this, event)
    }

    fn prepare_for_drag_operation(
        _this: ViewRef<Self>, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> bool {
        // Always accept drag operation if we get this far
        // This function won't be called unless dragging_entered/updated
        // has returned an acceptable operation
        true
    }

    fn perform_drag_operation(
        this: ViewRef<Self>, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
    ) -> bool {
        let modifiers = this.keyboard_state.last_mods();
        let drop_data = get_drop_data(sender);

        let event = MouseEvent::DragDropped {
            position: get_drag_position(sender),
            modifiers: make_modifiers(modifiers),
            data: drop_data,
        };

        let event_status = Self::trigger_event(this, Event::Mouse(event));

        matches!(event_status, EventStatus::AcceptDrop(_))
    }

    fn dragging_exited(this: ViewRef<Self>, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
        on_event(this, MouseEvent::DragLeft);
    }

    fn handle_notification(this: ViewRef<Self>, notification: &NSNotification) {
        let Some(window) = this.view.window() else { return };
        // The subject of the notification, in this case an NSWindow object.
        let Some(notification_object) = notification.object().and_then(|o| o.downcast().ok())
        else {
            return;
        };

        // Only trigger focus events if the NSWindow that's being notified about is our window,
        // and if the window's first responder is our NSView.
        if window != notification_object {
            return;
        }

        let Some(first_responder) = window.firstResponder() else { return };

        // If the first responder isn't our NSView, the focus events will instead be triggered
        // by the becomeFirstResponder and resignFirstResponder methods on the NSView itself.
        if !this.view.isEqual(Some(&first_responder)) {
            return;
        }

        Self::trigger_event(
            this,
            Event::Window(if window.isKeyWindow() {
                WindowEvent::Focused
            } else {
                WindowEvent::Unfocused
            }),
        );
    }

    fn mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_press(this, MouseButton::Left, event);
    }

    fn mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_release(this, MouseButton::Left, event);
    }

    fn right_mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_press(this, MouseButton::Right, event);
    }

    fn right_mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_release(this, MouseButton::Right, event);
    }

    fn other_mouse_down(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_press(this, MouseButton::Middle, event);
    }

    fn other_mouse_up(this: ViewRef<Self>, event: &NSEvent) {
        Self::report_release(this, MouseButton::Middle, event);
    }

    fn mouse_entered(this: ViewRef<Self>) {
        this.state.mouse_inside.set(true);
        // Back inside before the exit was ever reported: as far as the handler
        // knows the pointer never left, so there is no entry to announce
        // either. The pair stays balanced (see `mouse_exited`).
        if this.exit_withheld.replace(false) {
            return;
        }
        Self::trigger_event(this, Event::Mouse(MouseEvent::CursorEntered));
    }

    fn mouse_exited(this: ViewRef<Self>) {
        this.state.mouse_inside.set(false);
        // A button down means the drag is still ours, so the exit waits.
        //
        // The tracking area is `EnabledDuringMouseDrag`, so `mouseExited:`
        // arrives mid-drag — while AppKit goes on sending every `mouseDragged:`
        // and the closing `mouseUp:` to the view the press landed in, wherever
        // the pointer has got to. Reporting the exit is what breaks the drag:
        // a consumer told the pointer is gone stops following it (egui's
        // `PointerGone`), and a slider dragged a pixel past the window edge
        // lets go under the hand still holding it.
        //
        // Held back rather than dropped, because the exit is real once the
        // button is up — `settle_pointer_exit` is where it is paid, and it
        // reads the button state from the OS rather than from a count of the
        // presses this view has seen, which a release delivered elsewhere
        // would leave wrong forever. It also waits while a release is owed:
        // the button can read up while its `mouseUp:` is still queued, and the
        // release belongs ahead of the exit (`exit_waits`).
        if exit_waits(NSEvent::pressedMouseButtons(), this.buttons_reported_down.get()) {
            this.exit_withheld.set(true);
            return;
        }
        Self::trigger_event(this, Event::Mouse(MouseEvent::CursorLeft));
    }

    fn key_down(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                if let Some(host) = Self::host_view(this) {
                    host.keyDown(event);
                } else {
                    unsafe {
                        let superclass = msg_send![this.view, superclass];

                        let () = msg_send![super(this.view, superclass), keyDown:event];
                    }
                }
            }
        }
    }

    fn key_up(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                if let Some(host) = Self::host_view(this) {
                    host.keyUp(event);
                } else {
                    unsafe {
                        let superclass = msg_send![this.view, superclass];

                        let () = msg_send![super(this.view, superclass), keyUp:event];
                    }
                }
            }
        }
    }

    fn flags_changed(this: ViewRef<Self>, event: &NSEvent) {
        if let Some(key_event) = this.keyboard_state.process_native_event(event) {
            let status = Self::trigger_event(this, Event::Keyboard(key_event));

            if let EventStatus::Ignored = status {
                if let Some(host) = Self::host_view(this) {
                    host.flagsChanged(event);
                } else {
                    unsafe {
                        let superclass = msg_send![this.view, superclass];

                        let () = msg_send![super(this.view, superclass), flagsChanged:event];
                    }
                }
            }
        }
    }
}

/// The bit standing for a button this view reports, in
/// `NSEvent::pressedMouseButtons`' order.
///
/// Every button but the first two shares one bit, because that is how they
/// reach the handler: `otherMouseDown:` reports the third, fourth and fifth
/// alike as `MouseButton::Middle`, so this side of the comparison cannot tell
/// them apart either (see `buttons_owed_release`).
fn button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0b001,
        MouseButton::Right => 0b010,
        _ => 0b100,
    }
}

/// Which of the buttons a view has reported pressed are no longer held,
/// according to `NSEvent::pressedMouseButtons`.
///
/// Both sides are bitmasks in that method's order — bit 0 the left button, bit
/// 1 the right — and they part company above that. AppKit gives every remaining
/// button a bit of its own (bit 2 the third button, bit 3 the fourth, on up a
/// mouse with side buttons), while `button_bit` has only the one to give them.
/// So the tail is folded to match: ANY button above the first two being down
/// counts as that bit held. Comparing the masks unfolded would read a thumb
/// button held on bit 3 as bit 2 standing empty, and end a gesture the hand is
/// still making.
fn buttons_owed_release(reported_down: u8, os_pressed: usize) -> u8 {
    let os_down = (os_pressed as u8 & 0b11) | u8::from(os_pressed & !0b11 != 0) << 2;
    reported_down & !os_down
}

/// Whether the pointer's exit waits: while the OS reports a button down, the
/// drag is still ours; while a reported press is still owed its release, the
/// release comes first, wherever the OS says the button is.
fn exit_waits(os_pressed: usize, releases_owed: u8) -> bool {
    os_pressed != 0 || releases_owed != 0
}

#[cfg(test)]
mod tests {
    use super::{button_bit, buttons_owed_release, exit_waits, link_paces_frame};
    use crate::MouseButton;

    /// The refreshes a divided link draws on, out of `callbacks` at 144 Hz
    /// whose target timestamps carry `jitter(i)` seconds of noise.
    fn drawn(divisor: u32, callbacks: u32, jitter: impl Fn(u32) -> f64) -> Vec<u32> {
        let refresh = 1.0 / 144.0;
        let mut due = None;
        (0..callbacks)
            .filter(|&i| {
                let target = 1000.0 + f64::from(i) * refresh + jitter(i);
                link_paces_frame(divisor, due, target, refresh).map(|next| due = next).is_some()
            })
            .collect()
    }

    #[test]
    fn a_divisor_draws_every_nth_refresh() {
        for divisor in 1..=4 {
            let expected: Vec<u32> = (0..48).step_by(divisor as usize).collect();
            assert_eq!(drawn(divisor, 48, |_| 0.0), expected, "divisor {divisor}");
        }
    }

    /// Timestamps a fraction of a refresh off the grid must not move a frame
    /// to the next refresh, which would turn a half rate into a third. The
    /// drawn frames swing early and late by turns, which is the case a due
    /// time of exactly `divisor` refreshes ahead gets wrong.
    #[test]
    fn timestamp_jitter_does_not_slip_a_divided_frame() {
        let refresh = 1.0 / 144.0;
        let wobble = |i: u32| if (i / 2) % 2 == 0 { 0.2 * refresh } else { -0.2 * refresh };
        assert_eq!(drawn(2, 12, wobble), [0, 2, 4, 6, 8, 10]);
    }

    /// A callback the run loop never delivered costs that refresh alone: the
    /// next one draws, rather than waiting for the old phase to come round.
    #[test]
    fn a_missed_callback_draws_at_the_next_one() {
        let refresh = 1.0 / 144.0;
        let mut due = None;
        let mut drew = Vec::new();
        for i in [0u32, 1, 3, 4, 5, 6] {
            let target = f64::from(i) * refresh;
            if let Some(next) = link_paces_frame(2, due, target, refresh) {
                due = next;
                drew.push(i);
            }
        }
        assert_eq!(drew, [0, 3, 5]);
    }

    /// A move to a slower display leaves the due time on the old grid, and the
    /// cadence picks up on the new one within the old divisor's reach rather
    /// than waiting on a count that no longer means anything.
    #[test]
    fn a_slower_display_does_not_stall_a_divided_link() {
        let fast = 1.0 / 144.0;
        let slow = 1.0 / 60.0;
        let due = link_paces_frame(4, None, 0.0, fast).unwrap();
        let first = (1..)
            .map(|k| f64::from(k) * slow)
            .find(|&target| link_paces_frame(4, due, target, slow).is_some())
            .unwrap();
        assert!(first <= 4.0 * fast + slow, "first 60 Hz frame at {first}");
    }

    /// A button held is a button owed nothing, however long the drag runs.
    #[test]
    fn a_button_the_os_still_holds_is_never_released() {
        for (button, os) in
            [(MouseButton::Left, 0b001), (MouseButton::Right, 0b010), (MouseButton::Middle, 0b100)]
        {
            let bit = button_bit(button);
            assert_eq!(
                buttons_owed_release(bit, os),
                0,
                "{button:?} was let go of while the OS still had it down",
            );
        }
    }

    /// The case the whole repair exists for: the press was reported, the OS
    /// says nothing is down, and no `mouseUp:` ever came.
    #[test]
    fn a_press_the_os_has_forgotten_is_owed_its_release() {
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let bit = button_bit(button);
            assert_eq!(
                buttons_owed_release(bit, 0),
                bit,
                "{button:?} stayed down forever after its release went missing",
            );
        }
    }

    /// One button going up does not take another down with it.
    #[test]
    fn only_the_button_the_os_let_go_of_is_released() {
        let (left, right) = (button_bit(MouseButton::Left), button_bit(MouseButton::Right));
        assert_eq!(buttons_owed_release(left | right, 0b010), left);
        assert_eq!(buttons_owed_release(left | right, 0b001), right);
    }

    /// A side button holds the one bit every button past the first two shares,
    /// so a mouse with more than three of them keeps its drag.
    ///
    /// `otherMouseDown:` reports the fourth button as `Middle` — bit 2 here —
    /// while the OS has it on bit 3. Comparing the masks as they come would
    /// find bit 2 empty and let go mid-gesture.
    #[test]
    fn a_button_past_the_third_holds_the_middle_bit() {
        let middle = button_bit(MouseButton::Middle);
        for os in [0b100, 0b1000, 0b10000, 0b1 << 20] {
            assert_eq!(
                buttons_owed_release(middle, os),
                0,
                "a button held on the OS mask {os:b} read as let go",
            );
        }
        assert_eq!(buttons_owed_release(middle, 0b011), middle, "no other button was down");
    }

    /// Nothing pressed here is nothing to settle, whatever the OS reports —
    /// a drag begun in the host's own window is not this view's to end.
    #[test]
    fn a_press_this_view_never_saw_is_not_its_to_release() {
        for os in [0, 0b001, 0b010, 0b100, 0b111] {
            assert_eq!(buttons_owed_release(0, os), 0);
        }
    }

    /// The exit waits for a button the OS holds down, and for a release still
    /// owed after the hardware button is up: its `mouseUp:` is queued, and
    /// paying the exit first would deliver the release after it.
    #[test]
    fn the_exit_waits_for_the_release_it_follows() {
        let left = button_bit(MouseButton::Left);
        assert!(exit_waits(0b001, left), "a drag in progress");
        assert!(exit_waits(0, left), "up at the OS, its mouseUp: still queued");
        assert!(exit_waits(0b001, 0), "a press that was never ours");
        assert!(!exit_waits(0, 0), "nothing held and nothing owed");
    }
}

/// Info:
/// https://developer.apple.com/documentation/appkit/nstrackingarea
/// https://developer.apple.com/documentation/appkit/nstrackingarea/options
/// https://developer.apple.com/documentation/appkit/nstrackingareaoptions
fn new_tracking_area(this: &NSView) -> Retained<NSTrackingArea> {
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::CursorUpdate
        | NSTrackingAreaOptions::ActiveInActiveApp
        | NSTrackingAreaOptions::InVisibleRect
        | NSTrackingAreaOptions::EnabledDuringMouseDrag;

    // SAFETY: `this` is of the correct type (NSView)
    unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            this.bounds(),
            options,
            Some(this),
            None,
        )
    }
}

fn get_drag_position(sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) -> Point {
    let point = match sender {
        Some(sender) => sender.draggingLocation(),
        None => NSPoint::ZERO,
    };

    Point::new(point.x, point.y)
}

fn get_drop_data(sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) -> DropData {
    let Some(sender) = sender else {
        return DropData::None;
    };

    let pasteboard = sender.draggingPasteboard();
    let Some(file_list) = pasteboard.propertyListForType(unsafe { NSFilenamesPboardType }) else {
        return DropData::None;
    };

    let Ok(file_list) = file_list.downcast::<NSArray>() else {
        return DropData::None;
    };

    let files = file_list
        .into_iter()
        .filter_map(|s| s.downcast::<NSString>().ok())
        .map(|s| s.to_string().into())
        .collect();

    DropData::Files(files)
}

fn on_event(this: ViewRef<BaseviewView>, event: MouseEvent) -> NSDragOperation {
    let event_status = BaseviewView::trigger_event(this, Event::Mouse(event));
    match event_status {
        EventStatus::AcceptDrop(DropEffect::Copy) => NSDragOperation::Copy,
        EventStatus::AcceptDrop(DropEffect::Move) => NSDragOperation::Move,
        EventStatus::AcceptDrop(DropEffect::Link) => NSDragOperation::Link,
        EventStatus::AcceptDrop(DropEffect::Scroll) => NSDragOperation::Generic,
        _ => NSDragOperation::None,
    }
}
