//! Native JDWP debugger for Reclipse.
//!
//! The crate is intentionally UI-agnostic: the UI sends [`DebugCommand`]s and
//! polls [`DebugEvent`]s while a background thread owns the JDWP socket and the
//! target JVM process.

mod model;
mod protocol;
mod session;

pub use model::{
    BreakpointSpec, DebugCommand, DebugEvent, DebugSession, FrameInfo, LaunchSpec, SourceLocation,
    StepKind, StopInfo, StopReason, ThreadInfo, VariableInfo,
};
