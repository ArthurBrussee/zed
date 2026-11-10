//! quiet-ui: keeps a remote project's agents running on the box while Zed is
//! away, and lets Zed attach to them again and catch up.
//!
//! The relay stores and forwards raw JSON-RPC frames and reads only their id,
//! method and `params.sessionId`, so moving Zed to ACP v2 changes the method
//! names in [`frame`] and the turn-state mapping, not the relay.

pub mod daemon;
pub mod frame;
pub mod log;
mod relay;
mod state;

pub use relay::{AgentIo, AgentSpawner, LinkSink, Relay, TaskSpawner};
pub use state::{
    AgentCommand, AgentSpec, LinkFrame, LinkId, Output, ProcessId, RelayState, SessionSummary,
};

#[cfg(test)]
mod tests;
