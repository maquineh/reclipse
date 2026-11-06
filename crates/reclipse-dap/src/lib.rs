//! Debug Adapter Protocol server for Reclipse (external clients: VS Code, ...).
//!
//! The adapter speaks DAP (`Content-Length` framing over stdio/TCP) and drives
//! the native JDWP backend (`reclipse-debug`) through [`Backend`]. The UI keeps
//! using `reclipse-debug` directly; DAP is only the external-client path.

pub mod adapter;
pub mod bridge;
pub mod frame;

pub use adapter::{
    Adapter, Backend, BackendEvent, DapBreakpoint, DapFrame, DapScope, DapThread, DapVar,
};
pub use bridge::ReclipseBackend;
pub use frame::{read_message, serve_stdio, write_message};
