pub fn copy_to_clipboard(data: &str) {
    crate::platform::copy_to_clipboard(data)
}

/// The clipboard's text, if it holds any: a copied file's path where it holds
/// a file, as Finder's copy does, or else its plain text. Off macOS, none.
pub fn clipboard_text() -> Option<String> {
    crate::platform::clipboard_text()
}
