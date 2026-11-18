use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use baseview::{
    Event, EventStatus, PhySize, Size, Window, WindowHandle, WindowHandler, WindowInfo,
    WindowOpenOptions, WindowScalePolicy,
};
use copypasta::ClipboardProvider;
use egui::{Pos2, Rect, Rgba, ViewportCommand, pos2, vec2};
use keyboard_types::Modifiers;
use raw_window_handle::HasRawWindowHandle;

use crate::{GraphicsConfig, renderer::Renderer};

/// copypasta's native clipboard, which off macOS would need its `x11`
/// feature; there baseview opens no window, so copypasta's no-op one stands in.
#[cfg(target_os = "macos")]
type Clipboard = copypasta::ClipboardContext;
#[cfg(not(target_os = "macos"))]
type Clipboard = copypasta::nop_clipboard::NopClipboardContext;

#[cfg(feature = "nice-log")]
use nice_plug_core::{nice_error as error, nice_warn as warn};

#[cfg(all(feature = "tracing", not(feature = "nice-log")))]
use tracing::{error, warn};

/// Somewhere to ask, once a frame, whether this window has been resized from
/// OUTSIDE — a plugin host dragging the border of the window the view is
/// parented into, which on macOS arrives as no event at all.
///
/// Answered in logical size, since the window owns the scale factor.
///
/// The point of it is WHEN it is asked: before the frame's input is built. A
/// host resize applied later — from inside the update closure, which is where
/// a plugin would otherwise have to do it — lays the frame out at the size the
/// window has stopped being and presents it into a surface that has already
/// changed, so every frame of a drag is one frame of content that does not
/// fit.
#[derive(Clone)]
pub struct SizeSource(Arc<dyn Fn() -> Option<Size> + Send + Sync>);

impl SizeSource {
    pub fn new(source: impl Fn() -> Option<Size> + Send + Sync + 'static) -> Self {
        SizeSource(Arc::new(source))
    }

    fn take(&self) -> Option<Size> {
        (self.0)()
    }
}

impl std::fmt::Debug for SizeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SizeSource")
    }
}

/// How anything outside the window asks it for a UI pass on its next frame:
/// a thread that has changed what the UI shows, with nothing the window would
/// otherwise notice.
///
/// A frame runs the UI only when something asks for it (see `pass_due`): an
/// event, a size from [`SizeSource`], the repaint deadline the last pass left,
/// or this. A change made on another thread reaches none of the others, so
/// without it the window sees the change at the UI's next deadline. The
/// window's own events ask through the same flag.
///
/// One flag however many ask: a pass shows every change made before it. While
/// the surface is refusing to present, a request waits for the surface's next
/// retry like everything else, so asking on every change cannot submit frames
/// faster than a refusing surface drains them (#989).
#[derive(Clone, Debug, Default)]
pub struct PassRequest(Arc<AtomicBool>);

impl PassRequest {
    /// Run the UI on the window's next frame. Cheap enough to call from any
    /// thread on every change: one store.
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether a pass has been asked for since the last take, leaving it set.
    fn asked(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Whether a pass has been asked for since the last take, clearing it:
    /// the window's own read, once a frame.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct EguiWindowSettings {
    pub title: String,

    /// The logical size of the window
    ///
    /// These dimensions will be scaled by the scaling policy specified in `scale`. Mouse
    /// position will be passed back as logical coordinates.
    pub logical_size: Size,

    /// The dpi scaling policy
    pub scale_policy: WindowScalePolicy,

    pub graphics: GraphicsConfig,

    /// Where a size given to this window from outside arrives, if anywhere.
    /// See [`SizeSource`].
    pub size_source: Option<SizeSource>,

    /// How another thread asks this window for a pass. See [`PassRequest`].
    pub pass_request: PassRequest,

    /// Whom the view tells that it stopped being seen, if anyone: handed to
    /// baseview, which owns the test. See [`baseview::UnseenNotice`].
    pub unseen_notice: Option<baseview::UnseenNotice>,
}

impl EguiWindowSettings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tile(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_logical_size(mut self, size: Size) -> Self {
        self.logical_size = size;
        self
    }

    pub fn with_scale_policy(mut self, scale_policy: WindowScalePolicy) -> Self {
        self.scale_policy = scale_policy;
        self
    }

    pub fn with_graphics_config(mut self, config: GraphicsConfig) -> Self {
        self.graphics = config;
        self
    }

    pub fn with_size_source(mut self, source: SizeSource) -> Self {
        self.size_source = Some(source);
        self
    }

    pub fn with_pass_request(mut self, request: PassRequest) -> Self {
        self.pass_request = request;
        self
    }

    pub fn with_unseen_notice(mut self, notice: baseview::UnseenNotice) -> Self {
        self.unseen_notice = Some(notice);
        self
    }
}

impl Default for EguiWindowSettings {
    fn default() -> Self {
        Self {
            title: String::new(),
            logical_size: Size {
                width: 300.0,
                height: 200.0,
            },
            scale_policy: WindowScalePolicy::default(),
            graphics: GraphicsConfig::default(),
            size_source: None,
            pass_request: PassRequest::default(),
            unseen_notice: None,
        }
    }
}

pub struct Queue<'a> {
    bg_color: &'a mut Rgba,
    close_requested: &'a mut bool,
    shortcuts: &'a mut Vec<egui::KeyboardShortcut>,
    frame_divisor: &'a mut Option<u32>,
    frame_timing: Option<baseview::FrameTiming>,
    longest_tick_gap: Option<Duration>,
    tess_ms: f32,
    draw_gpu_ms: Option<f32>,
    acquire_ms: f32,
    tick_ms: f32,
    render_ms: f32,
    upload_ms: f32,
    ubuf_ms: f32,
    texture_ms: f32,
    encode_ms: f32,
    submit_ms: f32,
    prims: u32,
    verts: u32,
}

impl<'a> Queue<'a> {
    pub(crate) fn new(
        bg_color: &'a mut Rgba,
        close_requested: &'a mut bool,
        shortcuts: &'a mut Vec<egui::KeyboardShortcut>,
        frame_divisor: &'a mut Option<u32>,
        frame_timing: Option<baseview::FrameTiming>,
        longest_tick_gap: Option<Duration>,
        tess_ms: f32,
        draw_gpu_ms: Option<f32>,
        acquire_ms: f32,
        tick_ms: f32,
        render_ms: f32,
        upload_ms: f32,
        ubuf_ms: f32,
        texture_ms: f32,
        encode_ms: f32,
        submit_ms: f32,
        prims: u32,
        verts: u32,
    ) -> Self {
        Self {
            bg_color,
            //renderer,
            //repaint_requested,
            close_requested,
            shortcuts,
            frame_divisor,
            frame_timing,
            longest_tick_gap,
            tess_ms,
            draw_gpu_ms,
            acquire_ms,
            tick_ms,
            render_ms,
            upload_ms,
            ubuf_ms,
            texture_ms,
            encode_ms,
            submit_ms,
            prims,
            verts,
        }
    }

    /// How many primitives and vertices the previous frame uploaded.
    pub fn prims(&self) -> u32 {
        self.prims
    }

    pub fn verts(&self) -> u32 {
        self.verts
    }

    /// Of the uploads, the TEXTURE half.
    pub fn texture_ms(&self) -> f32 {
        self.texture_ms
    }

    /// The renderer's stages, in milliseconds: uploads (which is also where
    /// paint callbacks `prepare`), encoding egui's draw calls, and
    /// finish + submit + present.
    pub fn upload_ms(&self) -> f32 {
        self.upload_ms
    }

    /// Of that, `update_buffers` itself. See the renderer's accessor for why
    /// the two are not the same number.
    pub fn ubuf_ms(&self) -> f32 {
        self.ubuf_ms
    }

    pub fn encode_ms(&self) -> f32 {
        self.encode_ms
    }

    pub fn submit_ms(&self) -> f32 {
        self.submit_ms
    }

    /// Milliseconds the previous frame spent inside the renderer: tessellate,
    /// buffer and texture uploads, encoding egui's pass, submit and present,
    /// less the acquire wait as in [`Self::tick_ms`]. `tick_ms` minus this is
    /// the egui half.
    pub fn render_ms(&self) -> f32 {
        self.render_ms
    }

    /// Milliseconds the previous pass's frame callback took end to end, less
    /// the time it spent blocked waiting for a drawable ([`Self::acquire_ms`]).
    ///
    /// Every other reading is a STAGE; this is the whole of the work. The
    /// wait is left out because it is not work: with three drawables a frame
    /// paced by the display routinely waits most of a refresh for one, and a
    /// total counting that reads as a frame that is always nearly too slow.
    pub fn tick_ms(&self) -> f32 {
        self.tick_ms
    }

    /// Milliseconds the previous frame blocked waiting for the surface.
    ///
    /// Large here with every cost row small means the frame is not slow, it is
    /// early — the display, not the work, is setting the pace.
    pub fn acquire_ms(&self) -> f32 {
        self.acquire_ms
    }

    /// Milliseconds the GPU spent from callback preparation through egui's
    /// composite a few frames ago,
    /// consumed once; None while readback is pending or unavailable.
    ///
    /// The lattice's 3D time is included here and also shown separately in
    /// the overlay. Queue uploads and callback-owned command buffers are
    /// outside this bracket.
    pub fn take_draw_gpu_ms(&mut self) -> Option<f32> {
        self.draw_gpu_ms.take()
    }

    /// Milliseconds the PREVIOUS frame spent tessellating egui's shapes.
    ///
    /// Between the app's own frame time and the GPU's, this is the step that
    /// is otherwise invisible: shapes are cheap to append and expensive to
    /// turn into triangles, and the two happen in different places.
    pub fn tess_ms(&self) -> f32 {
        self.tess_ms
    }

    /// When the display asked for this frame, or `None` for a frame it did not
    /// ask for. See [`baseview::Window::frame_timing`].
    pub fn frame_timing(&self) -> Option<baseview::FrameTiming> {
        self.frame_timing
    }

    /// The longest gap since the last pass that a frame the display asked
    /// for ended, so not the time before a reveal (see `TickGaps`): what an
    /// app's check for a stalled run loop reads. Measured on every
    /// frame, the ones that run no pass included, so a pass that waited out
    /// the idle poll reads the display's own gaps rather than the poll's.
    pub fn longest_tick_gap(&self) -> Option<Duration> {
        self.longest_tick_gap
    }

    /// Run the frame callback on every `divisor`th refresh of the display
    /// from here on; 0 and 1 are every refresh. See
    /// [`baseview::Window::set_frame_divisor`]. Whether a frame runs the UI is
    /// decided apart from this, by `pass_due`.
    ///
    /// Applied once the frame returns (the window isn't reachable from
    /// inside), so calling it repeatedly within one frame keeps only the last
    /// value, and calling it every frame with an unchanged divisor costs
    /// nothing.
    pub fn set_frame_divisor(&mut self, divisor: u32) {
        *self.frame_divisor = Some(divisor);
    }

