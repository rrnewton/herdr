mod id;
pub mod output_log;
mod runtime;
mod runtime_registry;
pub mod state;
mod title;

pub use id::TerminalId;
pub use output_log::MirrorLog;
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
