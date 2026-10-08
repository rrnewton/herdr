use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

/// Server API feature: input requests honor an optional `expect` object
/// (see `InputExpectation`) and refuse mismatches with `expectation_failed`.
pub const SERVER_FEATURE_INPUT_EXPECT: &str = "input-expect";

/// Additive API features this server implements, reported in `ping` as
/// `features`. Clients must gate optional request fields on these names, not
/// on the protocol number: request params do not reject unknown fields, so an
/// older server would silently ignore a field it does not understand.
pub fn server_features() -> Vec<String> {
    vec![SERVER_FEATURE_INPUT_EXPECT.to_string()]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
}