    /// Set the background color.
    pub fn bg_color(&mut self, bg_color: Rgba) {
        *self.bg_color = bg_color;
    }

    /// Close the window.
    pub fn close_window(&mut self) {
        *self.close_requested = true;
    }

    /// Set the shortcuts the editor keeps from the host while no text field
    /// is focused, matched exactly (`owns_key`). Kept until set again, so an
    /// app whose shortcuts follow its state sets them every frame; a key
    /// arrives between frames, so it meets the last list set. Empty until set.
    pub fn set_shortcuts(&mut self, shortcuts: &[egui::KeyboardShortcut]) {
        self.shortcuts.clear();
        self.shortcuts.extend_from_slice(shortcuts);
    }
}

/// Handles an egui-baseview application
pub struct EguiWindow<State, U>
where
    State: 'static + Send,
    U: FnMut(&mut egui::Ui, &mut Queue, &mut State),
    U: 'static + Send,
{
    user_state: Option<State>,
    user_update: U,

    egui_ctx: egui::Context,
    viewport_id: egui::ViewportId,
    start_time: Instant,
    /// The last time handed to egui, which a frame never goes back past.
    egui_time: f64,
    egui_input: egui::RawInput,
    current_cursor_icon: baseview::MouseCursor,

    renderer: Renderer,

    clipboard_ctx: Option<Clipboard>,

    surface_size: SurfaceSize,
    /// See [`SizeSource`]: polled at the top of every frame, before the input
    /// that the frame is laid out from is built.
    size_source: Option<SizeSource>,
    scale_policy: WindowScalePolicy,
    /// The native backing scale: logical (host) pixels to physical pixels.
    /// egui's own scale is this times its zoom factor; see `points_per_pixel`.
    native_scale: f32,
    bg_color: Rgba,
    close_requested: bool,
    /// When the UI next wants a pass, on the frame clock (see [`frame_at`]):
    /// the frame time of the last pass plus the repaint delay it asked for.
    /// `None` while it asks for none.
    repaint_after: Option<Instant>,
    /// See [`PassRequest`]: another thread's requests, and this window's
    /// events.
    pass_request: PassRequest,
    tick_gaps: TickGaps,
    /// How many frames in a row the surface has refused to present. Reset by
    /// the first one that lands. See [`REFUSALS_BEFORE_BACKOFF`].
    refused_presents: u32,
    /// When the next frame offered to a refusing surface is due, or `None`
    /// while the surface is still being trusted to take one every tick.
    ///
    /// Separate from `repaint_after` because it paces something else:
    /// `repaint_after` is what the UI asked for, this is what the surface has
    /// shown it will accept. See [`pass_due`].
    retry_after: Option<Instant>,
    /// See [`Queue::set_shortcuts`].
    shortcuts: Vec<egui::KeyboardShortcut>,
    /// Tessellation time from the PREVIOUS frame — it is measured inside
    /// `render`, which runs after the update closure, so the closure can only
    /// ever be handed the last one. One frame stale, like every other
    /// after-the-fact measurement here.
    tess_ms: f32,
    /// Likewise for GPU drawing, which lags further still — the timestamps
    /// have to come back from the GPU.
    draw_gpu_ms: Option<f32>,
    /// Likewise for the surface wait.
    acquire_ms: f32,
    /// The whole previous callback, end to end.
    tick_ms: f32,
    /// The renderer half of it, and its stages.
    render_ms: f32,
    upload_ms: f32,
    ubuf_ms: f32,
    texture_ms: f32,
    encode_ms: f32,
    submit_ms: f32,
    prims: u32,
    verts: u32,
}

impl<State, U> EguiWindow<State, U>
where
    State: 'static + Send,
    U: FnMut(&mut egui::Ui, &mut Queue, &mut State),
    U: 'static + Send,
{
    fn new<B>(
        window: &mut baseview::Window<'_>,
        settings: EguiWindowSettings,
        mut build: B,
        update: U,
        mut state: State,
    ) -> EguiWindow<State, U>
    where
        B: FnMut(&egui::Context, &mut Queue, &mut State),
        B: 'static + Send,
    {
        let renderer = Renderer::new(window, settings.graphics).unwrap_or_else(|err| {
            // TODO: better error log and not panicking, but that's gonna require baseview changes
            error!("oops! the gpu backend couldn't initialize! \n {err}");
            panic!("gpu backend failed to initialize: \n {err}")
        });
        let egui_ctx = egui::Context::default();

        // Assume scale for now until there is an event with a new one.
        let native_scale = match settings.scale_policy {
            WindowScalePolicy::ScaleFactor(scale) => scale,
            WindowScalePolicy::SystemScaleFactor => 1.0,
        } as f32;

        let screen_rect = Rect::from_min_size(
            Pos2::new(0f32, 0f32),
            vec2(
                settings.logical_size.width as f32,
                settings.logical_size.height as f32,
            ),
        );

        let viewport_info = egui::ViewportInfo {
            parent: None,
            title: Some(settings.title),
            native_pixels_per_point: Some(native_scale),
            focused: Some(true),
            inner_rect: Some(screen_rect),
            ..Default::default()
        };
        let viewport_id = egui::ViewportId::default();

        let mut egui_input = egui::RawInput {
            max_texture_side: Some(renderer.font_atlas_max_texture_side()),
            screen_rect: Some(screen_rect),
            ..Default::default()
        };
        let _ = egui_input.viewports.insert(viewport_id, viewport_info);

        let initial_size =
            WindowInfo::from_logical_size(settings.logical_size, native_scale as f64)
                .physical_size();

        let mut bg_color = Rgba::BLACK;
        let mut close_requested = false;
        let mut shortcuts = Vec::new();
        let mut frame_divisor = None;
        let mut queue = Queue::new(
            &mut bg_color,
            &mut close_requested,
            &mut shortcuts,
            &mut frame_divisor,
            // The build runs outside any frame the display asked for.
            None,
            None,
            // No frame has been measured yet: tess, draw gpu, acquire, tick,
            // render, upload, ubuf, texture, encode, submit, then the two
            // geometry counts.
            0.0,
            None,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0,
            0,
        );
        (build)(&egui_ctx, &mut queue, &mut state);
        if let Some(divisor) = frame_divisor {
            window.set_frame_divisor(divisor);
        }

        // The window's own size has no other size to fall back to, so it is
        // clamped where a size from elsewhere would be refused.
        let surface_size = SurfaceSize::clamped(initial_size, renderer.max_texture_side());

        let clipboard_ctx = match Clipboard::new() {
            Ok(clipboard_ctx) => Some(clipboard_ctx),
            Err(e) => {
                error!("Failed to initialize clipboard: {}", e);
                None
            }
        };

        let start_time = Instant::now();

        Self {
            user_state: Some(state),
            user_update: update,

            egui_ctx,
            viewport_id,
            start_time,
            egui_time: 0.0,
            egui_input,
            current_cursor_icon: baseview::MouseCursor::Default,

            renderer,

            clipboard_ctx,

            surface_size,
            size_source: settings.size_source.clone(),
            native_scale,
            scale_policy: settings.scale_policy,
            bg_color,
            close_requested,
            repaint_after: Some(start_time),
            pass_request: settings.pass_request.clone(),
            tick_gaps: TickGaps::default(),
            refused_presents: 0,
            retry_after: None,
            shortcuts,
            tess_ms: 0.0,
            draw_gpu_ms: None,
            acquire_ms: 0.0,
            tick_ms: 0.0,
            render_ms: 0.0,
            upload_ms: 0.0,
            ubuf_ms: 0.0,
            texture_ms: 0.0,
            encode_ms: 0.0,
            prims: 0,
            verts: 0,
            submit_ms: 0.0,
        }
    }

    /// Open a new child window.
    ///
    /// * `parent` - The parent window.
    /// * `settings` - The settings of the window.
    /// * `state` - The initial state of your application.
    /// * `build` - Called once before the first frame. Allows you to do setup code and to
    ///   call `ctx.set_fonts()`. Optional.
    /// * `update` - Called before each frame. Here you should update the state of your
    ///   application and build the UI.
    pub fn open_parented<P, B>(
        parent: &P,
        settings: EguiWindowSettings,
        state: State,
        build: B,
        update: U,
    ) -> WindowHandle
    where
        P: HasRawWindowHandle,
        B: FnMut(&egui::Context, &mut Queue, &mut State),
        B: 'static + Send,
    {
        Window::open_parented(
            parent,
            #[allow(clippy::needless_update)]
            WindowOpenOptions {
                title: settings.title.clone(),
                size: settings.logical_size,
                scale: settings.scale_policy,
                unseen_notice: settings.unseen_notice.clone(),
                ..Default::default()
            },
            move |window: &mut baseview::Window<'_>| -> EguiWindow<State, U> {
                EguiWindow::new(window, settings, build, update, state)
            },
        )
    }

    /// Open a new window that blocks the current thread until the window is destroyed.
    ///
    /// * `settings` - The settings of the window.
    /// * `state` - The initial state of your application.
    /// * `build` - Called once before the first frame. Allows you to do setup code and to
    ///   call `ctx.set_fonts()`. Optional.
    /// * `update` - Called before each frame. Here you should update the state of your
    ///   application and build the UI.
    pub fn open_blocking<B>(settings: EguiWindowSettings, state: State, build: B, update: U)
    where
        B: FnMut(&egui::Context, &mut Queue, &mut State),
        B: 'static + Send,
    {
        Window::open_blocking(
            #[allow(clippy::needless_update)]
            WindowOpenOptions {
                title: settings.title.clone(),
                size: settings.logical_size,
                scale: settings.scale_policy,
                unseen_notice: settings.unseen_notice.clone(),
                ..Default::default()
            },
            move |window: &mut baseview::Window<'_>| -> EguiWindow<State, U> {
                EguiWindow::new(window, settings, build, update, state)
            },
        )
    }

    /// Replace the pressed key modifiers with the set an event has sent, all of
    /// them — see `translate_modifiers` for why a partial update leaves the
    /// wheel dead.
    fn update_modifiers(&mut self, modifiers: &Modifiers) {
        self.egui_input.modifiers = crate::translate::translate_modifiers(*modifiers);
    }
}

/// Consecutive refused presents before the loop stops offering one every tick.
///
/// Small on purpose. The refusals this rides through are the transient ones —
/// a surface going Outdated or Lost while a resize is in flight, where
/// retrying on the very next tick is what keeps the drag smooth and is the
/// behaviour [`SURFACE_RETRY`] must not slow down. Refusal that outlasts a
/// few ticks is not transient, and that is what the backoff is for.
const REFUSALS_BEFORE_BACKOFF: u32 = 3;

/// How often a surface that keeps refusing is offered another frame.
///
/// Two things set it. Below the GPU's own frame time it stops bounding
/// anything, since the whole point is to submit slower than the device
/// drains; a refusing surface costs 4 unpresented frames a second here against
/// the 60-144 it used to. Above a few hundred milliseconds it starts being
/// visible in the case the poll exists for — a surface that has quietly
/// become available again without anything saying so — where this interval is
/// how long the window stays frozen.
const SURFACE_RETRY: Duration = Duration::from_millis(250);

