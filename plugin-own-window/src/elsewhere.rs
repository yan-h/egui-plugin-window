use std::ffi::c_void;
use std::sync::{Arc, Weak};

use crate::EditorSize;

/// Off macOS the editor stays in the host's window: the hosts there have not
/// been seen hiding their plug-in windows, and nothing here has been tried on
/// them.
pub(crate) struct OwnWindow;

impl OwnWindow {
    pub(crate) fn new(_name: &str, _editor: Weak<EditorSize>) -> Arc<Self> {
        Arc::new(Self)
    }

    /// # Safety
    ///
    /// None here; the signature is the macOS one's.
    pub(crate) unsafe fn open(
        self: &Arc<Self>,
        _host: *mut c_void,
        _view: *mut c_void,
        _out: bool,
        _closed: Option<Box<dyn Fn() -> bool>>,
    ) {
    }

    pub(crate) fn offered(&self) -> bool {
        false
    }

    pub(crate) fn want(&self, _out: bool) {}

    pub(crate) fn next_frame(&self, _editor: &EditorSize) -> bool {
        false
    }

    pub(crate) fn resize(&self, _size: (u32, u32)) -> Option<(u32, u32)> {
        None
    }

    pub(crate) fn is_out(&self) -> bool {
        false
    }

    pub(crate) fn close(&self) {}
}
