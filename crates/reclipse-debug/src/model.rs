//! Public model shared by the UI and the JDWP worker.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointSpec {
    pub path: PathBuf,
    /// 1-based source line.
    pub line: usize,
    /// Binary class name, e.g. `com.acme.App` or `com.acme.App$Inner`.
    pub class_name: String,
    /// Optional boolean expression evaluated client-side when the breakpoint hits.
    pub condition: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub java: PathBuf,
    pub classpath: String,
    pub main_class: String,
    pub jvm_args: Vec<String>,
    pub program_args: Vec<String>,
    pub env: HashMap<String, String>,
    pub workdir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Into,
    Over,
    Out,
}

#[derive(Debug, Clone)]
pub enum DebugCommand {
    SetBreakpoint(BreakpointSpec),
    ClearBreakpoint { path: PathBuf, line: usize },
    Resume,
    Step(StepKind),
    Pause,
    Disconnect,
    SelectFrame { thread_id: u64, frame_index: usize },
    Evaluate { expression: String },
    EvaluateWatches { expressions: Vec<String> },
    RedefineClasses { classes: Vec<(String, Vec<u8>)> },
    SetPauseOnExceptions(bool),
    SetExceptionConfig(ExceptionConfig),
}

/// Filter for exception breakpoints: which exceptions suspend the debuggee.
#[derive(Debug, Clone)]
pub struct ExceptionConfig {
    /// Master switch (the "Pausar em exceções" checkbox).
    pub enabled: bool,
    /// Suspend on exceptions caught by the debuggee.
    pub caught: bool,
    /// Suspend on uncaught exceptions.
    pub uncaught: bool,
    /// Class name filters (`NullPointerException`,
    /// `java.lang.IllegalArgumentException`, ...). Empty matches every
    /// exception; subclasses of a listed type also match.
    pub types: Vec<String>,
}

impl Default for ExceptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            caught: false,
            uncaught: true,
            types: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint,
    Step,
    Pause,
    Exception,
    Entry,
}

impl StopReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Breakpoint => "breakpoint",
            Self::Step => "passo",
            Self::Pause => "pausado",
            Self::Exception => "exceção",
            Self::Entry => "entrada",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct SourceLocation {
    /// JVM signature, e.g. `Lcom/acme/App;`.
    pub signature: String,
    pub source_file: Option<String>,
    /// 1-based source line (0 when the line table is unavailable).
    pub line: usize,
    pub method: String,
    pub code_index: u64,
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub index: usize,
    pub thread_id: u64,
    pub frame_id: u64,
    pub location: SourceLocation,
    /// Raw JDWP IDs used to fetch locals; not displayed by the UI.
    pub type_id: u64,
    pub method_id: u64,
}

#[derive(Debug, Clone)]
pub struct VariableInfo {
    pub name: String,
    pub value: String,
    pub type_name: String,
}

#[derive(Debug, Clone)]
pub struct StopInfo {
    pub reason: StopReason,
    pub thread: ThreadInfo,
    pub threads: Vec<ThreadInfo>,
    pub frames: Vec<FrameInfo>,
    pub variables: Vec<VariableInfo>,
    pub location: Option<SourceLocation>,
    pub exception: Option<String>,
    /// Present when a conditional breakpoint's expression could not be evaluated.
    pub condition_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WatchValue {
    pub expression: String,
    pub value: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DebugEvent {
    Status(String),
    Started {
        description: String,
    },
    Stopped(Box<StopInfo>),
    Variables {
        frame_index: usize,
        variables: Vec<VariableInfo>,
    },
    Resumed,
    Output(String),
    BreakpointResolved {
        path: PathBuf,
        line: usize,
    },
    BreakpointError {
        path: PathBuf,
        line: usize,
        message: String,
    },
    EvaluateResult {
        expression: String,
        value: Option<String>,
        error: Option<String>,
    },
    WatchValues(Vec<WatchValue>),
    Exited {
        message: String,
    },
    Error(String),
}

pub struct DebugSession {
    tx: Sender<DebugCommand>,
    rx: Receiver<DebugEvent>,
}

impl std::fmt::Debug for DebugSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DebugSession")
    }
}

impl DebugSession {
    pub fn launch(
        spec: LaunchSpec,
        breakpoints: Vec<BreakpointSpec>,
        pause_on_exceptions: bool,
    ) -> Result<Self, String> {
        if spec.main_class.trim().is_empty() {
            return Err("classe main vazia para o debugger".to_owned());
        }
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || {
            if let Err(error) = session::run_launch(
                spec,
                breakpoints,
                pause_on_exceptions,
                command_rx,
                event_tx.clone(),
            ) {
                let _ = event_tx.send(DebugEvent::Error(error));
            }
        });
        Ok(Self {
            tx: command_tx,
            rx: event_rx,
        })
    }

    pub fn attach(
        address: String,
        breakpoints: Vec<BreakpointSpec>,
        pause_on_exceptions: bool,
    ) -> Result<Self, String> {
        if address.trim().is_empty() {
            return Err("endereço de attach vazio (use host:porta)".to_owned());
        }
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || {
            if let Err(error) = session::run_attach(
                address,
                breakpoints,
                pause_on_exceptions,
                command_rx,
                event_tx.clone(),
            ) {
                let _ = event_tx.send(DebugEvent::Error(error));
            }
        });
        Ok(Self {
            tx: command_tx,
            rx: event_rx,
        })
    }

    pub fn send(&self, command: DebugCommand) -> bool {
        self.tx.send(command).is_ok()
    }

    /// Clone the command sender so background work (e.g. hot swap compilation)
    /// can talk to the same debug session.
    pub fn sender(&self) -> Sender<DebugCommand> {
        self.tx.clone()
    }

    pub fn poll(&self) -> Option<DebugEvent> {
        self.rx.try_recv().ok()
    }
}

impl Drop for DebugSession {
    fn drop(&mut self) {
        let _ = self.tx.send(DebugCommand::Disconnect);
    }
}