/// Whether this frame runs the UI pass, and so draws: every pass that runs
/// renders, so this is decided before the pass rather than after it.
///
/// A pass is due when something asked for it (`asked`: an event or another
/// thread's [`PassRequest`]), or when the frame lands past the deadline the
/// last pass left (`repaint_after`). Every other frame returns
/// before the UI runs, which is what an idle editor's frames are: before this,
/// the pass ran on every refresh to find out whether anything had changed, and
/// cost a still editor 107 ms of Bitwig's main thread a second at 144 Hz
/// (#1676).
///
/// `frame_at` and the deadline are both on the frame clock ([`frame_at`]), so
/// the comparison is between two refreshes of the display's grid rather than
/// between two moments the callbacks happened to run at, which is what made
/// it a coin toss on pass-time jitter (#1703, #1708). A frame at or past the
/// deadline is due. A deadline exactly on a refresh (the 50 ms poll at 60 and
/// 120 Hz) can still fall either side of it by microseconds, but the frame
/// that loses now runs nothing, so the tie costs no pass, only which refresh
/// draws.
///
/// Split out of the frame because it is the whole of the decision and the only
/// part of it a test can reach: the tick around it needs a live window, a
/// surface and a device.
///
/// `retry_after` is the term that is not about what the UI wants. A surface
/// that refuses to present makes the frame one nobody sees. An occluded
/// window's acquire is refused, and baseview runs no frame for a window it
/// reads as occluded; but wgpu-hal reads occlusion off a different window
/// (below), so a frame baseview runs can still be refused, on every tick, for
/// as long as the two disagree. Drawing it anyway is not merely waste: the
/// acquire is ALSO where a presenting frame waits for vsync,
/// and that wait is the only backpressure in this loop. Refused immediately
/// instead of waited on, it leaves the tick free to encode and submit a whole
/// scene every frame while the GPU is still working through the last one.
/// wgpu reclaims a frame's staged uploads when its submission COMPLETES — the
/// poll at the end of `Queue::submit` is non-blocking, so it frees only what
/// has already landed — so submitting faster than the device drains grows the
/// in-flight set without bound: gigabytes within a minute of switching away
/// from the host, handed back all at once when the window comes forward and
/// vsync starts pacing again. Submitting slower than the device drains is what
/// bounds it. The flush in the renderer's `Occluded` arm is a different fix,
/// and only stops a frame that DID draw from stranding its uploads.
///
/// Keyed on the refusals themselves rather than on the occlusion event,
/// because the event decides nothing here and can disagree. wgpu-hal reads the
/// occlusion state by walking its own layer up to a delegate's window, while
/// baseview reads the view's; `Occluded(false)` can be lost or filtered
/// outright — a view momentarily without a window, a host reparenting the
/// plugin — and a window that never paints again is a far worse failure than
/// the memory this saves. The acquire is the one authority both agree on, the
/// same reasoning the renderer's `before_present` uses to un-hide the layer.
/// So a refusing surface is still offered a frame every [`SURFACE_RETRY`], and
/// the first one that presents clears the backoff.
///
/// While the backoff is armed the retry decides alone, a request included: a
/// thread may ask on every change, and honouring that would be the same
/// unpaced submit loop. The request stays set, so the retry's pass draws it.
/// A size from [`SizeSource`] is the one thing that runs a pass at once
/// whatever this says (see `on_frame`): the source hands each size over once.
///
/// Texture deltas need no term of their own, as they did while a pass could
/// run without drawing: they are uploaded only inside `render()`, and every
/// pass now renders, so no delta outlives its pass.
fn pass_due(
    asked: bool,
    repaint_after: Option<Instant>,
    retry_after: Option<Instant>,
    frame_at: Instant,
) -> bool {
    let past = |due: Instant| frame_at >= due;
    match retry_after {
        // A refusing surface is paced by its own clock, never the UI's: the UI
        // asks for a zero-delay repaint on every animating frame, and
        // honouring that, or a request on every change, is the spin the
        // backoff exists to stop.
        Some(retry) => past(retry),
        None => asked || repaint_after.is_some_and(past),
    }
}

/// The instant this frame is drawn for: the display link's target refresh,
/// which a frame the link asked for is on screen by, or `now` for one it did
/// not (a reveal's, any frame off macOS). One clock for egui's
/// time, the repaint deadline and the decision on it.
fn frame_at(timing: Option<baseview::FrameTiming>, now: Instant) -> Instant {
    timing.and_then(|timing| timing.target_at()).unwrap_or(now)
}

/// The gaps between frames, measured on every frame whether it runs a pass or
/// not, for an app's check that its run loop has not stalled
/// ([`Queue::longest_tick_gap`]).
///
/// A frame the display link asked for ends the gap since the last frame and
/// counts it. A frame it did not ask for counts nothing and starts the
/// measurement afresh: that is the reveal's frame, run from the occlusion
/// notification, and the gap it would end is the time the window spent
/// unseen, since baseview runs no frame then, rather than a stall. Read off
/// the frame rather than off `Occluded(true)`, which can be lost.
///
/// What this still reads as one gap is the time a view spent hidden in a
/// visible window, which sends no occlusion event, and a covered window whose
/// reveal notification was lost, so that the link's first frame ends the gap:
/// a wrong log line in an app's stall check, accepted.
#[derive(Default)]
struct TickGaps {
    last: Option<Instant>,
    longest: Option<Duration>,
}

impl TickGaps {
    fn tick(&mut self, at: Instant, linked: bool) {
        if let Some(last) = self.last.filter(|_| linked) {
            let gap = at.saturating_duration_since(last);
            self.longest = Some(self.longest.map_or(gap, |longest| longest.max(gap)));
        }
        self.last = Some(at);
    }

    /// The longest gap since the last take.
    fn take(&mut self) -> Option<Duration> {
        self.longest.take()
    }
}

/// What a finished render leaves behind for [`pass_due`] to read next
/// tick: how many presents have now been refused in a row, and when to offer
/// the surface another frame.
///
/// The other half of the decision, and split out for the same reason — a
/// refusal is only reachable through a real surface, and the counter is the
/// whole of what separates a transient one from occlusion.
fn after_render(presented: bool, refused: u32, now: Instant) -> (u32, Option<Instant>) {
    if presented {
        return (0, None);
    }
    let refused = refused.saturating_add(1);
    let retry =
        (refused >= REFUSALS_BEFORE_BACKOFF).then(|| now.checked_add(SURFACE_RETRY)).flatten();
    (refused, retry)
}

/// The wheel delta as egui should read it.
///
/// macOS reports x with the opposite sign to egui's (a bug that winit has
/// only half fixed, and baseview may share), so x is flipped there. Shift is
/// the exception: macOS has already turned a vertical wheel into a
/// horizontal one by the time it reaches us, with the vertical wheel's own
/// sign, and egui folds a Shift wheel back to `x + y`. Flipping x on top of
/// that reverses every Shift-wheel, which on the lattice reversed the zoom.
fn wheel_delta(delta: egui::Vec2, shift: bool) -> egui::Vec2 {
    if !shift {
        egui::vec2(-delta.x, delta.y)
    } else {
        delta
    }
}

#[cfg(test)]
mod wheel_delta_tests {
    use super::*;

    #[test]
    fn shift_wheel_keeps_its_sign_on_macos() {
        // macOS delivers a Shift + wheel-up as a positive x; egui sums x and y.
        let d = wheel_delta(egui::vec2(1.0, 0.0), true);
        assert_eq!(d.x + d.y, 1.0);
    }

    #[test]
    fn horizontal_scroll_is_flipped_without_shift() {
        assert_eq!(wheel_delta(egui::vec2(1.0, 2.0), false), egui::vec2(-1.0, 2.0));
    }
}

/// The time egui animates a frame at: its [`frame_at`], so egui's own
/// animations step by whole refreshes as the plugin's picture does, however
/// late the callback ran. Never before `previous`, since a frame the link did
/// not ask for, between two it did, is behind the last target, and egui reads
/// time going backwards as a negative frame time.
fn egui_time(previous: f64, start: Instant, at: Instant) -> f64 {
    at.saturating_duration_since(start).as_secs_f64().max(previous)
}

#[cfg(test)]
mod pass_due_tests {
    use super::*;

    /// Link frames whose callbacks ran 0.3–9 ms late animate egui one refresh
    /// apart, and neither a frame the link did not ask for nor one whose
    /// target is not this frame's takes egui's time back.
    #[test]
    fn egui_animates_at_the_refresh_and_never_goes_back() {
        let start = Instant::now();
        let refresh = 1.0 / 144.0;
        let link = |vsync: f64, late_ms: f64| {
            let fired = vsync + late_ms / 1000.0;
            baseview::FrameTiming {
                fired: 500.0 + fired,
                fired_at: start + Duration::from_secs_f64(fired),
                timestamp: 500.0 + vsync,
                target_timestamp: 500.0 + vsync + refresh,
                refresh,
                divisor: 1,
            }
        };
        let now = Instant::now();
        let mut time = 0.0;
        let mut times = Vec::new();
        for (frame, late_ms) in [0.3, 5.1, 0.4, 9.0, 0.6].into_iter().enumerate() {
            let at = frame_at(Some(link(1.0 + frame as f64 * refresh, late_ms)), now);
            time = egui_time(time, start, at);
            times.push(time);
        }
        for step in times.windows(2).map(|pair| pair[1] - pair[0]) {
            assert!((step - refresh).abs() < 1e-6, "stepped {step} s against {refresh} s");
        }
        // The link frames ran a second past `start`, so now is well before the
        // last target, as a reveal repaint between link frames is.
        let unasked = egui_time(time, start, frame_at(None, now));
        assert_eq!(unasked, time, "a frame the link did not ask for");
        // Five seconds ahead is not this frame's, so it falls back to now too.
        let far = baseview::FrameTiming { target_timestamp: 500.0 + 7.0, ..link(1.1, 0.3) };
        let wrong = egui_time(time, start, frame_at(Some(far), now));
        assert_eq!(wrong, time, "a target AppKit got wrong");
    }

    #[test]
    fn draw_gpu_queue_consumes_each_completion_once() {
        let mut color = Rgba::BLACK;
        let mut close = false;
        let mut shortcuts = Vec::new();
        let mut divisor = None;
        let mut completed = Vec::new();
        for sample in [Some(2.0), None, None, None, None, Some(10.0), None] {
            let mut queue = Queue::new(
                &mut color, &mut close, &mut shortcuts, &mut divisor,
                None, None,
                0.0, sample, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0, 0,
            );
            completed.extend(queue.take_draw_gpu_ms());
            assert_eq!(queue.take_draw_gpu_ms(), None);
        }
        assert_eq!(completed, [2.0, 10.0]);
    }

    /// The deadline the plugin's UI leaves while anything is moving:
    /// `request_repaint()`, a repaint delay of zero, from the last frame, a
    /// refresh before this one. Under it a visible window draws every frame.
    fn animating(now: Instant) -> Option<Instant> {
        now.checked_sub(Duration::from_secs_f64(1.0 / 144.0))
    }

