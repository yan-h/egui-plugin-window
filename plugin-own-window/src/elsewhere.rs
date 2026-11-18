use std::ffi::c_void;
use std::sync::Arc;

use crate::{Limits, OwnSize};

/// Off macOS the editor stays in the host's window: the hosts there have not
/// been seen hiding their plug-in windows, and nothing here has been tried on
/// them.
pub struct OwnWindow;

impl OwnWindow {
    pub fn new(_name: &str) -> Arc<Self> {
        Arc::new(Self)
    }

    /// # Safety
    ///
    /// None here; the signature is the macOS one's.
    pub unsafe fn open(
        self: &Arc<Self>,
        _host: *mut c_void,
        _view: *mut c_void,
        _out: bool,
        _closed: Option<Box<dyn Fn() -> bool>>,
    ) {
    }

    pub fn offered(&self) -> bool {
        false
    }

    pub fn want(&self, _out: bool) {}

    pub fn take_size(&self, _limits: Limits) -> Option<OwnSize> {
        None
    }

    pub fn resize(&self, _size: (u32, u32)) -> bool {
        false
    }

    pub fn is_out(&self) -> bool {
        false
    }

    pub fn close(&self) {}
}
