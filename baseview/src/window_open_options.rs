use std::sync::Arc;

use crate::Size;

/// How the app is told that its view has stopped being seen.
///
/// "Seen" is the test that decides whether a frame runs (macOS): the view and
/// every ancestor shown, in a window whose occlusion state reads visible. The
/// view keeps the last answer, and calls this when it turns from seen to not
/// seen: the window covered, minimized, ordered out or on another Space, the
/// view or an ancestor hidden, or the view taken out of its window. Never at
/// open, never on a reveal, never twice for one loss, and not after the
/// window is closed.
///
/// The answer is asked again at each event that can change it: the window's
/// occlusion notification, `viewDidHide` and `viewDidUnhide` (sent for an
/// ancestor too), a move to another window or to none, and every display
/// link callback, which re-asks it before deciding whether to run a frame.
/// So a lost occlusion notification is repaired by the next link callback
/// while the link fires (it goes on firing for a covered window), and is not
/// caught while it does not (a minimized or ordered-out window's link stops).
///
/// Called on the main thread, never inside the window handler's borrow: a
/// loss found while the handler is running (a frame, an event) is told once
/// that borrow ends, so the app may take its own locks and call its host from
/// it. It can arrive inside the app's own calls that move or hide the view:
/// a view moved into a window not yet on screen reads as not seen, so a
/// plug-in that moves its editor into a window of its own is told at the move.
#[derive(Clone)]
pub struct UnseenNotice(Arc<dyn Fn() + Send + Sync>);

impl UnseenNotice {
    pub fn new(notice: impl Fn() + Send + Sync + 'static) -> Self {
        UnseenNotice(Arc::new(notice))
    }

    pub(crate) fn tell(&self) {
        (self.0)()
    }
}

impl std::fmt::Debug for UnseenNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnseenNotice")
    }
}

impl PartialEq for UnseenNotice {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The dpi scaling policy of the window
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum WindowScalePolicy {
    /// Use the system's dpi scale factor
    #[default]
    SystemScaleFactor,
    /// Use the given dpi scale factor (e.g. `1.0` = 96 dpi)
    ScaleFactor(f64),
}

/// The options for opening a new window
#[derive(Debug, Clone, PartialEq)]
pub struct WindowOpenOptions {
    pub title: String,

    /// The logical size of the window
    ///
    /// These dimensions will be scaled by the scaling policy specified in `scale`. Mouse
    /// position will be passed back as logical coordinates.
    pub size: Size,

    /// The dpi scaling policy
    pub scale: WindowScalePolicy,

    /// Whom the view tells that it stopped being seen, if anyone. See
    /// [`UnseenNotice`].
    pub unseen_notice: Option<UnseenNotice>,
}

impl WindowOpenOptions {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    #[inline]
    pub fn with_size(mut self, width: f64, height: f64) -> Self {
        self.size = Size::new(width, height);
        self
    }

    #[inline]
    pub fn with_scale_policy(mut self, scale: WindowScalePolicy) -> Self {
        self.scale = scale;
        self
    }

    #[inline]
    pub fn with_unseen_notice(mut self, notice: UnseenNotice) -> Self {
        self.unseen_notice = Some(notice);
        self
    }
}

impl Default for WindowOpenOptions {
    fn default() -> Self {
        Self {
            title: String::from("baseview window"),
            size: Size { width: 500.0, height: 400.0 },
            scale: WindowScalePolicy::default(),
            unseen_notice: None,
        }
    }
}