    #[test]
    fn a_backed_off_surface_is_not_offered_a_frame_every_tick() {
        let now = Instant::now();
        assert!(pass_due(false, animating(now), None, now));
        // Same frame, same zero-delay request, and the only difference is
        // that the surface has shown it will refuse: the unpaced submit loop
        // is exactly this assertion coming back true.
        let armed = now.checked_add(SURFACE_RETRY);
        assert!(!pass_due(false, animating(now), armed, now));
    }

    #[test]
    fn a_backed_off_surface_is_still_offered_a_frame_each_interval() {
        let now = Instant::now();
        // An idle UI, whose own deadline is an interval away: with the backoff
        // gone this frame runs nothing, so a due retry is the only thing that
        // can explain the pass — which is what makes this measure the retry
        // rather than agree with it.
        let ahead = now.checked_add(Duration::from_secs(1));
        assert!(!pass_due(false, ahead, None, now));
        // Armed one interval ago, so this frame tries the acquire — the offer
        // whose success is the only thing that has to be believed for a
        // window with no `Occluded(false)` to come back.
        assert!(pass_due(false, ahead, now.checked_sub(SURFACE_RETRY), now));
    }

    #[test]
    fn a_transient_refusal_still_retries_on_the_next_tick() {
        let now = Instant::now();
        // A resize's surface goes Outdated for a frame or two. Every refusal
        // below the threshold must leave the backoff unarmed, so the retry is
        // the very next frame and the drag stays smooth — 250 ms of stale
        // content per hitch is exactly what this must not buy.
        let mut refused = 0;
        for _ in 1..REFUSALS_BEFORE_BACKOFF {
            let retry;
            (refused, retry) = after_render(false, refused, now);
            assert_eq!(retry, None);
            assert!(pass_due(false, animating(now), retry, now));
        }
        // One more, and it is no longer transient.
        let (_, retry) = after_render(false, refused, now);
        assert!(retry.is_some());
        assert!(!pass_due(false, animating(now), retry, now));
    }

    #[test]
    fn one_present_ends_the_backoff() {
        let now = Instant::now();
        // However long it has been refusing — this is the recovery that has
        // to work without any occlusion event arriving to announce it.
        let (refused, retry) = after_render(false, u32::MAX - 1, now);
        assert!(retry.is_some());
        assert!(!pass_due(false, animating(now), retry, now));
        let (refused, retry) = after_render(true, refused, now);
        assert_eq!((refused, retry), (0, None));
        assert!(pass_due(false, animating(now), retry, now));
    }

    /// An idle UI's deadline is ahead, so its frames run nothing; an event or
    /// another thread's request runs the next one, unless the surface is
    /// backed off, when it waits for the retry; and a UI that asks for nothing
    /// at all runs none until something asks.
    #[test]
    fn a_frame_runs_a_pass_only_when_asked_or_due() {
        let now = Instant::now();
        let poll = now.checked_add(Duration::from_millis(50));
        assert!(!pass_due(false, poll, None, now));
        assert!(pass_due(true, poll, None, now), "asked");
        let armed = now.checked_add(SURFACE_RETRY);
        assert!(!pass_due(true, poll, armed, now), "asked, backed off");
        let retry = now.checked_sub(SURFACE_RETRY);
        assert!(pass_due(true, poll, retry, now), "asked, at the retry");
        assert!(!pass_due(false, None, None, now), "nothing asked for");
    }

    /// The idle poll at 144 Hz is met by the eighth refresh, not the seventh,
    /// and a deadline of the last frame (a repaint asked for at once) by the
    /// next one.
    #[test]
    fn a_deadline_is_met_by_the_first_frame_at_or_past_it() {
        let set = Instant::now();
        let poll = set.checked_add(Duration::from_millis(50));
        let refresh = Duration::from_secs_f64(1.0 / 144.0);
        assert!(!pass_due(false, poll, None, set + refresh * 7), "48.6 ms");
        assert!(pass_due(false, poll, None, set + refresh * 8), "55.6 ms");
        assert!(pass_due(false, Some(set), None, set + refresh), "a repaint asked at once");
    }

    /// A gap a link frame ends counts, and the time a window spent unseen
    /// does not: the reveal's frame, which the link did not ask for, starts
    /// the next gap rather than ending one. Each pass reads the longest since
    /// the last.
    #[test]
    fn a_gap_counts_and_the_time_unseen_does_not() {
        let start = Instant::now();
        let at = |seconds: f64| start + Duration::from_secs_f64(seconds);
        let mut gaps = TickGaps::default();
        gaps.tick(at(0.0), true);
        gaps.tick(at(0.007), true);
        gaps.tick(at(0.5), true);
        gaps.tick(at(0.507), true);
        assert_eq!(gaps.take(), Some(at(0.5) - at(0.007)), "between two link frames");
        assert_eq!(gaps.take(), None, "taken");
        gaps.tick(at(11.5), false);
        assert_eq!(gaps.take(), None, "the reveal");
        gaps.tick(at(11.507), true);
        assert_eq!(gaps.take(), Some(at(11.507) - at(11.5)), "from the reveal on");
    }
}

