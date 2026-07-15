//! Shared client-side scroll-routing policy.
//!
//! Every client that renders a locally-mirrored terminal (the single-pane
//! responsive mirror today, the full mirror TUI in later phases) must make the
//! same decision for each scroll input: handle it locally against the client's
//! own viewport with zero round-trip, or forward it to the server so the live
//! application can act on it. Keeping that decision in one pure, unit-tested
//! function stops the code paths from drifting apart (code-review Finding 1).

use crate::pane::InputState;
use crate::protocol::AttachScrollSource;

/// Where a scroll input should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollDisposition {
    /// Scroll the client's local viewport; no network round-trip.
    Local,
    /// Forward the original input to the server; the remote application owns the
    /// scroll region (e.g. a full-screen app on the alternate screen).
    ForwardToServer,
}

/// Decide how to route a scroll input against a locally-mirrored terminal.
///
/// * Mouse-wheel scrolls always drive the local scrollback viewport.
/// * Page keys drive the local scrollback viewport only when the terminal input
///   state says plain page keys belong to host scrollback. Unknown state
///   fails open to the child application.
pub(crate) fn scroll_disposition(
    source: &AttachScrollSource,
    input_state: Option<InputState>,
) -> ScrollDisposition {
    match source {
        AttachScrollSource::Wheel => ScrollDisposition::Local,
        AttachScrollSource::PageKey { .. }
            if input_state.is_some_and(InputState::plain_page_keys_use_host_scrollback) =>
        {
            ScrollDisposition::Local
        }
        AttachScrollSource::PageKey { .. } => ScrollDisposition::ForwardToServer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_always_scrolls_locally() {
        assert_eq!(
            scroll_disposition(&AttachScrollSource::Wheel, None),
            ScrollDisposition::Local
        );
        assert_eq!(
            scroll_disposition(&AttachScrollSource::Wheel, Some(host_scrollback_state())),
            ScrollDisposition::Local
        );
    }

    #[test]
    fn page_keys_scroll_locally_only_for_host_scrollback_state() {
        let page = AttachScrollSource::PageKey { input: vec![0x1b] };
        assert_eq!(
            scroll_disposition(&page, Some(host_scrollback_state())),
            ScrollDisposition::Local
        );
        assert_eq!(
            scroll_disposition(
                &page,
                Some(InputState {
                    application_cursor: true,
                    ..host_scrollback_state()
                })
            ),
            ScrollDisposition::ForwardToServer
        );
        assert_eq!(
            scroll_disposition(&page, None),
            ScrollDisposition::ForwardToServer
        );
    }

    fn host_scrollback_state() -> InputState {
        InputState {
            alternate_screen: false,
            application_cursor: false,
            bracketed_paste: false,
            focus_reporting: false,
            mouse_protocol_mode: crate::input::MouseProtocolMode::None,
            mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
            mouse_alternate_scroll: false,
            modify_other_keys: false,
        }
    }
}
