# Carried patches against upstream

What the two forked crates change from their crates.io releases, patch by
patch, and, last, where `plugin-own-window`, which has no upstream, came from.
The text is harmonigraph's record of its forks, moved here with the code.
Issue numbers such as #1551 are harmonigraph's (yan-h/harmonigraph), as are
`docs/archive/` and `tools/`; `ci.sh` is this repository's.

## baseview

- **Upstream base**: `baseview 0.1.4` from crates.io (the version
egui-baseview 0.3 pins;
wired in via `[patch.crates-io]`, here and in each project, which covers both a direct dependency and egui-baseview's).
- **Pruned to the macOS platform** (#1810): the X11 and Windows platforms, their Xlib, xkbcommon, GLX, connection-poller and Win32 wrappers, the macOS OpenGL context with the `opengl` and `uuid` features, and their target dependencies are gone.
The `hitTest:` override is gone with it (its `ViewImpl` method, registration and extern fn), since its one case was the deleted OpenGL subview; otonal's fork deleted it the same way.
- **No-window platform off macOS** (`src/platform/none.rs`, from otonal's prune in yan-h/otonal#1557): no window opens, and everything else compiles, so a Linux cloud session builds the workspace and runs its headless tests.
`open_parented` returns a handle that reports itself closed, `open_blocking` returns at once, and `Window` is uninhabited, so a build closure is never called.
- **Patch** (retired with its file, `src/wrappers/appkit/timer.rs`): registered the macOS
run-loop timer in `kCFRunLoopCommonModes` instead of `kCFRunLoopDefaultMode`.
Default-mode timers stop firing while the run loop is in an event-tracking mode (native drags/resizes, menu tracking), freezing plugin GUIs until mouse release.
The timer was upstream's frame timer, then the display link's watchdog (patch 4), and went with the watchdog when patch 9 stopped frames for a view nobody can see;
the link is added in common modes for the same reason.
- **Patch 2** (4 sites: `src/platform/macos/{window,view}.rs`,
`src/wrappers/appkit/view.rs` + `view/implementation.rs`):
own the mouse cursor properly.
`set_mouse_cursor` used a one-shot `addCursorRect`, which AppKit discards on the next cursor-rect rebuild —
after which the HOST window's rects win, so the plugin's cursor appeared to depend on whatever Bitwig had behind the window.
Now the desired cursor is stored and re-asserted from a `resetCursorRects` override, with `invalidateCursorRectsForView` forcing a rebuild on change.
Because cursor rects only activate once the window becomes KEY (first click), a `cursorUpdate:` override (delivered via the tracking area's CursorUpdate | ActiveInActiveApp options) plus direct application on change-while-hovered were added for the freshly opened window as well.
KNOWN LIMITATION:
in Bitwig the cursor still doesn't take until the plugin window is first clicked (the host's not-yet-key window appears not to deliver tracking/cursorUpdate events);
accepted as cosmetic —
everything is correct from the first click on.
- **Patch 3** (5 files: `src/event.rs`,
`src/wrappers/appkit/notification_center.rs`, `src/platform/macos/view.rs`,
`src/wrappers/appkit/view.rs` + `view/implementation.rs`):
new `WindowEvent::Occluded(bool)` (mirroring winit's event), emitted from an `NSWindowDidChangeOcclusionStateNotification` observer filtered to the view's own window.
On re-expose the view is marked `setNeedsDisplay` and, more to the point, a frame is run right there in the notification rather than left to the next display-link callback (patch 4) —
nothing presents while the window is occluded (no frame runs for it, patch 9), so what is on screen is the drawable from before it was hidden, and the ghost lasts exactly until one frame gets out.
For a view that is not hidden, that frame passes patch 9's test, whose window half (`window_shows`) is the same reading the notification has just reported visible;
a hidden view runs no frame there, and draws once it is shown.
Waiting for the callback made that up to a frame interval:
measured at 11-14 ms under the ~15 ms frame timer the link replaced, and it scales with the frame divisor, so a frame-rate cap makes it worse;
painting from the notification measures under 1 ms. `trigger_frame` takes the handler borrow fallibly to stay safe against a notification arriving mid-frame (that direction only —
`trigger_event` still borrows infallibly and documents its panic), and the handler drops a notification repeating the state the view is already in, which would otherwise buy an off-cadence frame for nothing.
That state is kept per window attachment (#1070):
`window_visible` is an `Option<bool>`,
and a `viewDidMoveToWindow` override (a new `ViewImpl` method registered in the two appkit wrapper files) clears it to `None` and samples the new window,
so a reparented view reports its new window even when both windows read the same.
The view also samples once at open, right after the handler's initial resize event;
`occlusion_ready` holds the override off until then, because the first `viewDidMoveToWindow` runs during parenting before the handler exists.
That is what lets a window that opens occluded deliver its reveal.
The open-time sample reports the state without the re-expose frame.
Together with the egui-baseview patch below, this fixes the outdated ghost image that stayed on screen after tabbing away from the host and back, until the window was clicked.
macOS only;
other platforms never emit the event.
- **Patch 4** (`src/lib.rs`, `src/window_open_options.rs`, `src/window.rs`,
`src/platform/macos/{view,window}.rs`, `src/wrappers/appkit/view.rs` + `view/implementation.rs`, `Cargo.toml`):
drive frames from the view's display link instead of a fixed timer (#1551).
Upstream's timer fired every 15 ms with no relation to vsync, so a frame was ready whenever it was ready and the display asked when it asked;
at 144 Hz in Bitwig that missed about 4% of refreshes, and a timer at twice the refresh rate did not help.
Now `NSView.displayLinkWithTarget:selector:` (macOS 14) calls a new `displayLinkFired:` method once per refresh of whichever screen the view is on,
from the main run loop in common modes, created once the view is parented and invalidated in `close`.
The link retains the view, so a missed `invalidate` leaks it (the plugin's `editor-startup` probe catches that),
and the callback holds its own reference to the view because a frame can close the window and drop the link's.
`Window::set_frame_divisor` draws on every Nth refresh, paced on the link's `targetTimestamp` rather than by counting callbacks, with the next frame due half a refresh short of N ahead so jitter cannot slip it.
A divisor rather than a rate because the link's `preferredFrameRateRange` rounds to divisors of the panel's rate: measured on a 144 Hz panel, 120 ran at 144 and 90 and 60 at 72.
A link stops while its view is hidden or its window minimized or ordered out (measured; it keeps firing while the window is merely covered),
and no frame runs for any of these anyway (patch 9), so nothing stands in for a stopped link.
A 10 Hz watchdog timer did until patch 9, for harmonigraph, whose frames aged its notes and ended an armed take while the editor was open but out of sight.
A plug-in must not leave such work to an open editor's frames: no frame runs for a view nobody can see.
**harmonigraph must not pin patch 9 until its own change lands**:
its input worker ages notes and ends takes only while the editor is shut,
and its frames also carry its `SizeSource`'s host-window duties (re-hiding the host's window) and everything its UI closure drains.
`Window::frame_timing` hands the frame callback the link's timestamps and the divisor it paced the frame at, for egui-baseview's frame log,
and the fire on `Instant`'s clock beside the media clock's (`fired_at`);
`FrameTiming::target_at` puts the target on `Instant`'s clock, refusing one more than 0.1 s from the fire, for the plugin's picture and egui's clock to time the frame by.
The earlier version of this patch made the timer's interval configurable and reported `NSScreen::maximumFramesPerSecond`;
`WindowOpenOptions::frame_interval`, `set_frame_interval`, `display_max_fps` and the interval constants are gone with the timer.
macOS only;
`set_frame_divisor` is a no-op elsewhere, where no window opens, and `frame_timing` returns `None`.
- **Patch 5** (`src/platform/macos/view.rs`): hold the pointer's exit back
while a mouse button is down.
The tracking area carries `EnabledDuringMouseDrag`, so `mouseExited:` arrives mid-drag —
while AppKit goes on sending every `mouseDragged:` and the closing `mouseUp:` to the view the press landed in, wherever the pointer has got to.
Reporting the exit there is what breaks the drag:
a consumer told the pointer is gone stops following it (egui turns `CursorLeft` into `PointerGone`), so a slider dragged a pixel past the window edge let go under the hand still holding it.
The `CursorLeft` is now withheld rather than dropped —
paid from the matching mouse-up, and from the frame tick for the releases that never arrive at all (a press that was never ours, a host that took the release), both gated on `NSEvent::pressedMouseButtons` rather than on a count of the presses this view has seen, which a release delivered elsewhere would leave wrong forever.
Patch 10 adds that it also waits while a reported press is owed its release, so the release always comes first.
`mouseEntered:` clears it and stays silent, so enter and exit remain paired.
macOS only;
the other backends have no drag-time exit.
- **Patch 6** (`src/platform/macos/view.rs`, `Cargo.toml`): report the release
of a button the OS says is no longer held.
A press this view reports is owed a release, and AppKit does not always pay —
a `mouseUp:` handed to a host popup, a modal opening under the finger, a gesture the host takes over —
so the consumer is left believing the button is down forever.
That costs far more than it sounds like, because drag state is global:
egui gates EVERY `ScrollArea` on `dragged_id().is_none()`, so one unreleased press stops the wheel in every scrollable pane at once, with the window focused and the pointer over the pane that will not move.
Patch 5 does not reach it (it waits on an exit that never comes if the pointer never leaves) and neither does a consumer-side guard on lost focus;
both watch the POINTER, and this is the BUTTON.
So the frame tick now compares the buttons the view has reported pressed against `NSEvent::pressedMouseButtons` —
the same authority Patch 5 already trusts —
and synthesises the missing `ButtonReleased`.
A stuck button must read stuck across two ticks (the hardware goes up before a queued `mouseUp:` is dispatched, so one tick cannot tell a fast click from a lost release), a late real `mouseUp:` is then swallowed so a bit yields one release however it is paid, and every button above the first two shares a bit because `otherMouseDown:` reports them all as `Middle` —
so two of THOSE held at once are one bit between them, the second release is dropped as a duplicate, and a stuck one behind an already-released sibling is not repaired.
Neither reaches the consumer, whose button is a bool that the first release has already put up.
Real drags are untouched:
through one the button reads down.
`ci.sh` runs the unit tests over the mask arithmetic.
macOS only.
- **Patch 8** (`src/platform/macos/view.rs`, `Cargo.toml`, `tests/host_keys.rs`; from otonal's fork): send unhandled keys to the host's view.
A key-down, key-up or modifier change the handler returns `Ignored` for goes to the original parent `NSView`, held weakly, rather than to `super`;
`super` takes it only for a view with no parent (an owned window), or once that parent no longer loads.
Once the view moves into a window of its own, that window's responder chain ends without reaching the host, so `super` would drop the key.
`tests/host_keys.rs` (no harness: AppKit needs the main thread) sends Cmd-S through real native views while docked, in their own window and redocked, and checks the host gets each ignored event once and no captured one;
`cargo test -p baseview` runs it, so `ci.sh` does on macOS; elsewhere `ci.sh` only builds it.
macOS only.
- **Patch 9** (`src/platform/macos/view.rs`, `tests/unseen_frames.rs`, `Cargo.toml`; from otonal's fork): run no frame for a view nobody can see.
`trigger_frame` returns at once unless the view is not hidden, has no hidden ancestor, and is in a window whose `occlusionState` contains `Visible` (`is_visible`):
a view hidden, or in a window covered, minimized, ordered out or on another Space, runs nothing, not even the settles of patches 5 and 6.
The window half is one function, `window_shows`, which patch 3's notification sample reads too.
Asked of AppKit on every frame rather than kept from patch 3's notification, so it cannot go stale when a notification is lost;
a covered window's link still fires on every refresh, so the test is three AppKit calls and nothing more.
Ahead of the settles because an unseen view has no gesture to settle: what it owes then is paid once it shows, before anything is drawn from it,
a withheld exit on the first frame and a stuck button on the second, since a release needs two settles to be confirmed.
Events still reach the handler while the view is unseen, and the first frame once it shows draws what they changed:
the reveal's (patch 3) for a window back from occlusion, the next link callback once the link resumes for a view un-hidden in a visible window.
It retires patch 4's 10 Hz watchdog and with it patch 1's timer.
`tests/unseen_frames.rs` (no harness, as `host_keys.rs`) sends `displayLinkFired:` by hand to a view in a window that is not on screen, and to one in no window, and checks that neither runs a frame;
a frame running for a view that can be seen is tested in `tests/unseen_notice.rs` (patch 11), which sets what the window's occlusion reads as through the `testing` feature, since a window reads as visible only once the window server says so, later and not reliably on a CI runner.
macOS only.
- **Patch 10** (`src/event.rs`, `src/platform/macos/view.rs`, `tests/press_location.rs`, `Cargo.toml`): a press and a release carry where they happened, and a withheld exit waits for the release it follows.
`MouseEvent::ButtonPressed` and `ButtonReleased` have a `position`, in the logical coordinates `CursorMoved` reports.
Upstream's carried none, so the consumer put a button where it last saw the pointer, and that goes stale:
`mouseMoved:` arrives only while the app is active (`ActiveInActiveApp`), so a click into the editor from another app (`acceptsFirstMouse:` says yes) landed on whatever was under the pointer when it last left.
`mouseDown:`, `mouseUp:` and their right and other counterparts fill it from the event's own location, through the one conversion (`view_point`) every move, drag and wheel goes through;
a release outside the view, the end of a drag out of the window, carries that outside point.
Patch 6's synthesised release has no event, and carries where the pointer is now (the window's `mouseLocationOutsideOfEventStream`);
a settle runs only for a visible view, which is in a window, so it never lacks one.
The wheel keeps its move ahead, gated on the view's bounds, since its event has no position.
Patch 5's exit now waits for an owed release as well as for a button the OS holds (`exit_waits`), at `mouseExited:` and at every settle:
a frame between the hardware button going up and its queued `mouseUp:` being dispatched otherwise paid the exit ahead of the release.
A release that never comes is repaired by Patch 6 within two settles, and the exit is paid in the same settle.
`tests/press_location.rs` (no harness, as `host_keys.rs`) sends a `mouseUp:` no press owes, then a press and a release outside the view with no move before either, and checks the first is not reported and the other two carry their points;
a unit test beside the mask tests checks `exit_waits`.
macOS only.
- **Patch 11** (`src/window_open_options.rs`, `src/platform/macos/view.rs`, `src/wrappers/appkit/view.rs` + `view/implementation.rs`, `src/lib.rs`, `Cargo.toml`, `tests/unseen_notice.rs`; for harmonigraph): tell the app when its view stops being seen.
Since patch 9 no frame runs for a view nobody can see, so work an app finishes from its frames has no chance to finish as the view goes unseen:
harmonigraph ends an open host parameter gesture from a pass, and one begun just before its editor was hidden stayed touched in Bitwig until the editor was seen again.
`WindowOpenOptions::with_unseen_notice` takes an `UnseenNotice`, an `Fn()`.
"Seen" is patch 9's frame test, `is_visible`, with one owner: `check_seen` asks it, keeps the answer in `seen`, and tells the app when it turns from seen to not seen.
It is asked at every event that can change it: the occlusion notification, `viewDidHide` and `viewDidUnhide` (new `ViewImpl` methods, sent for an ancestor too), a move to another window or to none, and the frame gate on every link callback the frame divisor lets through, which repairs a lost notification up to `divisor` refreshes late while the link fires.
Never at open (`seen` starts false and the open samples it), never on a reveal, never twice for one loss, and not once the window is closed, whose own removal from its window would otherwise read as one.
It is never called inside the window handler's borrow: a loss found while a frame or an event holds it (`pay_unseen` finds the borrow taken) is owed in `unseen_owed` and paid as `trigger_frame`, `trigger_event` or `trigger_deferrable_event` lets go, so the app may call its host, whatever the host does to this window in return.
A loss found and undone within one borrow (the view taken out of a window and put back in a visible one during a frame) is still told, and nothing is told once the window is closed, a loss owed from inside the closing borrow included.
`pay_unseen` calls a clone of the notice, so a host destroying the editor from inside the call cannot free the closure while it runs.
Otherwise it runs synchronously inside AppKit's own hide and move callbacks, so it can arrive inside the app's own calls that hide the view or move it between windows: a view moved into a window not yet on screen reads as not seen, so a plug-in moving its editor into a window of its own is told at the move.
Unprotected: a notification lost while no frame runs (a minimized or ordered-out window, whose link stops), which is not caught until an event asks again;
and a host that destroys the editor synchronously from inside a host call the notice makes, past what the app guards itself.
No notice set, nothing changes; otonal sets none.
The `testing` feature (native tests only; no plug-in enables it) lets a test set what a window's occlusion state reads as (`baseview::testing::set_window_shows`), since a window reads as visible only once the window server says so, which a test process may never see.
`tests/unseen_notice.rs` (no harness, as `host_keys.rs`), which `ci.sh` runs with that feature, checks one call each for the view hidden, an ancestor hidden, the window occluded by notification, the window occluded with the notification lost and the next link callback finding it, and the view taken out of its window;
none at open, on a reveal, on a move back, for a repeat, or at close;
and that a loss found inside a frame, inside an event `trigger_event` delivers and inside one `trigger_deferrable_event` delivers is told after each lets go, by a notice that sends the view an event through `trigger_event`, whose infallible borrow would panic inside the handler's.
- **Upgrade**: a port onto this tree, not a re-vendor.
Start from a fresh comparison with the versions then published.
The [last one](docs/archive/gui-cohort-feasibility.md) is archived as a dated result (2026-09-19, re-evaluated 2026-10-21):
its disposition table stops at Patch 6, so dispose of later patches from their entries here.
Patch 7 (#1692's re-pace of a changed divisor, with `FrameTiming::woken`) was retired whole in #1747, with the idle pacing it served;
a divisor set from a frame takes effect after the one frame the link has already scheduled.
baseview 0.4 drives frames from a display link too, but pauses it while idle and has no divisor, visibility test or frame timing.
Take from a newer baseview only the macOS changes wanted,
keeping the cursor-rect ownership patch, the occlusion-event patch with its per-attachment sampling, the display link in common modes with its divisor and frame timing, no frame for a view nobody can see with its unseen notice, the withheld pointer exit, positioned button events, the synthesized release for a stuck button, the host's unhandled keys, and the no-window platform off macOS.
- **Upstreaming**: declined (2026-07-19).
Kept as a local patch rather than proposed upstream.

## egui-baseview

- **Upstream base**: `egui-baseview 0.3.0` from crates.io (the RustAudio
fork used by nice-plug).
- **Pruned to the wgpu renderer on macOS** (#1810): the OpenGL renderer and its `opengl` feature, the `x11` features of copypasta and egui-wgpu, the examples and the upstream CI workflow are gone.
The conditions constant on macOS are folded:
cut, copy and paste take Cmd alone, and the modifier translation and the wheel's flip take their macOS branches.
Off macOS the clipboard is copypasta's no-op context, since no window opens there.
- **Patch 1** (2 call sites, `src/window.rs`): after a `Queue::resize()`,
the window resize triggered by the physical-size change passed physical pixels to `Window::resize()`, which takes logical points —
on scaled displays the window/view ended up `pixels_per_point` times too large (2x-zoomed, bottom-left-anchored content on Retina).
Convert with `logical_size_for` at both call sites (build path and `on_frame`), at the native scale.
- **Patch 2** (1 site, `src/window.rs` `on_frame`): texture deltas (font
atlas rebuilds after `set_fonts`, new images) are only uploaded inside `render()`, but frames with no repaint due skip `render()` and drop the deltas permanently —
glyph coordinates then point into a stale atlas (scrambled text).
Force a render whenever `textures_delta` is non-empty.
Only visible for apps that throttle repaints;
always-repaint apps mask it.
Since Patch 22 every pass renders, so no delta can outlive its pass and the forcing term is gone.
- **Patch 3** (`src/window.rs` + the renderer): stale-frame ("ghost")
recovery after window occlusion.
`Renderer::render` now returns whether a frame was actually presented;
a skipped present (occluded window, outdated/lost surface —
the wgpu renderer's early-return paths) no longer consumes the repaint request, so rendering retries instead of freezing on the last presented frame (Patch 17 bounds how often once the refusal is not transient).
The new `baseview::WindowEvent::Occluded(false)` (see the baseview patch above) schedules an immediate repaint so the first frame after re-expose is fresh.
- **Patch 4** (2 sites, `src/renderer/wgpu/renderer.rs` `render`): flush
staged uploads on the surface-not-available early returns.
`render` uploads egui's per-frame vertex/index/texture data (via `update_buffers` /`update_texture`, i.e. `queue.write_*`) BEFORE it acquires the surface texture;
those staging buffers live in wgpu's pending writes and are only reclaimed by a `submit()` (through wgpu-core `pre_submit`).
The Occluded/Timeout and Suboptimal/Outdated/Lost arms returned WITHOUT submitting, stranding that frame's staging buffers.
Combined with Patch 3 (a skipped present retried every timer tick, before Patch 17's backoff), a backgrounded plugin window re-ran `render` ~66×/s and accumulated staging buffers into the *gigabytes* within minutes —
the memory dropped instantly on refocus, when the next presented frame's `submit` finally drained the backlog.
Fix:
each early return now submits `user_cmd_bufs` + the upload encoder (no drawable acquired, so nothing presents —
the window stays frozen while hidden, which is expected —
but pending writes drain and `maintain` runs every tick, so memory stays flat).
Root-caused from a 26 GB balloon while tabbed away from Bitwig.
- **Patch 5** (1 site, `src/window.rs` `on_frame`): make delayed repaints
actually come due.
The repaint deadline was recomputed as `now + repaint_delay` on *every* tick that painted nothing, and egui rebuilds `repaint_delay` from scratch each pass (reset to `MAX` in `begin_pass_repaint_logic`, then the min of that pass's requests) while the UI closure runs on every tick, painting or not.
So a steady `request_repaint_after(N)` re-based the deadline on each tick, and for any N longer than the tick interval (then ~15 ms, the macOS frame timer; now one display refresh) `now` never caught up:
the deadline receded forever and the window painted nothing until an input event or a texture upload forced it.
Every delayed repaint was silently dead —
the idle poll included, which went unnoticed because the plugin shell then requested a repaint whenever it drained MIDI, and an idle window has nothing to show anyway.
Fix:
keep the EARLIEST pending deadline rather than overwriting it, and schedule the next one from the instant a frame actually painted instead of leaving it unset (clearing it to `None` cost a whole tick, so every capped interval ran one tick long).
Found while adding the System pane's frame-rate cap, which was first built on exactly this mechanism and did nothing at all without the fix;
the cap is now baseview's frame divisor (Patch 6).
Since Patch 22 no pass runs without drawing, so only the second half is left: the deadline is set by the pass that draws, from its frame time.
- **Patch 6** (`src/window.rs`): pass baseview's frame divisor through, so the
app can pace its own window.
`Queue::set_frame_divisor` sets the app's chosen rate from inside a frame (applied after the frame returns, where the `Window` is reachable —
the same deferred shape `resize` already uses).
This is what makes a frame rate setting enforceable:
egui's `request_repaint_after` cannot do it, since egui keeps the SMALLEST delay requested in a pass and any zero-delay `request_repaint` (input event, hover animation, a sounding voice) also forces the next pass to zero —
so a cap built on it evaporates exactly when the UI is busy.
Nor can the two be combined: a repaint delay of one frame skips every frame that arrives a hair early, halving the rate (#1551).
`Queue::frame_timing` hands the frame its link timing from baseview patch 4, so the app can time its content by the refresh rather than by when the callback ran,
and egui's own `input.time` is the link's target too (never going back past the last one), so egui's animations step with the picture (#1562).
Before #1551 this plumbed a configurable frame timer interval and the display's maximum rate instead.
- **Patch 7** (2 lines, `src/renderer.rs` + `src/lib.rs`): re-export
`WgpuSetup` alongside `GraphicsConfig`.
`WgpuConfiguration` was already public, but its `wgpu_setup` field cannot be matched without the enum, so there was no way to reach the `device_descriptor` hook and request an extra device feature (we ask for timestamp queries, for the overlay's GPU-time row).
Pure re-export;
no behavior change.
- **Patch 8** (`src/renderer/wgpu/renderer.rs`, `src/window.rs`): measure the
frame's other two halves and hand them back through `Queue` —
`tess_ms` (time in `egui::Context::tessellate`) and `draw_gpu_ms` (GPU time from paint-callback preparation through egui's composite, lattice 3D included, via a timestamp query pair;
named `egui_gpu_ms` and bracketing only egui's own pass before #1074).
Both sat in blind spots:
tessellation runs after the update closure returns, so it is neither the app's own frame time nor GPU time, and a wgpu paint callback's timer brackets only ITS passes, never the 2D UI around them.
A frame cost could live entirely in either and read as zero everywhere.
Since #1074 the opening sample is the beginning of a 1x1 pass placed before `update_buffers`, so callback preparation is inside the bracket,
and the closing sample is the END of egui's render pass, so the bracket cannot close before the composite finishes.
The former tail pass, whose beginning closed the bracket, could read reversed against egui's own beginning;
the new pair measured 0 of 60 reversed on Metal.
Queue-staged uploads and callback-owned command buffers stay outside it.
Readback is a four-step cycle polled with `PollType::Poll`, never `Wait` —
blocking for the number would stall the pipeline being measured.
The resolve waits for `on_submitted_work_done` and goes in its own submission:
resolved with the frame, an M5 Pro under macOS 27 read the closing slot before its stamp landed on 98 of 100 frames,
and the slot kept the last frame's value, so nearly every pair came out reversed and was dropped.
A repeated opening stamp is refused for the same reason.
The cycle duplicates `harmonigraph-render`'s lattice `GpuTimer` (`gpu_timer.rs`), which differs only in where its closing stamp goes;
a fix to either belongs in both.
- **Patch 9** (`src/renderer/wgpu/renderer.rs`, `src/window.rs`): split the
upload reading, then fix what the split exposed.
`last_ubuf_ms` times `update_buffers` on its own, because the surrounding `last_upload_ms` also spans creating the command encoder, taking the renderer's write lock, and the MSAA resize —
none of which are uploads, and two of which are places a frame blocks rather than works.
With those apart, the resize guard turned out to fire every frame.
It tested `msaa_texture_view.is_none()`, but `RendererOptions::default()` sets `msaa_samples` to 0 and `resize_and_generate_msaa_view` only fills that view when it is above 1 —
so the view was never `Some`, and every frame reconfigured the surface to the size it already had.
Rebuilding a swapchain per frame cost 0.5-3 ms, wandered on a ~1 s period as GPU load moved, and was unattributable:
it read as "buffer uploads" on the perf overlay while `update_buffers` itself measured 0.3 ms. The term is now gated on `msaa_samples > 1`;
`width`/`height` start at 0, so the first frame still configures.
The condition is `surface_is_stale`, a pure function with its own test (from otonal's fork), which `ci.sh` runs.
- **Patch 10** (`src/renderer/wgpu/renderer.rs` `layer_present` module +
hooks, `Cargo.toml` objc2 deps):
present a resize without the flash (issue #121's fold flicker).
Two artifacts, one layer:
wgpu presents into a `CAMetalLayer` that raw-window-metal adds as a plain (not view-backed) sublayer, synced to the view's root layer from a KVO observer —
so every geometry change it takes carries CA's default quarter-second implicit ease (of the presentation layer only;
the model reports final values, which is why #120's probes saw "nothing animating"), and it takes those changes inside the HOST's transaction, out of reach of any `CATransaction` wrapped around our own resize calls.
A null `actions` dictionary on the layer kills the lookup for every caller.
Second:
a present is decoupled from the transaction carrying the new bounds, so the resize commit shows the old drawable stretched into the new geometry for a frame;
`presentsWithTransaction`, raised for three presents starting with the adopting frame's (`PWT_FRAMES = 3`, drained one per frame from the frame that arms it;
set before the acquire —
the flag is captured there), folds new content and new bounds into one commit.
Costs a main-thread wait per present, so it is not left on.
- **Patch 11** (retired with `pointer_position`): kept the last pointer position when the pointer left,
since a press and a release were sent at it, and a release after `CursorLeft` was dropped, leaving egui believing the button held forever.
baseview's Patch 10 gives both events their own position, so nothing is stored here any more:
a button is delivered at its event's point under the current zoom, behind a `PointerMoved` to that point (`push_pointer_button`), because egui judges click against drag from moves alone and would count a distant release with no move before it as a click.
- **Patch 12** (`src/renderer/wgpu/renderer.rs`, `src/window.rs`):
the same layer as egui-baseview's Patch 10, across the other discontinuity —
a window coming back from occlusion.
Nothing can present while it is hidden, so the layer still holds the frame from before it went away and the compositor puts that back on screen the instant the window is visible again, before this process is told anything.
`Renderer::window_occluded` hides the layer while the window is occluded, so the re-expose composites the host's background instead of a picture that has stopped being true:
a black hole for that frame rather than a lie —
honesty, not invisibility, since one frame is the floor either way (see the baseview occlusion patch).
The layer comes back in `before_present`, from the frame holding a drawable it is about to present, under `presentsWithTransaction` (raised for as long as the layer is hidden) so the two land in one commit.
That gate is the load-bearing part:
a successful acquire proves both that the window is visible —
the acquire refuses an occluded one —
and that there is something to show, so no occlusion event has to be trusted to arrive for the layer to come back, and no frame unhides ahead of content.
A lost `Occluded(false)` (a view momentarily without a window, a host reparenting the plugin) would otherwise leave a permanently blank editor.
- **Patch 13** (`src/renderer/wgpu/renderer.rs`): publish egui's managed font
texture into `CallbackResources` after this frame's texture deltas and before paint callbacks prepare.
Harmonigraph's glyph callbacks bind that GPU allocation directly;
cloning the `wgpu::Texture` handle does not copy its pixels.
This removes the 4.67–24.69 ms whole-atlas mirror upload paid whenever a zoom first reaches a new label size.
- **Patch 14** (`src/renderer.rs`, the renderer, `src/window.rs`): cap the
font atlas at 4096 on devices whose texture limit is larger.
egui fixes the atlas width at the reported limit and doubles its CPU image's height, so an 8192-wide atlas retained 64–128 MiB after walking the lattice's zoom sizes;
a full texture delta and the GPU texture temporarily multiplied that cost into roughly 400 MiB of process memory.
The cap is still eight times the largest glyph Harmonigraph allows, and puts a 64 MiB ceiling on egui's CPU image before its 80% rebuild clears historical sizes.
At the fresh Name size, a full-range zoom reaches 32 MiB on its first sweep and stays there through every return trip;
the largest valid Name size reaches the 64 MiB ceiling once and reuses it too.
- **Patch 15** (`src/renderer/wgpu/renderer.rs`, re-exports): let an editor keep a `SharedGpuContext` across window lifetimes.
Only the instance, adapter, device and queue survive;
each open still creates a surface and an egui renderer with a fresh texture namespace.
The original adapter is checked against the new surface before reuse.
Device-loss callbacks mark a retained setup unusable, and a nonblocking poll at open delivers any pending loss before deciding to reuse it.
Failed renderer creation also clears the retained setup, so a later open can recover with a fresh device.
Creating a fresh instance keeps the previous `new_without_display_handle` descriptor.
Harmonigraph pairs this with its own cache of immutable lattice pipelines, keyed on instance, device identity and output format, so a closed editor does not depend on the driver's compiler cache still being warm when it reopens.
Measured on Metal:
constructing the lattice resources took 2.58 s cold and 49 ms warm;
reusing their compiled handles took 1.4 µs in the headless reopen test.
`retain` and `existing` are public so the plugin can create that device at activation, before any window, and start compiling on it (#1746):
the first open then finds the device retained and the compile done or under way.
Hot-reload builds retain their existing rebuild behavior.
- **Patch 16** (`src/translate.rs`, `src/window.rs`): take every modifier from the event that carries them.
Mouse events wrote only Alt, Shift and `command` (from Ctrl, even on macOS), and Cmd and Ctrl were otherwise toggled per modifier key, which reaches the view only while it has the keyboard.
A Cmd or Ctrl let go after Cmd-Tab, Cmd-Space, Ctrl-arrow or a Cmd-click on the host has its key-up delivered elsewhere, so `mac_cmd` or `ctrl` stayed held until the next whole press inside the editor.
egui reads either as the zoom modifier on a wheel, so every wheel became a zoom:
every settings `ScrollArea` stopped, while the pictures, which also read `zoom_delta`, went on zooming.
That is the symptom of #501's stranded drag with no drag at all, so `end_stranded_drag` had nothing to end and the Console stayed silent.
Mouse and key events now both replace all five fields with the event's own set, Cmd as `mac_cmd` and `command` on macOS;
upstream egui-baseview 0.7 already has this shape.
- **Patch 17** (`src/window.rs`): stop drawing frames for a surface that
keeps refusing to present (#989).
A present refused three times in a row (`REFUSALS_BEFORE_BACKOFF`) is offered another only every 250 ms (`SURFACE_RETRY`) instead of every frame, because an occluded window's acquire is refused rather than waited on, and drawing anyway submitted 60-144 unpresented frames a second against 13-37 ms of GPU work with nothing pacing them —
wgpu reclaims a frame's staged uploads only when its submission completes, so outrunning the device grew the in-flight set into gigabytes within a minute of switching away from the host.
Keyed on the refusals rather than the occlusion event, since wgpu-hal and baseview read occlusion off different windows and can disagree, and the first present that lands (or an `Occluded(false)`) clears the backoff.
baseview runs no frame for a window it reads as occluded (its patch 9), so an occlusion alone no longer arms the backoff;
it still covers that disagreement (a surface wgpu-hal reads as occluded in a window baseview reads as visible), repeated acquire timeouts, and a surface that fails to recover.
Texture deltas need no exemption: every pass renders (Patch 22), so a frame carrying one is never skipped and glyph coordinates never point into a stale atlas.
The same change also runs `free_textures` on all three render exits instead of only past the acquire:
a `free`-only delta on a bailing frame used to drop egui's texture-manager ids permanently, leaking the retired atlas exactly when the backoff above forces a frame through on the strength of consuming that delta.
`pass_due` (Patch 22's, which carries this backoff) and `after_render` are pure functions with their own tests in `egui-baseview`, which `ci.sh` runs.
- **Patch 18** (`src/window.rs`, `src/translate.rs`): one rule for which keys the editor owns and which go to the host (#1433), shared by both plug-ins.
Every key reaches the editor first, and `owns_key` decides from the state the last egui pass left;
every key it does not own goes to the host.
Only a press is owned: every release and every modifier-only event goes to the host as well as to egui,
so the host never loses the release of a key it saw go down, nor a modifier change.
The host does receive the release of a key it never saw go down, as it already does after Enter or Escape ends a field.
The egui-internal claims take the bare chord only, so a host command on the same key with another modifier (Cmd-Shift-C) stays the host's.
A drag in progress owns bare Escape, which egui cancels it on (`interaction::interact`), since its widget's focus no longer keeps the key.
An open memory-tracked popup owns bare Escape, even without widget focus, so closing a combo box does not also send Escape to the DAW (#1659).
A label holding a selection owns bare Cmd-C, which egui turns into its own copy, so the DAW does not copy too (#1608),
and bare Escape, which clears that selection (#1674);
the arrows and Cmd-A, which move it, stay the host's, since a stray click on a label would otherwise take the arrows from the DAW.
A focused text field (`Context::text_edit_focused`, which a `DragValue` being typed into counts as) owns typing and TextEdit's editing keys (`text_edit_owns_key`),
and a listed shortcut it does not use goes to the host.
That table mirrors egui 0.35's TextEdit event handling, `events` and `check_for_mutating_key_press` in `widgets/text_edit/builder.rs` and `CCursorRange::on_key_press` in `text_selection/cursor_range.rs`,
so an egui upgrade re-derives it from them.
Its clipboard chords are any Command chord on C, X or V, as both forks read them.
With no text field focused, the editor owns exactly the shortcuts the app lists with `Queue::set_shortcuts`, matched exactly,
so the editor's undo and redo never reach the DAW while the editor window has the keyboard.
A focused widget of another kind owns no key by its focus: Space and Enter after a click on a button, Tab and Escape, and a slider's arrows go to the host unless listed.
A press the editor owns reaches egui, with its text or its cut, copy or paste (`queue_key`, classified once by `press_sends`);
a press it does not own never does, so one keystroke acts in one place,
and Cmd-X with a label selected goes to the DAW alone.
A release always reaches egui, so a key cannot stay held there when focus or modifiers changed after its press.
It replaces upstream's `KeyCapture` modes, `Queue::set_key_capture` and the `keyboard_types::Key` re-export they used.
`translate_virtual_key` lowercases a character before matching it,
so Shift and Caps Lock letters still produce egui key events.
- **Patch 19** (`src/window.rs` `wheel_delta`): flip a macOS wheel's x only while Shift is up (#1536).
Upstream negates x on every macOS wheel, but macOS has already turned a Shift+wheel into a horizontal delta carrying the vertical wheel's own sign, and egui folds a Shift wheel back to `x + y`,
so the flip reversed every Shift-wheel and with it the lattice's zoom.
Upstream egui-baseview 0.9.0 still flips unconditionally, so a migration carries this forward.
Unchecked on a trackpad: a sideways swipe with Shift held keeps its raw sign too, which is reversed if macOS swaps no axes for a precise scroll.
`wheel_delta` is a pure function with its own tests in `egui-baseview`, which `ci.sh` runs.
- **Patch 20** (`src/window.rs`, `src/renderer/wgpu/renderer.rs`, `src/frame_log.rs`, `Cargo.toml`): time frames against the display (#1551).
`tick_ms` and `render_ms` leave out the drawable wait, which `acquire_ms` reports on its own:
with three drawables, a frame the display paces waits most of a refresh for one in steady state, and a total counting that read as a frame that was always nearly too slow.
The `frame-log` feature (macOS, wgpu) writes every frame's timing to `~/Library/Logs/Harmonigraph/`:
the display link's timestamps and frame divisor from baseview patch 4, the frame's stages, when it submitted, the layer's `maximumDrawableCount`,
and, through the wgpu-hal present observer, the GPU's start and end on the present's command buffer and the drawable's `presentedTime`.
Together they classify a missed refresh as a late link callback, a wait for a drawable, late CPU or GPU work, or a late latch.
A frame that runs no UI pass (Patch 22) still logs a tick row, with nothing submitted, so a log shows what deciding that cost.
`frame_log::event` is public so the plugin can add its own rows on the same clock and frame (#1670).
Built only on request (`--features frame-log` on the plugin);
`tools/frame-log-summary.py` reads the log.
- **Patch 22** (`src/window.rs`, `src/frame_log.rs`): decide whether a frame runs the UI before running it (#1676, #1747).
Upstream ran the UI pass on every frame to find out whether it should draw, so an idle editor waiting out its 50 ms poll ran it on every refresh: 107 ms per second of Bitwig's main thread at 144 Hz.
Now the display link runs at the app's chosen rate (Patch 6) and `on_frame` does the host-frame duties first, measuring the gap since the last frame and asking the `SizeSource`, then returns before the UI unless a pass is due (`pass_due`):
something asked for one, or the frame lands past the deadline the last pass left.
What asks: any event (`on_event`) and a `PassRequest`, which another thread holds and sets when it changes what the UI shows (the plugin's input worker, on a note or on sound after silence); the window's events set the same flag.
The deadline and the frame time it is compared with are both on the frame clock (`frame_at`: the link's target refresh, or now for a frame the link did not ask for), so the comparison is between two refreshes rather than two moments callbacks ran at;
a frame at or past it is due, and where a deadline falls exactly on a refresh (the poll at 60 and 120 Hz) and the clocks put it a hair either side, the frame that loses runs nothing, so the tie costs no pass (#1708).
Every pass that runs renders.
While Patch 17's backoff is armed, its retry decides alone, a request included, so a thread asking on every change cannot reopen #989's unpaced submits; the request stays set for the retry's pass.
A size from the `SizeSource` runs that frame's pass whatever the backoff says, since the source hands each size over once and only a pass applies it.
`Queue::longest_tick_gap` hands the app the longest gap between frames since its last pass, measured on every frame, for its stall check (`TickGaps`).
A frame the link asked for ends a gap and counts it; one it did not ask for counts nothing and starts the measurement afresh.
That frame is now the reveal's, run from the occlusion notification, and the gap it would end is the time the window spent unseen, since no frame runs then (baseview patch 9);
read off the frame rather than off `Occluded(true)`, which can be lost.
Until baseview's watchdog went, the same rule kept the watchdog's slow frames from reading as stalls.
The time a view spent hidden in a visible window (no occlusion event), or covered when the reveal's notification was lost, still reads as one gap: a wrong log line, accepted.
It replaces Patch 21 (#1692, #1707), which slowed the link's divisor to land on the frame the idle poll drew on, and is retired whole with baseview Patch 7:
`paced_divisor`, `PACE_MARGIN`, `MAX_FRAME_GAP`, `RESIZE_HOLD`, the restore of the chosen divisor on every event and the earliest-deadline keep for undrawn passes all went with it.
- **Patch 23** (`src/window.rs`, the renderer; from otonal's fork): one owner for a size a surface can be made at.
`SurfaceSize` wraps a `PhySize` whose sides are neither zero nor past the device's texture limit (`Renderer::max_texture_side`, the device's own, not the font atlas cap of Patch 14).
Its checking constructors take a physical size, or a logical size at the native scale through baseview's `WindowInfo::from_logical_size`, the arithmetic its `Resized` event reports with;
a non-finite or non-positive size or scale saturates to zero or `u32::MAX` in that cast, which the check refuses.
The window holds one and `Renderer::render` takes one, so the renderer has no size check of its own.
A size from the `SizeSource`, an `InnerSize` command, a `Queue::resize` (recorded in the pass and applied after it, as the frame divisor is) and a `Resized` event go through it;
one it refuses is dropped and the window keeps its size, where the `.max(1.0)` conversions they replace clamped only zero.
The opening size, which has nothing to fall back to, is clamped into 1 to the limit on each side instead.
The device limit is read only when a size arrives.
`SurfaceSize` and `logical_size_for` have their own tests.
- **Patch 24** (`src/window.rs`; from otonal's fork): egui's zoom factor in every points conversion, as egui-winit does.
The screen rect and `InnerSize` commands use the native scale times `Context::zoom_factor`, pointer positions and pixel wheel deltas (Patch 28) the zoom alone, being logical already, and `render` tessellates at the pass's `pixels_per_point`;
physical sizes, logical host sizes and the viewport's `native_pixels_per_point` stay native.
`points_per_pixel` is computed from the two rather than stored.
Unzoomed, nothing changes; zoomed, upstream laid the UI out for a window larger than its own and missed its pointer.
The pointer's last position is kept in logical pixels and converted with the zoom of the event delivered at it, so a press after a zoom change with no move between lands under the pointer.
A zoom takes effect only where a plug-in sets the factor itself:
egui's keyboard zoom (Cmd-plus, Cmd-equals, Cmd-minus) never fires, since neither key table here produces `Plus`, `Equals` or `Minus`,
and Cmd-0, which egui reads as a reset to 1, reaches egui only where a plug-in lists it as a shortcut (Patch 18), and otherwise goes to the host alone.
- **Patch 25** (`src/window.rs`; from otonal's fork): a focus change also sets `RawInput::focused` (`update_focus_input`), beside the viewport's `focused` and the `WindowFocused` event.
egui's input state reads that field, and ends a pane drag on focus loss by it.
- **Patch 26** (`src/translate.rs`, `src/window.rs`; from otonal's fork): a key whose logical value names no egui key falls back to the physical key it sits on (`translate_key`), as egui-winit does.
AppKit reports ⌥A as the "å" it types, so without it ⌥A reached egui as no key and matched no listed shortcut.
Both the key event egui is sent and the shortcut match use it.
- **Patch 27** (`src/window.rs`; for harmonigraph): `EguiWindowSettings::with_unseen_notice` hands an app's `UnseenNotice` to baseview (its patch 11), which owns the test and the call.
- **Patch 28** (`src/window.rs` `scroll_in_points`): a precise scroll divides by egui's zoom alone (#2).
baseview forwards AppKit's `scrollingDeltaX/Y`, which are in points, the pointer's unit; Patch 24 divided them by the backing scale as well, as upstream did,
so every trackpad scroll moved half as far on a 2x display. winit, behind egui-winit, gives the same delta.
A line scroll is unchanged.
- **Upgrade**: a port onto this tree, not a re-vendor.
Start from a fresh comparison with the versions then published.
The [last one](docs/archive/gui-cohort-feasibility.md) is archived as a dated result (2026-09-19, re-evaluated 2026-10-21):
its disposition table stops at Patch 17, so dispose of later patches from their entries here.
The egui 0.36 publication trigger is met and investigated,
and egui-baseview 0.9.0 has been re-evaluated since (#1529);
that result recommends retaining the product stack until the need-driven trigger it names.
Published egui-baseview 0.7.2 replaces the Queue API and some old patch topologies,
so do not mechanically reapply every numbered patch.
The map records retained obligations,
including pre-input host-size adoption,
beyond this numbered inventory,
and the toolchain and wgpu-hal costs of a future migration.
- **Upstreaming**: declined (2026-07-19).
Kept as local patches rather than proposed upstream.

## plugin-own-window

- **Source**: no upstream. It replaces the two plug-ins' copies of the editor's own window:
otonal's `crates/otonal-plugin/src/editor/pop_out.rs`, read at otonal `d22d5533` (branch `switch-to-egui-plugin-window`),
and harmonigraph's `crates/harmonigraph-plugin/src/editor/pop_out.rs`, read at harmonigraph `5a1fce2c` (branch `claude/no-work-left-to-frames`).
otonal's was ported from harmonigraph's at harmonigraph `b154fe82`; which side each behaviour takes was settled in yan-h/otonal#1673 ("The own window, compared").
- **From both, unchanged**: the window's level, Spaces and stacking behind other apps (`front_changed`, `tuck`, `settle`), the host's window concealed rather than hidden, moves deferred to the run loop, and the close button telling the host rather than closing.
- **From harmonigraph's**: one object per open editor, owned by the editor handle, with weak references elsewhere and its cleanup on drop;
the main-thread rule held by `dispatch2::MainThreadBound` rather than a `Mutex` and `unsafe impl Send`;
the latch that tells the host of a close click once;
no close button where there is no host to tell.
- **From otonal's**: the minimum and maximum size given by the shell (at `open`, then at every frame through `take_size`'s `Limits`), where harmonigraph's had a fixed minimum;
`is_out`, for a host `set_size` arriving while the view is out;
a move out before a pending dock was seen owes the host no resize;
a pending look for an app's windows called off at a dock;
a resize the plug-in asks for keeps the window's top-left corner, computed in one frame update.
AppKit's own `setContentSize:` kept the top-left too where measured (this repository's tests, macOS 26 and 27), but nothing documents it,
so for harmonigraph, whose copy left it to AppKit, this is very likely no visible change.
- **In neither**:
  - The agent's Objective-C class is built at run time under a process-unique name. Both plug-ins link this crate into one host process, whose class names are shared.
  objc2 0.6.4's `define_class!` names a class `module_path!()::Name` plus the crate version, and when a class of that auto-generated name already exists it does not panic:
  `define_class` in `src/__macro_helpers/define_class.rs` takes the `else` branch of `ClassBuilder::new` and returns `AnyClass::get(c_name)` ("we assume that the existing class is the one that we want", lines 248-297; it panics only for a name given with `#[name = ...]`).
  The second plug-in would then use the first one's class, with the first build's methods and instance layout.
  - The shell is told of each move synchronously, before the view moves (`Move::Out` with the window's content size, and `Move::Back` at a dock or a close while out), so what it keeps of the editor's size, a save's among it, is right without waiting for a frame, which does not run while the editor cannot be seen.
  harmonigraph's copy wrote its saved state from inside `pop` and `dock`; otonal's told its shell nothing until the next frame.
  - A deferred close carries the window clicked and is told only while that window is still the editor's; and a move withdrawn by `want` has its queued `apply:` cancelled, so at most one is queued.
  Harmonigraph's copy cancelled a pending close at a dock; otonal's did neither, and left a withdrawn `apply:` queued in both.
  A click queued before a dock is honoured; one queued behind a pending dock is dropped.
  - `resize` returns the content size AppKit applied, for the editor to take, rather than a success flag.
  - The window reopens where it was, at the editor's size within the limits, keeping the remembered top-left corner (decided by Yan, tracked in yan-h/egui-plugin-window#12).
  Both copies meant to, but set the size between `setFrameUsingName:` and `setFrameAutosaveName:`, and naming the frame restores the remembered one again (measured),
  so their window reopened as large as the last own window was, and the editor took that size. The size is now set after the naming.
  - It takes the two views as raw `NSView` pointers rather than either shell's window-handle types, so it depends on neither shell nor on `raw-window-handle`.
- **Tests**: `tests/own_window.rs` (the moves and their notices, the close latch and the order of a click and a dock, a click from a replaced window, a drop with calls pending, the resize's corner, the limits, a reopen over a remembered frame) and `tests/host_keys.rs` (otonal's `host_keys` case for a view in its own window and docked again, with the moves made here).