impl<State, U> WindowHandler for EguiWindow<State, U>
where
    State: 'static + Send,
    U: FnMut(&mut egui::Ui, &mut Queue, &mut State),
    U: 'static + Send,
{
    fn on_frame(&mut self, window: &mut Window) {
        // The whole callback, end to end, less the drawable wait (see
        // `Queue::tick_ms`). Every other reading measures a STAGE of it, so
        // `tick` against the interval between frames separates "the work is
        // slow" from "we are not being called".
        let tick_start = Instant::now();
        #[cfg(all(feature = "frame-log", target_os = "macos"))]
        let log_start = crate::frame_log::begin();
        let Some(state) = &mut self.user_state else {
            return;
        };

        // Every frame, before deciding whether it runs the UI: the gaps a
        // stall check reads are the display's, not the passes'.
        let timing = window.frame_timing();
        self.tick_gaps.tick(tick_start, timing.is_some());

        // BEFORE the input the frame is laid out from: a size this window has
        // already been given from outside (see `SizeSource`). Read after it,
        // the frame is built for a window that has stopped being that size,
        // and lands in a surface that is already the new one. Asked on every
        // frame that runs, the ones that run no pass included; none runs while
        // the view cannot be seen, so an app cannot leave work it needs done
        // while unseen to the source. A size it gives runs
        // this frame's pass whatever the backoff says: the source hands it
        // over once, and only a pass applies it to the window.
        let old_size = self.surface_size;
        let resized = self.size_source.as_ref().and_then(SizeSource::take).and_then(|logical| {
            SurfaceSize::from_logical(logical, self.native_scale, self.renderer.max_texture_side())
        });
        if let Some(size) = resized {
            self.surface_size = size;
        }

        let frame_at = frame_at(timing, tick_start);
        let asked = self.pass_request.asked();
        if resized.is_none() && !pass_due(asked, self.repaint_after, self.retry_after, frame_at) {
            #[cfg(all(feature = "frame-log", target_os = "macos"))]
            crate::frame_log::end(crate::frame_log::Tick {
                window: std::ptr::from_ref(self) as usize,
                start: log_start,
                timing,
                tick_ms: tick_start.elapsed().as_secs_f32() * 1000.0,
                acquire_ms: 0.0,
                render_ms: 0.0,
                rendered: false,
                drawables: self.renderer.max_drawables(),
            });
            return;
        }
        self.pass_request.take();

        self.egui_time = egui_time(self.egui_time, self.start_time, frame_at);
        self.egui_input.time = Some(self.egui_time);
        set_screen_rect(
            &mut self.egui_input,
            self.viewport_id,
            self.surface_size,
            points_per_pixel(self.native_scale, self.egui_ctx.zoom_factor()),
        );

        //let mut repaint_requested = false;
        let mut frame_divisor = None;
        let mut queue = Queue::new(
            &mut self.bg_color,
            &mut self.close_requested,
            &mut self.shortcuts,
            &mut frame_divisor,
            timing,
            self.tick_gaps.take(),
            self.tess_ms,
            self.draw_gpu_ms.take(),
            self.acquire_ms,
            self.tick_ms,
            self.render_ms,
            self.upload_ms,
            self.ubuf_ms,
            self.texture_ms,
            self.encode_ms,
            self.submit_ms,
            self.prims,
            self.verts,
        );

        let mut full_output = self.egui_ctx.run_ui(self.egui_input.take(), |ui| {
            (self.user_update)(ui, &mut queue, state)
        });

        if let Some(divisor) = frame_divisor {
            window.set_frame_divisor(divisor);
        }

        if self.close_requested {
            window.close();
        }

        // Prevent data from being allocated every frame by storing this
        // in a member field.

        let Some(viewport_output) = full_output.viewport_output.get(&self.viewport_id) else {
            // The main window was closed by egui.
            window.close();
            return;
        };

        for command in viewport_output.commands.iter() {
            match command {
                ViewportCommand::Close => {
                    window.close();
                }
                _ => {}
            }
        }

        if self.surface_size != old_size {
            // A size from the source, in physical pixels; `Window::resize()`
            // takes logical points, or the window ends up `native_scale`
            // times too large on scaled displays.
            window.resize(logical_size_for(self.surface_size, self.native_scale));
        }

        // Copied out of the borrow so it stays readable after `render()` takes
        // `&mut full_output` below (Duration is Copy, so this costs nothing).
        let repaint_delay = viewport_output.repaint_delay;

        // The renderer half of the callback, whole. `tick` minus this is the
        // egui half — the UI closure plus egui's own end-of-pass work — so
        // between them nothing in the frame is unattributed, even though
        // neither is a single stage.
        let render_start = Instant::now();
        let presented = self.renderer.render(
            window,
            self.bg_color,
            self.surface_size,
            // egui's scale for this pass, zoom included.
            full_output.pixels_per_point,
            &mut self.egui_ctx,
            &mut full_output,
        );

        let waited_ms = self.renderer.last_acquire_ms();
        self.render_ms = render_start.elapsed().as_secs_f32() * 1000.0 - waited_ms;

        // A skipped present (occluded window, lost/outdated surface) must not
        // consume the repaint request: the next frame runs a pass again, so
        // the first frame after the surface comes back is fresh rather than
        // the pre-occlusion ghost.
        //
        // On the frame clock (`frame_at`), as `pass_due` compares it: from the
        // refresh this pass drew for, not from when the pass happened to end.
        self.repaint_after =
            if presented { frame_at.checked_add(repaint_delay) } else { Some(frame_at) };

        // A present PROVES the surface is available — the acquire refuses an
        // occluded window — so it is what the backoff defers to, and the only
        // thing that has to be believed for the window to come back. See
        // `after_render`.
        (self.refused_presents, self.retry_after) =
            after_render(presented, self.refused_presents, frame_at);
        self.tess_ms = self.renderer.last_tess_ms();
        self.draw_gpu_ms = self.renderer.take_gpu_ms();
        self.acquire_ms = self.renderer.last_acquire_ms();
        self.upload_ms = self.renderer.last_upload_ms();
        self.ubuf_ms = self.renderer.last_ubuf_ms();
        self.texture_ms = self.renderer.last_texture_ms();
        self.prims = self.renderer.last_prims();
        self.verts = self.renderer.last_verts();
        self.encode_ms = self.renderer.last_encode_ms();
        self.submit_ms = self.renderer.last_submit_ms();

        for command in full_output.platform_output.commands {
            match command {
                egui::OutputCommand::CopyText(text) => {
                    if let Some(clipboard_ctx) = &mut self.clipboard_ctx
                        && let Err(err) = clipboard_ctx.set_contents(text)
                    {
                        error!("Copy/Cut error: {}", err);
                    }
                }
                egui::OutputCommand::CopyImage(_) => {
                    warn!("Copying images is not supported in egui_baseview.");
                }
                egui::OutputCommand::OpenUrl(open_url) => {
                    if let Err(err) = open::that_detached(&open_url.url) {
                        error!("Open error: {}", err);
                    }
                }
            }
        }

        let cursor_icon =
            crate::translate::translate_cursor_icon(full_output.platform_output.cursor_icon);
        if self.current_cursor_icon != cursor_icon {
            self.current_cursor_icon = cursor_icon;

            window.set_mouse_cursor(cursor_icon);
        }

        // A temporary workaround for keyboard input not working sometimes.
        // See https://github.com/BillyDM/egui-baseview/issues/20
        #[cfg(feature = "keyboard_focus_workaround")]
        {
            if !full_output.platform_output.events.is_empty()
                || full_output.platform_output.ime.is_some()
            {
                window.focus();
            }
        }
        self.tick_ms = tick_start.elapsed().as_secs_f32() * 1000.0 - waited_ms;
        #[cfg(all(feature = "frame-log", target_os = "macos"))]
        crate::frame_log::end(crate::frame_log::Tick {
            window: std::ptr::from_ref(self) as usize,
            start: log_start,
            timing,
            tick_ms: self.tick_ms,
            acquire_ms: waited_ms,
            render_ms: self.render_ms,
            rendered: true,
            drawables: self.renderer.max_drawables(),
        });
    }

    #[allow(unused_variables)]
    fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
        // Any event runs the UI on the next frame (see `pass_due`): whatever
        // it changes is drawn on the next refresh.
        self.pass_request.request();

        // Parent/embedded windows do not always gain keyboard focus
        // Automatically on click. Request focus explicitly before forwarding the event.
        if matches!(
            event,
            Event::Mouse(baseview::MouseEvent::ButtonPressed { .. })
        ) && !window.has_focus()
        {
            window.focus();
        }

        match &event {
            baseview::Event::Mouse(event) => match event {
                baseview::MouseEvent::CursorMoved {
                    position,
                    modifiers,
                } => {
                    self.update_modifiers(modifiers);

                    let pos = pointer_in_points(position, self.egui_ctx.zoom_factor());
                    self.egui_input.events.push(egui::Event::PointerMoved(pos));
                }
                baseview::MouseEvent::ButtonPressed {
                    button,
                    position,
                    modifiers,
                }
                | baseview::MouseEvent::ButtonReleased {
                    button,
                    position,
                    modifiers,
                } => {
                    self.update_modifiers(modifiers);

                    let pressed = matches!(event, baseview::MouseEvent::ButtonPressed { .. });
                    push_pointer_button(
                        &mut self.egui_input.events,
                        position,
                        self.egui_ctx.zoom_factor(),
                        *button,
                        pressed,
                        self.egui_input.modifiers,
                    );
                }
                baseview::MouseEvent::WheelScrolled {
                    delta: scroll_delta,
                    modifiers,
                } => {
                    self.update_modifiers(modifiers);

                    let (unit, delta) = scroll_in_points(scroll_delta, self.egui_ctx.zoom_factor());
                    let shift = self.egui_input.modifiers.shift;
                    let delta = wheel_delta(delta, shift);

                    self.egui_input.events.push(egui::Event::MouseWheel {
                        unit,
                        delta,
                        modifiers: self.egui_input.modifiers,
                        phase: egui::TouchPhase::Move,
                    });
                }
                baseview::MouseEvent::CursorLeft => {
                    self.egui_input.events.push(egui::Event::PointerGone);
                }
                _ => {}
            },
            baseview::Event::Keyboard(event) => {
                // The set the event carries rather than a toggle per modifier
                // key, as for mouse events: a key-up that went to another
                // window is corrected by the next event of either kind.
                self.update_modifiers(&event.modifiers);
                let modifiers = self.egui_input.modifiers;
                // Whose key this is, decided once: the host's answer and what
                // egui is sent both follow it.
                let owned = owns_key(&self.egui_ctx, &self.shortcuts, modifiers, event);
                let clipboard = self.clipboard_ctx.as_mut();
                queue_key(&mut self.egui_input.events, event, modifiers, owned, clipboard);
                return if owned { EventStatus::Captured } else { EventStatus::Ignored };
            }
            baseview::Event::Window(event) => match event {
                baseview::WindowEvent::Resized(window_info) => {
                    // A zero, non-finite or negative backing scale gives the
                    // event a size no surface can be made at, so this refuses
                    // it too. A fixed scale policy's scale is the app's own,
                    // trusted as in `new()`.
                    let Some(size) = SurfaceSize::new(
                        window_info.physical_size(),
                        self.renderer.max_texture_side(),
                    ) else {
                        return EventStatus::Ignored;
                    };
                    self.native_scale = match self.scale_policy {
                        WindowScalePolicy::ScaleFactor(scale) => scale,
                        WindowScalePolicy::SystemScaleFactor => window_info.scale(),
                    } as f32;
                    self.surface_size = size;

                    // The screen and viewport rects are built for the next
                    // pass, from this size, before it runs.
                    let viewport_info = self
                        .egui_input
                        .viewports
                        .get_mut(&self.viewport_id)
                        .unwrap();
                    viewport_info.native_pixels_per_point = Some(self.native_scale);
                }
                event @ (baseview::WindowEvent::Focused | baseview::WindowEvent::Unfocused) => {
                    update_focus_input(&mut self.egui_input, self.viewport_id, event);
                }
                baseview::WindowEvent::Occluded(occluded) => {
                    // The renderer's half of it: what the window shows for the
                    // frame between coming back and having something fresh to
                    // show. See the wgpu renderer's `layer_present`.
                    self.renderer.window_occluded(*occluded);
                    if !occluded {
                        // Re-exposed after occlusion: the compositor may
                        // have kept showing a stale snapshot of the window,
                        // and this event runs a pass on the next frame like
                        // any other once a backoff left from before the
                        // occlusion is dropped, as it is here, so that frame
                        // and the next ones are offered rather than one
                        // 250 ms later.
                        // This is a shortcut, never the way back: the retry
                        // recovers a window whose `Occluded(false)` never
                        // arrives, which is why `pass_due` is keyed on
                        // refusals and not on this.
                        self.refused_presents = 0;
                        self.retry_after = None;
                    }
                }
                baseview::WindowEvent::WillClose => {}
            },
        }

        match &event {
            baseview::Event::Mouse(_) => {
                if self.egui_ctx.egui_is_using_pointer() || self.egui_ctx.egui_wants_pointer_input()
                {
                    EventStatus::Captured
                } else {
                    EventStatus::Ignored
                }
            }
            // A window event; a key returned its own answer above.
            _ => EventStatus::Captured,
        }
    }
}

/// Whether the editor owns this key event: an owned press reaches egui and is
/// kept from the host. Every other press is the host's alone, so the host's
/// transport and shortcuts keep working over the editor, and no press acts in
/// both.
///
/// Only a press is owned. Every release and every modifier-only event goes to
/// the host as well as to egui, so the host never loses the release of a key it
/// saw go down, nor a modifier change, however focus or modifiers moved since.
/// The cost is that the host receives the release of a key it never saw go
/// down (a typed character, a listed shortcut): the same stray release it
/// already receives after Enter or Escape ends a field, which no host acts on.
///
/// Bare Escape is owned while a drag is in progress, which egui cancels on it;
/// and while a popup is open, which it closes, even without widget focus.
/// A selection in a label owns bare Cmd-C and bare Escape: unowned, the same
/// Cmd-C also goes up to the host, and its own copy competes with the label's
/// for the pasteboard; Escape clears the selection, and would clear the
/// host's too. A click without a drag counts, because egui copies the whole
/// label from a bare cursor. The arrows and Cmd-A, which also move a label
/// selection, stay the host's: owning them would take the arrows from the
/// host after any stray click on a label. Cut means nothing on a label, so
/// the host cuts alone. These match the bare chord only, so a host command
/// on the same key with another modifier (Cmd-Shift-C) stays the host's.
///
/// A focused text field owns the keys it uses ([`text_edit_owns_key`]), and
/// a listed shortcut it does not use goes to the host. With no text field
/// focused, the editor owns exactly its listed shortcuts
/// ([`Queue::set_shortcuts`]); a focused widget of another kind owns no key
/// by its focus, so Space after a click on a button is the host's.
///
/// Read between egui passes, from the state the last pass left, so persistent
/// popup, drag and selection state is read before the Escape that clears it.
fn owns_key(
    ctx: &egui::Context,
    shortcuts: &[egui::KeyboardShortcut],
    modifiers: egui::Modifiers,
    event: &keyboard_types::KeyboardEvent,
) -> bool {
    use egui::text_selection::LabelSelectionState;
    if event.state != keyboard_types::KeyState::Down {
        return false;
    }
    let escape = matches!(event.key, keyboard_types::Key::Escape) && modifiers.is_none();
    let copy = matches!(press_sends(event, modifiers), PressSends::Copy)
        && modifiers.command_only()
        && !modifiers.ctrl;
    let label_selected = || {
        ctx.with_plugin(|labels: &mut LabelSelectionState| labels.has_selection()) == Some(true)
    };
    if escape && (ctx.dragged_id().is_some() || egui::Popup::is_any_open(ctx))
        || (escape || copy) && label_selected()
    {
        return true;
    }
    if ctx.text_edit_focused() {
        return text_edit_owns_key(event, modifiers);
    }
    let key = crate::translate::translate_key(event);
    shortcuts.iter().any(|shortcut| {
        key == Some(shortcut.logical_key) && modifiers.matches_exact(shortcut.modifiers)
    })
}

/// What a press sends egui beside its key event. Read by both `queue_key`,
/// which sends it, and `text_edit_owns_key`, which owns whatever it sends a
/// field.
#[derive(Debug, PartialEq)]
enum PressSends<'a> {
    Cut,
    Copy,
    Paste,
    Text(&'a str),
    Nothing,
}

