//! Conditional input: the optional `expect` precondition on input requests.
//!
//! A client that checks which program a pane hosts and then sends input by
//! pane id races against pane replacement: between its check and the write,
//! the pane id can come to name a different terminal (for example after a
//! server restart restores a pane under the same public id). `expect` closes
//! that window. The client passes the terminal id it verified, and the handler
//! compares it with the terminal the pane hosts *now*, in the same handler
//! invocation that performs the write.
//!
//! Why this is atomic with respect to pane replacement: API requests reach
//! `App::handle_api_request` one at a time on the app thread, and each input
//! handler runs to completion with `&mut App`. Pane and terminal tables are
//! only mutated through `App`, so no pane close, split, move, restore, or
//! terminal replacement can interleave between this check and the handler's
//! `try_send_bytes`. The bytes are queued on the runtime of the very terminal
//! that was checked, so even if that pane is replaced right after the handler
//! returns, the queued bytes (including `agent.prompt`'s delayed Enter) can
//! only reach the checked terminal's PTY, never the replacement's.
//!
//! What this does NOT cover: a program exiting, or exec-ing a different
//! program, inside the same terminal. That happens inside the PTY's process
//! tree, outside Herdr's pane table, and leaves the terminal id unchanged.
//! Callers that care must still verify the foreground program themselves.

use crate::api::schema::{ErrorBody, InputExpectation, EXPECTATION_FAILED_ERROR_CODE};
use crate::app::App;
use crate::layout::PaneId;

impl App {
    /// Check an input request's optional `expect` against the pane resolved
    /// from its target. Call after resolving the target and before writing any
    /// byte; on `Err` the caller must return the error and write nothing.
    pub(super) fn check_input_expectation(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
        target: &str,
        expect: Option<&InputExpectation>,
    ) -> Result<(), ErrorBody> {
        let Some(expect) = expect else {
            return Ok(());
        };
        let Some(expected_terminal_id) = expect.terminal_id.as_deref() else {
            return Err(ErrorBody {
                code: "invalid_expectation".into(),
                message: "expect must set at least one field (terminal_id)".into(),
            });
        };
        let actual_terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id);
        let actual = actual_terminal_id
            .as_ref()
            .map(|terminal_id| terminal_id.as_str());
        if actual == Some(expected_terminal_id) {
            return Ok(());
        }
        Err(ErrorBody {
            code: EXPECTATION_FAILED_ERROR_CODE.into(),
            message: format!(
                "input expectation failed for {target}: expected terminal {expected_terminal_id}, actual terminal {}; nothing was written",
                actual.unwrap_or("none"),
            ),
        })
    }
}
