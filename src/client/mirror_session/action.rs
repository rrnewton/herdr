//! Effect sink for mirror structural mutations (`design-mirror-tui.md` §3.4).
//!
//! The mirror keeps the server app's key→[`crate::app::NavigateAction`] mapping
//! **and** its effect logic: it drives resolved actions and overlay-mode keys
//! through the exact same code paths the interactive app uses
//! ([`crate::app::App::execute_tui_navigate_action`] and
//! [`crate::app::App::handle_non_terminal_key`]). Those paths issue structural
//! session/layout mutations through `App::dispatch_runtime_mutation`, which the
//! mirror captures (`App::captured_runtime_mutations`) rather than applying to
//! its PTY-less replica. The driver then forwards each captured [`Method`] to the
//! authoritative server as a JSON API call through this sink; the replica updates
//! only when the resulting event arrives (server stays authoritative).
//!
//! Because the effect layer is now shared, there is no per-action allowlist to
//! curate: **any** `NavigateAction` the main TUI supports works in the mirror
//! automatically — view-local actions mutate the replica, structural actions are
//! captured and forwarded. The sink stays behind a trait so tests can observe
//! forwarded mutations with a recording double.

use crate::api::schema::Method;

use super::control::{JsonApiClient, JsonApiError};

/// The effect layer for structural mirror actions. The real impl issues JSON API
/// calls; a test double records them.
pub(crate) trait MirrorActionSink {
    /// Dispatch an authoritative mutation to the server.
    fn dispatch(&self, verb: &'static str, method: Method) -> Result<(), JsonApiError>;
}

/// Production sink: forwards mutations over the JSON API control plane.
pub(crate) struct JsonApiSink<'a> {
    pub control: &'a JsonApiClient,
}

impl MirrorActionSink for JsonApiSink<'_> {
    fn dispatch(&self, verb: &'static str, method: Method) -> Result<(), JsonApiError> {
        self.control.mutate(verb, method)?;
        Ok(())
    }
}