/// The clipboard chords by the key they sit on, so they work on any layout;
/// any Command chord on X, C or V counts, as egui-winit reads them.
fn press_sends(event: &keyboard_types::KeyboardEvent, modifiers: egui::Modifiers) -> PressSends<'_> {
    use keyboard_types::Code;
    match &event.key {
        _ if modifiers.command && event.code == Code::KeyX => PressSends::Cut,
        _ if modifiers.command && event.code == Code::KeyC => PressSends::Copy,
        _ if modifiers.command && event.code == Code::KeyV => PressSends::Paste,
        keyboard_types::Key::Character(written) if !modifiers.ctrl && !modifiers.command => {
            PressSends::Text(written)
        }
        _ => PressSends::Nothing,
    }
}

/// Send egui a key event as `owns_key` decided it. A press the host gets never
/// reaches egui, so one keystroke acts in one place; an owned press brings its
/// key and what it sends ([`press_sends`]). A release always reaches egui, so
/// a key cannot stay held there when focus or modifiers changed after its
/// press.
fn queue_key(
    events: &mut Vec<egui::Event>,
    event: &keyboard_types::KeyboardEvent,
    modifiers: egui::Modifiers,
    owned: bool,
    clipboard: Option<&mut Clipboard>,
) {
    let pressed = event.state == keyboard_types::KeyState::Down;
    if pressed && !owned {
        return;
    }

    if let Some(key) = crate::translate::translate_key(event) {
        events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: event.repeat,
            modifiers,
        });
    }

    if !pressed {
        return;
    }
    match press_sends(event, modifiers) {
        PressSends::Cut => events.push(egui::Event::Cut),
        PressSends::Copy => events.push(egui::Event::Copy),
        PressSends::Paste => {
            if let Some(clipboard) = clipboard {
                match clipboard.get_contents() {
                    Ok(contents) => events.push(egui::Event::Text(contents)),
                    Err(err) => {
                        error!("Paste error: {}", err);
                    }
                }
            }
        }
        PressSends::Text(written) => events.push(egui::Event::Text(written.to_owned())),
        PressSends::Nothing => {}
    }
}

/// The keys a focused text field uses: whatever `queue_key` sends it as typing
/// or a clipboard event, and the editing commands egui 0.35's TextEdit reads:
/// `events` and `check_for_mutating_key_press` in `widgets/text_edit/builder.rs`,
/// and `CCursorRange::on_key_press` in `text_selection/cursor_range.rs` for the
/// cursor moves. An egui upgrade re-derives the table from those. Whether a
/// character is valid belongs to the field: a letter a number field rejects
/// must never become a host command.
fn text_edit_owns_key(event: &keyboard_types::KeyboardEvent, modifiers: egui::Modifiers) -> bool {
    if press_sends(event, modifiers) != PressSends::Nothing {
        return true;
    }

    use egui::Key;
    match crate::translate::translate_key(event) {
        Some(
            Key::ArrowLeft
            | Key::ArrowRight
            | Key::ArrowUp
            | Key::ArrowDown
            | Key::Home
            | Key::End
            | Key::Backspace
            | Key::Delete,
        ) => true,
        Some(Key::Enter | Key::Tab | Key::Escape) => {
            !modifiers.alt && !modifiers.ctrl && !modifiers.command
        }
        Some(Key::A) if modifiers.command => true,
        Some(Key::Z | Key::Y) => modifiers.matches_logically(egui::Modifiers::COMMAND),
        Some(Key::H | Key::K | Key::U | Key::W) => modifiers.ctrl,
        Some(Key::A | Key::E | Key::P | Key::N | Key::B | Key::F) => {
            modifiers.ctrl && !modifiers.shift
        }
        _ => false,
    }
}

/// RawInput.focused drives egui's InputState; the event also clears held keys.
fn update_focus_input(
    input: &mut egui::RawInput,
    viewport_id: egui::ViewportId,
    event: &baseview::WindowEvent,
) {
    let focused = match event {
        baseview::WindowEvent::Focused => true,
        baseview::WindowEvent::Unfocused => false,
        _ => return,
    };
    // Both on purpose, neither a duplicate of the other: harmonigraph ends a
    // gesture on the focus-lost event, and egui reads the flag.
    input.focused = focused;
    input.events.push(egui::Event::WindowFocused(focused));
    if let Some(viewport) = input.viewports.get_mut(&viewport_id) {
        viewport.focused = Some(focused);
    }
}

/// egui points per physical pixel: the reciprocal of the native scale times
/// egui's zoom factor, as egui-winit computes it. Logical host sizes stay
/// native.
fn points_per_pixel(native_scale: f32, zoom: f32) -> f32 {
    (native_scale * zoom).recip()
}

/// A cursor position in logical (host) pixels, in egui points under `zoom`.
fn pointer_in_points(position: &baseview::Point, zoom: f32) -> Pos2 {
    pos2(position.x as f32, position.y as f32) / zoom
}

/// A wheel or trackpad scroll as egui reads it. A precise delta is in
/// logical (host) pixels, AppKit's points, as the pointer is, so it divides
/// by the zoom alone: dividing by the backing scale as well halved every
/// trackpad scroll on a 2x display. winit, behind egui-winit, gives the same.
fn scroll_in_points(delta: &baseview::ScrollDelta, zoom: f32) -> (egui::MouseWheelUnit, egui::Vec2) {
    match *delta {
        baseview::ScrollDelta::Lines { x, y } => (egui::MouseWheelUnit::Line, egui::vec2(x, y)),
        baseview::ScrollDelta::Pixels { x, y } => {
            (egui::MouseWheelUnit::Point, egui::vec2(x, y) / zoom)
        }
    }
}

/// A press or release at the position baseview gives it, in points under the
/// zoom it is delivered at, so a click right after a zoom change lands where
/// the pointer is.
///
/// Always behind a move to the same point. egui judges a click from moves
/// alone (`has_moved_too_much_for_a_click` is set only by `PointerMoved`), so
/// a release at a distant point with no move before it would still count as
/// a click. The move costs nothing when the pointer is already there: egui's
/// movement clock is reset by the click as well, for tooltips, and its
/// velocity is sampled once per pass from the latest position, not per event.
/// Asking the context whether egui already has the point would mean reading
/// this pass's queued events too, for no change in what egui decides.
fn push_pointer_button(
    events: &mut Vec<egui::Event>,
    position: &baseview::Point,
    zoom: f32,
    button: baseview::MouseButton,
    pressed: bool,
    modifiers: egui::Modifiers,
) {
    let Some(button) = crate::translate::translate_mouse_button(button) else { return };
    let pos = pointer_in_points(position, zoom);
    events.push(egui::Event::PointerMoved(pos));
    events.push(egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers,
    });
}

/// Calculate screen rectangle in points.
fn calculate_screen_rect(physical_size: PhySize, points_per_pixel: f32) -> Rect {
    let logical_size = (
        physical_size.width as f32 * points_per_pixel,
        physical_size.height as f32 * points_per_pixel,
    );
    Rect::from_min_size(Pos2::new(0f32, 0f32), vec2(logical_size.0, logical_size.1))
}

/// A size a surface and its textures can be made at: neither side zero, and
/// neither past the device's texture limit. The renderer takes no other, so
/// each writer of the window's size checks it here, once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceSize(PhySize);

impl SurfaceSize {
    pub(crate) fn new(size: PhySize, limit: usize) -> Option<Self> {
        let fits = |side: u32| side > 0 && side as usize <= limit;
        (fits(size.width) && fits(size.height)).then_some(Self(size))
    }

    /// The physical size of a logical one at the native scale, rounded as
    /// baseview rounds the size its `Resized` event reports. A non-finite or
    /// non-positive size or scale saturates to zero or `u32::MAX` in the cast,
    /// which `new` refuses.
    pub(crate) fn from_logical(logical: Size, native_scale: f32, limit: usize) -> Option<Self> {
        Self::new(
            WindowInfo::from_logical_size(logical, native_scale as f64).physical_size(),
            limit,
        )
    }

    /// The window's opening size, which has nothing to fall back to: each
    /// side clamped into 1..=limit.
    fn clamped(size: PhySize, limit: usize) -> Self {
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        Self(PhySize::new(size.width.min(limit).max(1), size.height.min(limit).max(1)))
    }

    pub(crate) fn get(self) -> PhySize {
        self.0
    }
}

fn logical_size_for(size: SurfaceSize, scale: f32) -> Size {
    let physical = size.get();
    Size::new(
        physical.width as f64 / scale as f64,
        physical.height as f64 / scale as f64,
    )
}

/// The screen rect and the viewport's, in points, for the input a pass is
/// laid out from: `points_per_pixel` carries egui's zoom, while `size` and
/// the host's logical size stay native.
fn set_screen_rect(
    input: &mut egui::RawInput,
    viewport_id: egui::ViewportId,
    size: SurfaceSize,
    points_per_pixel: f32,
) {
    let screen = calculate_screen_rect(size.get(), points_per_pixel);
    input.screen_rect = Some(screen);
    if let Some(viewport) = input.viewports.get_mut(&viewport_id) {
        viewport.inner_rect = Some(screen);
    }
}

#[cfg(test)]
mod key_capture_tests {
    use super::*;
    use keyboard_types::Modifiers as M;

    fn command() -> M {
        M::META
    }

    /// Whether the editor owns `event`, read with the modifiers it carries.
    fn owns(
        ctx: &egui::Context,
        shortcuts: &[egui::KeyboardShortcut],
        event: &keyboard_types::KeyboardEvent,
    ) -> bool {
        owns_key(ctx, shortcuts, crate::translate::translate_modifiers(event.modifiers), event)
    }

    /// What egui is sent for `event` on the verdict `owned`.
    fn queued(event: &keyboard_types::KeyboardEvent, owned: bool) -> Vec<egui::Event> {
        let mut events = Vec::new();
        let modifiers = crate::translate::translate_modifiers(event.modifiers);
        queue_key(&mut events, event, modifiers, owned, None);
        events
    }

