/// Put text on the clipboard, and say whether it was written. Off macOS,
/// nothing is.
pub fn copy_to_clipboard(data: &str) -> bool {
    crate::platform::copy_to_clipboard(data)
}

/// The clipboard's plain text, if it holds any. Off macOS, none.
pub fn clipboard_text() -> Option<String> {
    crate::platform::clipboard_text()
}
