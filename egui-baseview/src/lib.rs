#[cfg(all(feature = "frame-log", target_os = "macos"))]
pub mod frame_log;
mod renderer;
mod translate;
mod window;

pub use baseview;
pub use renderer::{GraphicsConfig, WgpuSetup};
#[cfg(feature = "wgpu")]
pub use renderer::SharedGpuContext;
pub use window::{EguiWindow, EguiWindowSettings, PassRequest, Queue, SizeSource, StopNotice};