    /// A context whose last pass left a text field focused.
    fn typing() -> egui::Context {
        let ctx = egui::Context::default();
        let mut text = String::new();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut text).request_focus()
        });
        assert!(ctx.text_edit_focused());
        ctx
    }

    #[test]
    fn escape_closes_an_unfocused_combo_without_reaching_the_host() {
        use keyboard_types::{Key, KeyState, KeyboardEvent};
        let ctx = egui::Context::default();
        let combo = std::cell::Cell::new(egui::Id::NULL);
        let mut draw = |ui: &mut egui::Ui| {
            combo.set(
                egui::ComboBox::from_id_salt("popup escape")
                    .selected_text("choice")
                    .show_ui(ui, |ui| {
                        let _ = ui.selectable_label(false, "choice");
                    })
                    .response
                    .id,
            );
        };
        let _ = ctx.run_ui(egui::RawInput::default(), &mut draw);
        egui::Popup::open_id(&ctx, combo.get().with("popup"));
        let _ = ctx.run_ui(egui::RawInput::default(), &mut draw);
        assert!(egui::ComboBox::is_open(&ctx, combo.get()));
        assert!(!ctx.egui_wants_keyboard_input());
        let event = KeyboardEvent { key: Key::Escape, state: KeyState::Down, ..Default::default() };
        assert!(owns(&ctx, &[], &event), "the popup uses Escape even without widget focus");
        let alt_escape = KeyboardEvent { modifiers: M::ALT, ..event.clone() };
        assert!(!owns(&ctx, &[], &alt_escape), "only the bare chord");
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = ctx.run_ui(input, &mut draw);
        assert!(!egui::ComboBox::is_open(&ctx, combo.get()));
        assert!(!owns(&ctx, &[], &event));
    }

    /// egui cancels a drag on Escape (`interaction::interact`), so a drag in
    /// progress owns bare Escape, focused or not.
    #[test]
    fn escape_cancels_a_drag_without_reaching_the_host() {
        use keyboard_types::{Key, KeyboardEvent};
        let ctx = egui::Context::default();
        let frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let mut rect = Rect::NOTHING;
            let _ = ctx.run_ui(raw, |ui| {
                rect = ui.allocate_response(vec2(100.0, 100.0), egui::Sense::drag()).rect
            });
            rect
        };
        let escape = KeyboardEvent { key: Key::Escape, ..Default::default() };
        let at = frame(vec![]).center();
        assert!(!owns(&ctx, &[], &escape), "nothing going on: Escape is the host's");

        let to = at + vec2(30.0, 0.0);
        frame(vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerMoved(to)]);
        assert!(ctx.dragged_id().is_some());
        assert!(owns(&ctx, &[], &escape), "the drag uses Escape");

        frame(vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(ctx.dragged_id().is_none(), "egui cancelled the drag");
        assert!(!owns(&ctx, &[], &escape));
    }

    #[test]
    fn reserved_shortcuts_are_not_forwarded_to_the_host_without_widget_focus() {
        use keyboard_types::{Key, KeyState, KeyboardEvent};
        let shortcuts = [
            egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z),
            egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
                egui::Key::Z,
            ),
            egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Delete),
            egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Escape),
            egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Escape),
        ];
        // A focused button changes nothing: only a text field takes keys.
        let button = egui::Context::default();
        let _ = button.run_ui(egui::RawInput::default(), |ui| ui.button("button").request_focus());
        for (key, modifiers, expected) in [
            (Key::Character("z".into()), command(), true),
            (Key::Character("Z".into()), command() | M::SHIFT, true),
            (Key::Delete, M::empty(), true),
            (Key::Escape, M::empty(), true),
            (Key::Escape, M::SHIFT, true),
            (Key::Character("z".into()), M::empty(), false),
            (Key::Character("z".into()), command() | M::ALT, false),
            (Key::Character(" ".into()), M::empty(), false),
            (Key::Character("s".into()), command(), false),
        ] {
            let event = KeyboardEvent { key, modifiers, ..Default::default() };
            let release = KeyboardEvent { state: KeyState::Up, ..event.clone() };
            for ctx in [&egui::Context::default(), &button] {
                assert_eq!(owns(ctx, &shortcuts, &event), expected, "{event:?}");
                assert!(!owns(ctx, &shortcuts, &release), "a release is always the host's too");
            }
        }
    }

    #[test]
    fn unused_commands_reach_the_host_with_or_without_text_focus() {
        use keyboard_types::{Key, KeyboardEvent};
        for (key, modifiers) in [
            (Key::Character("s".into()), command()),
            (Key::Character("S".into()), command() | M::SHIFT),
            (Key::Character("o".into()), command()),
            (Key::Character("n".into()), command()),
            (Key::Enter, command()),
            (Key::Enter, M::ALT),
            (Key::Tab, M::CONTROL),
            (Key::Tab, M::ALT),
            (Key::Escape, M::ALT),
            (Key::F5, M::empty()),
            (Key::PageUp, M::empty()),
            // A modifier alone is always the host's as well.
            (Key::Shift, M::SHIFT),
            (Key::Meta, command()),
            (Key::Alt, M::ALT),
            (Key::Control, M::CONTROL),
            (Key::CapsLock, M::CAPS_LOCK),
        ] {
            for ctx in [egui::Context::default(), typing()] {
                let event = KeyboardEvent { key: key.clone(), modifiers, ..Default::default() };
                assert!(!owns(&ctx, &[], &event), "{event:?}");
                assert_eq!(queued(&event, false), [], "the host's press never reaches egui");
            }
        }
    }

    #[test]
    fn typing_and_text_editing_stay_in_the_focused_field() {
        use keyboard_types::{Code, Key, KeyboardEvent};
        let ctx = typing();
        for (key, code, modifiers) in [
            // A number field may reject these characters; ownership cannot depend on parsing.
            (Key::Character("s".into()), Code::KeyS, M::empty()),
            (Key::Character("!".into()), Code::Digit1, M::SHIFT),
            (Key::Character("å".into()), Code::KeyA, M::ALT),
            (Key::Character("λ".into()), Code::Unidentified, M::empty()),
            (Key::Character(" ".into()), Code::Space, M::empty()),
            (Key::Backspace, Code::Backspace, M::ALT),
            (Key::Delete, Code::Delete, command()),
            (Key::ArrowLeft, Code::ArrowLeft, M::CONTROL | M::SHIFT),
            (Key::ArrowRight, Code::ArrowRight, M::ALT),
            (Key::ArrowUp, Code::ArrowUp, command()),
            (Key::ArrowDown, Code::ArrowDown, M::SHIFT),
            (Key::Home, Code::Home, M::empty()),
            (Key::End, Code::End, M::SHIFT),
            (Key::Enter, Code::Enter, M::empty()),
            (Key::Tab, Code::Tab, M::SHIFT),
            (Key::Escape, Code::Escape, M::empty()),
            (Key::Character("a".into()), Code::KeyA, command()),
            (Key::Character("z".into()), Code::KeyZ, command()),
            (Key::Character("Z".into()), Code::KeyZ, command() | M::SHIFT),
            (Key::Character("y".into()), Code::KeyY, command()),
            (Key::Character("x".into()), Code::KeyX, command()),
            (Key::Character("v".into()), Code::KeyV, command()),
            // Clipboard translation uses physical codes, even on another layout.
            (Key::Character("q".into()), Code::KeyC, command()),
            (Key::Character("h".into()), Code::KeyH, M::CONTROL),
            (Key::Character("k".into()), Code::KeyK, M::CONTROL),
            (Key::Character("u".into()), Code::KeyU, M::CONTROL),
            (Key::Character("w".into()), Code::KeyW, M::CONTROL),
            (Key::Character("f".into()), Code::KeyF, M::CONTROL),
            (Key::Character("e".into()), Code::KeyE, M::CONTROL),
        ] {
            let event = KeyboardEvent { key, code, modifiers, ..Default::default() };
            assert!(owns(&ctx, &[], &event), "{event:?}");
        }
        let s = KeyboardEvent { key: Key::Character("s".into()), code: Code::KeyS, ..Default::default() };
        let key = egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(queued(&s, owns(&ctx, &[], &s)), [key, egui::Event::Text("s".into())]);
    }

    #[test]
    fn text_focus_replaces_canvas_shortcut_ownership() {
        use keyboard_types::{Key, KeyboardEvent};
        let save = [egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S)];
        let event =
            KeyboardEvent { key: Key::Character("s".into()), modifiers: command(), ..Default::default() };
        assert!(owns(&egui::Context::default(), &save, &event));
        assert!(!owns(&typing(), &save, &event));
    }

    /// A focused control that is not a text field owns no key by its focus,
    /// so Space after a click on a button plays the host, and never presses
    /// the button as well.
    #[test]
    fn a_focused_button_leaves_space_to_the_host() {
        use keyboard_types::{Code, Key, KeyState, KeyboardEvent};
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| ui.button("button").request_focus());
        assert!(ctx.egui_wants_keyboard_input() && !ctx.text_edit_focused());
        let space = KeyboardEvent {
            key: Key::Character(" ".into()),
            code: Code::Space,
            state: KeyState::Down,
            ..Default::default()
        };
        let owned = owns(&ctx, &[], &space);
        assert!(!owned);
        assert_eq!(queued(&space, owned), [], "the host's press never reaches egui");
        let listed = [egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Space)];
        assert!(owns(&ctx, &listed, &space), "unless the app lists it");
    }

    /// A numeric field being edited is a text field: a click puts egui's
    /// `DragValue` into a `TextEdit` of its own.
    #[test]
    fn a_number_being_typed_owns_its_digits_but_not_unused_commands() {
        use keyboard_types::{Code, Key, KeyState, KeyboardEvent};
        let ctx = egui::Context::default();
        let mut value = 1.0_f32;
        let mut frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let mut rect = Rect::NOTHING;
            let _ = ctx.run_ui(raw, |ui| rect = ui.add(egui::DragValue::new(&mut value)).rect);
            rect
        };
        let at = frame(vec![]).center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(vec![egui::Event::PointerMoved(at), press(true)]);
        frame(vec![press(false)]);
        frame(vec![]);
        assert!(ctx.text_edit_focused(), "a click edits the number as text");

        let event = |key: &str, code, modifiers| KeyboardEvent {
            key: Key::Character(key.into()),
            code,
            modifiers,
            state: KeyState::Down,
            ..Default::default()
        };
        assert!(owns(&ctx, &[], &event("7", Code::Digit7, M::empty())));
        assert!(!owns(&ctx, &[], &event("s", Code::KeyS, command())), "Cmd-S is the host's");
    }

    #[test]
    fn a_label_selection_takes_cmd_c_from_the_host() {
        use keyboard_types::{Code, KeyboardEvent};
        let copy = (egui::Modifiers::COMMAND, Code::KeyC);
        let claims = |ctx, (modifiers, code)| {
            let event = KeyboardEvent { code, ..Default::default() };
            owns_key(ctx, &[], modifiers, &event)
        };

        let ctx = egui::Context::default();
        // Draws the label, and says where it went.
        let frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let mut label = Rect::NOTHING;
            let _ = ctx.run_ui(raw, |ui| label = ui.label("frame stall 41 ms").rect);
            label
        };
        let label = frame(vec![]);
        assert!(!claims(&ctx, copy), "nothing selected: the host keeps Cmd-C");

        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let (from, to) = (label.left_center() + vec2(1.0, 0.0), label.right_center());
        frame(vec![egui::Event::PointerMoved(from), press(from, true)]);
        frame(vec![egui::Event::PointerMoved(to)]);
        frame(vec![press(to, false)]);
        frame(vec![]);

        assert!(claims(&ctx, copy), "a selected label claims Cmd-C");
        assert!(!claims(&ctx, (egui::Modifiers::NONE, Code::KeyC)));
        assert!(!claims(&ctx, (egui::Modifiers::COMMAND, Code::KeyV)));
        let shift_copy = (egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), Code::KeyC);
        assert!(!claims(&ctx, shift_copy), "Cmd-Shift-C is the host's own command");

        // What the owned press sends egui, on the verdict the press gets.
        let sent = |code| {
            let event = KeyboardEvent { code, modifiers: command(), ..Default::default() };
            queued(&event, owns(&ctx, &[], &event))
        };
        assert!(sent(Code::KeyC).contains(&egui::Event::Copy), "the label copies");
        assert_eq!(sent(Code::KeyX), [], "a cut is the host's alone");
    }

    #[test]
    fn a_label_selection_takes_escape_from_the_host() {
        use keyboard_types::{Key, KeyboardEvent};
        let claims = |ctx: &egui::Context, key: Key| {
            let event = KeyboardEvent { key, ..Default::default() };
            owns_key(ctx, &[], egui::Modifiers::NONE, &event)
        };

        let ctx = egui::Context::default();
        let frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let mut label = Rect::NOTHING;
            let _ = ctx.run_ui(raw, |ui| label = ui.label("frame stall 41 ms").rect);
            label
        };
        let label = frame(vec![]);
        assert!(!claims(&ctx, Key::Escape), "nothing selected: the host keeps Escape");

        // A bare click arms it: egui holds a cursor in the label as a selection.
        let at = label.center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(vec![egui::Event::PointerMoved(at), press(true)]);
        frame(vec![press(false)]);
        frame(vec![]);

        assert!(claims(&ctx, Key::Escape), "Escape clears the selection, so it is egui's");
        let cut = KeyboardEvent {
            key: Key::Character("x".into()),
            code: keyboard_types::Code::KeyX,
            ..Default::default()
        };
        assert!(
            !owns_key(&ctx, &[], egui::Modifiers::COMMAND, &cut),
            "a cut means nothing on a label: the host cuts alone"
        );
        assert!(!claims(&ctx, Key::ArrowLeft), "the arrows stay the host's");
        let cmd_escape = KeyboardEvent { key: Key::Escape, ..Default::default() };
        assert!(!owns_key(&ctx, &[], egui::Modifiers::COMMAND, &cmd_escape), "only bare Escape");

        frame(vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(vec![]);
        assert!(!claims(&ctx, Key::Escape), "cleared: the next Escape is the host's");
    }

    /// AppKit reports ⌥A as the "å" it types, which names no key; the key it
    /// sits on is the one a shortcut matches.
    #[test]
    fn an_option_letter_is_matched_by_the_key_it_sits_on() {
        use keyboard_types::{Code, Key, KeyState, KeyboardEvent};
        let option_a = [egui::KeyboardShortcut::new(egui::Modifiers::ALT, egui::Key::A)];
        let event = |code| KeyboardEvent {
            key: Key::Character("å".into()),
            code,
            modifiers: M::ALT,
            state: KeyState::Down,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        assert!(owns(&ctx, &option_a, &event(Code::KeyA)));
        assert!(!owns(&ctx, &option_a, &event(Code::KeyS)));
    }

    #[test]
    fn releasing_a_modifier_before_the_letter_does_not_leave_a_key_held() {
        use keyboard_types::{Key, KeyState, KeyboardEvent};
        let option_a = [egui::KeyboardShortcut::new(egui::Modifiers::ALT, egui::Key::A)];
        let ctx = egui::Context::default();
        for (state, modifiers) in [
            (KeyState::Down, M::ALT),
            // Option is released before A; its release no longer matches the shortcut.
            (KeyState::Up, M::empty()),
            (KeyState::Down, M::ALT),
        ] {
            let event =
                KeyboardEvent { key: Key::Character("a".into()), state, modifiers, ..Default::default() };
            let events = queued(&event, owns(&ctx, &option_a, &event));
            let modifiers = crate::translate::translate_modifiers(modifiers);
            let raw = egui::RawInput { events, modifiers, focused: true, ..Default::default() };
            let _ = ctx.run_ui(raw, |ui| {
                assert_eq!(ui.input(|input| input.key_down(egui::Key::A)), state == KeyState::Down);
            });
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    /// A size from the source reaches the input the pass is laid out from,
    /// in logical size, at both backing scales.
    #[test]
    fn native_resize_precedes_input_viewport_layout_and_surface_at_both_scales() {
        for scale in [1.0, 2.0] {
            let source = SizeSource::new(|| Some(Size::new(1300.0, 750.0)));
            let size = source
                .take()
                .and_then(|logical| SurfaceSize::from_logical(logical, scale, 8192))
                .unwrap();
            assert_eq!(
                size.get(),
                PhySize::new((1300.0 * scale) as u32, (750.0 * scale) as u32)
            );
            let mut input = egui::RawInput::default();
            set_screen_rect(&mut input, egui::ViewportId::ROOT, size, points_per_pixel(scale, 1.0));
            let context = egui::Context::default();
            let _ = context.run_ui(input, |ui| {
                assert_eq!(
                    ui.ctx().input(|i| i.viewport().inner_rect.unwrap().size()),
                    vec2(1300.0, 750.0)
                );
                assert_eq!(ui.ctx().input(|i| i.viewport_rect().size()), vec2(1300.0, 750.0));
                assert!(ui.max_rect().width() <= 1300.0);
            });
        }
    }

    /// egui's zoom divides the logical window into fewer points, at both
    /// backing scales, while the surface keeps its physical size and the
    /// viewport its native scale.
    #[test]
    fn native_resize_screen_and_pointer_points_account_for_zoom() {
        for scale in [1.0, 2.0] {
            let size = SurfaceSize::from_logical(Size::new(1400.0, 800.0), scale, 8192).unwrap();
            assert_eq!(
                size.get(),
                PhySize::new((1400.0 * scale) as u32, (800.0 * scale) as u32)
            );
            let mut input = egui::RawInput::default();
            let _ = input
                .viewports
                .insert(egui::ViewportId::ROOT, egui::ViewportInfo {
                    native_pixels_per_point: Some(scale),
                    ..Default::default()
                });
            set_screen_rect(&mut input, egui::ViewportId::ROOT, size, points_per_pixel(scale, 1.25));
            assert_eq!(input.screen_rect.unwrap().size(), vec2(1120.0, 640.0));
            let context = egui::Context::default();
            context.options_mut(|options| options.zoom_factor = 1.25);
            let _ = context.run_ui(input, |ui| {
                assert_eq!(ui.ctx().pixels_per_point(), scale * 1.25);
                assert_eq!(ui.ctx().content_rect().size(), vec2(1120.0, 640.0));
            });
            let corner = baseview::Point::new(1400.0, 800.0);
            assert_eq!(pointer_in_points(&corner, 1.25), pos2(1120.0, 640.0));
        }
    }

    /// A precise scroll moves the content as far as the pointer moves over
    /// the same logical distance, at any backing scale; a line scroll stays
    /// in lines.
    #[test]
    fn a_precise_scroll_moves_as_far_as_the_pointer() {
        let zoom = 1.25;
        let pointer = pointer_in_points(&baseview::Point::new(100.0, 40.0), zoom)
            - pointer_in_points(&baseview::Point::new(0.0, 0.0), zoom);
        assert_eq!(
            scroll_in_points(&baseview::ScrollDelta::Pixels { x: 100.0, y: 40.0 }, zoom),
            (egui::MouseWheelUnit::Point, pointer)
        );
        assert_eq!(
            scroll_in_points(&baseview::ScrollDelta::Lines { x: 1.0, y: 3.0 }, zoom),
            (egui::MouseWheelUnit::Line, vec2(1.0, 3.0))
        );
    }

    /// A press with no move before it lands where baseview says, under the
    /// zoom it is delivered at; a release far from it is no click, and one
    /// beside it is.
    #[test]
    fn a_button_lands_at_its_own_position_and_a_long_drag_is_no_click() {
        let clicked = |release: baseview::Point| {
            let mut events = Vec::new();
            for (position, pressed) in [(baseview::Point::new(400.0, 200.0), true), (release, false)]
            {
                push_pointer_button(
                    &mut events,
                    &position,
                    2.0,
                    baseview::MouseButton::Left,
                    pressed,
                    egui::Modifiers::NONE,
                );
            }
            assert!(events.iter().any(|event| matches!(
                event,
                egui::Event::PointerButton { pos, pressed: true, .. } if *pos == pos2(200.0, 100.0)
            )));
            let context = egui::Context::default();
            let raw = egui::RawInput { events, ..Default::default() };
            let mut clicked = false;
            let _ = context.run_ui(raw, |ui| clicked = ui.ctx().input(|i| i.pointer.any_click()));
            clicked
        };
        assert!(clicked(baseview::Point::new(401.0, 200.0)), "a click in place");
        assert!(!clicked(baseview::Point::new(600.0, 200.0)), "a drag is no click");
    }

    #[test]
    fn native_resize_focus_events_reach_egui_and_persist_between_frames() {
        let context = egui::Context::default();
        let mut input = egui::RawInput::default();
        for (event, expected) in [
            (Some(baseview::WindowEvent::Focused), true),
            (Some(baseview::WindowEvent::Unfocused), false),
            (None, false),
            (Some(baseview::WindowEvent::Focused), true),
        ] {
            if let Some(event) = event {
                update_focus_input(&mut input, egui::ViewportId::ROOT, &event);
                assert!(input.events.iter().any(
                    |event| matches!(event, egui::Event::WindowFocused(focused) if *focused == expected)
                ));
            }
            let _ = context.run_ui(input.take(), |ui| {
                assert_eq!(ui.ctx().input(|i| i.focused), expected);
                assert_eq!(ui.ctx().input(|i| i.viewport().focused), Some(expected));
            });
        }
    }

    #[test]
    fn native_resize_child_takes_logical_units() {
        for scale in [1.0, 2.0] {
            let size = SurfaceSize::new(
                PhySize::new((800.0 * scale) as u32, (500.0 * scale) as u32),
                8192,
            )
            .unwrap();
            assert_eq!(logical_size_for(size, scale), Size::new(800.0, 500.0));
        }
    }

    /// The one check every size passes before a surface or texture is made
    /// at it, and the clamp for the opening size, which cannot be refused.
    #[test]
    fn a_surface_size_is_within_the_device_limit_and_never_zero() {
        let logical = |w, h, scale| SurfaceSize::from_logical(Size::new(w, h), scale, 8192);
        assert_eq!(logical(4096.0, 4096.0, 2.0).map(SurfaceSize::get), Some(PhySize::new(8192, 8192)));
        assert_eq!(logical(4097.0, 4096.0, 2.0), None);
        assert_eq!(logical(4096.0, 4096.0, 3.0), None);
        assert_eq!(logical(f64::INFINITY, 360.0, 1.0), None);
        assert_eq!(logical(f64::NAN, 360.0, 1.0), None);
        assert_eq!(logical(0.0, 360.0, 1.0), None);
        assert_eq!(logical(-640.0, 360.0, 1.0), None);
        assert_eq!(logical(640.0, 360.0, 0.0), None);
        assert_eq!(logical(640.0, 360.0, f32::NAN), None);
        assert_eq!(SurfaceSize::new(PhySize::new(8193, 1), 8192), None);
        assert_eq!(SurfaceSize::new(PhySize::new(1, 0), 8192), None);
        assert_eq!(
            SurfaceSize::clamped(PhySize::new(0, 9000), 8192).get(),
            PhySize::new(1, 8192)
        );
    }
}
