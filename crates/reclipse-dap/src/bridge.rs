//! [`Backend`] over a live [`reclipse_debug::DebugSession`].
//!
//! The async session is driven synchronously: responses use the last cached
//! `Stopped` event, `evaluate` waits (bounded) for its result, and control
//! commands (`continue`/steps) return immediately while the follow-up stop
//! arrives through [`Backend::take_events`]. Variables cover the stopped
//! (top) frame; other frames list without variables.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use reclipse_debug::{
    BreakpointSpec, DebugCommand, DebugEvent, DebugSession, LaunchSpec, StepKind, StopInfo,
};

use crate::adapter::{
    Backend, BackendEvent, DapBreakpoint, DapFrame, DapScope, DapThread, DapVar,
    StepKind as DapStep,
};

pub struct ReclipseBackend {
    session: Option<DebugSession>,
    pending: HashMap<String, Vec<u32>>,
    stop: Option<StopInfo>,
    exited: bool,
}

impl ReclipseBackend {
    pub fn new() -> Self {
        Self {
            session: None,
            pending: HashMap::new(),
            stop: None,
            exited: false,
        }
    }

    fn require_session(&self) -> Result<&DebugSession, String> {
        self.session
            .as_ref()
            .ok_or_else(|| "sessão de debug não iniciada (launch/attach)".to_owned())
    }

    fn specs(&self) -> Vec<BreakpointSpec> {
        let mut specs = Vec::new();
        for (path, lines) in &self.pending {
            let path = std::path::PathBuf::from(path);
            let class_name = class_name_for_source(&path).unwrap_or_default();
            for line in lines {
                specs.push(BreakpointSpec {
                    path: path.clone(),
                    line: *line as usize,
                    class_name: class_name.clone(),
                    condition: None,
                });
            }
        }
        specs
    }

    fn live_breakpoints(&mut self) {
        // Forward breakpoints set after launch as well.
        let Some(session) = &self.session else {
            return;
        };
        for (path, lines) in &self.pending {
            let path = std::path::PathBuf::from(path);
            let class_name = class_name_for_source(&path).unwrap_or_default();
            for line in lines {
                session.send(DebugCommand::SetBreakpoint(BreakpointSpec {
                    path: path.clone(),
                    line: *line as usize,
                    class_name: class_name.clone(),
                    condition: None,
                }));
            }
        }
    }
}

impl Default for ReclipseBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort `package` + top-level type from a `.java` file.
pub fn class_name_for_source(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut package = None;
    let mut top = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package ") {
            package = Some(rest.trim_end_matches(';').trim().to_owned());
        }
        if top.is_none() {
            for keyword in ["class ", "interface ", "enum ", "@interface ", "record "] {
                if let Some(after) = line.strip_prefix(&format!("public {keyword}")).or_else(|| {
                    if line.starts_with(keyword) && !line.starts_with("import") {
                        Some(&line[keyword.len()..])
                    } else {
                        None
                    }
                }) {
                    let name: String = after
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                        .collect();
                    if !name.is_empty() {
                        top = Some(name);
                        break;
                    }
                }
            }
        }
        if package.is_some() && top.is_some() {
            break;
        }
    }
    let top = top.or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    })?;
    Some(match package {
        Some(package) if !package.is_empty() => format!("{package}.{top}"),
        _ => top,
    })
}

impl Backend for ReclipseBackend {
    fn launch(&mut self, main: &str, classpath: &str, cwd: &str) -> Result<(), String> {
        let workdir = if cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(cwd)
        };
        let session = DebugSession::launch(
            LaunchSpec {
                java: std::path::PathBuf::from("java"),
                classpath: classpath.to_owned(),
                main_class: main.to_owned(),
                jvm_args: Vec::new(),
                program_args: Vec::new(),
                env: Default::default(),
                workdir,
            },
            self.specs(),
            false,
        )?;
        self.session = Some(session);
        self.stop = None;
        self.exited = false;
        self.live_breakpoints();
        Ok(())
    }

    fn attach(&mut self, host: &str, port: u16) -> Result<(), String> {
        let session = DebugSession::attach(format!("{host}:{port}"), self.specs(), false)?;
        self.session = Some(session);
        self.stop = None;
        self.exited = false;
        self.live_breakpoints();
        Ok(())
    }

    fn set_breakpoints(&mut self, path: &str, lines: &[u32]) -> Vec<DapBreakpoint> {
        self.pending.insert(path.to_owned(), lines.to_vec());
        self.live_breakpoints();
        lines
            .iter()
            .map(|line| DapBreakpoint {
                id: 0,
                verified: false,
                line: u64::from(*line),
            })
            .collect()
    }

    fn configuration_done(&mut self) -> Result<(), String> {
        self.require_session().map(|_| ())
    }

    fn threads(&mut self) -> Vec<DapThread> {
        self.stop
            .as_ref()
            .map(|stop| {
                stop.threads
                    .iter()
                    .map(|thread| DapThread {
                        id: thread.id,
                        name: thread.name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stack_trace(&mut self, thread_id: u64) -> Result<Vec<DapFrame>, String> {
        let stop = self
            .stop
            .as_ref()
            .ok_or_else(|| "JVM não está pausada".to_owned())?;
        if stop.thread.id != thread_id {
            return Err(format!("thread {thread_id} não está pausada"));
        }
        Ok(stop
            .frames
            .iter()
            .map(|frame| DapFrame {
                id: frame.index as u64,
                name: frame.location.method.clone(),
                path: frame.location.source_file.clone(),
                line: frame.location.line as u64,
            })
            .collect())
    }

    fn scopes(&mut self, frame_id: u64) -> Result<Vec<DapScope>, String> {
        let stop = self
            .stop
            .as_ref()
            .ok_or_else(|| "JVM não está pausada".to_owned())?;
        if (frame_id as usize) >= stop.frames.len() {
            return Err("frame inválido".to_owned());
        }
        // Only the stopped (selected) frame carries variables in StopInfo.
        if frame_id as usize == stop.frames.first().map(|frame| frame.index).unwrap_or(0) {
            Ok(vec![DapScope {
                id: 1,
                name: "Locals".to_owned(),
            }])
        } else {
            Ok(Vec::new())
        }
    }

    fn variables(&mut self, scope_id: u64) -> Result<Vec<DapVar>, String> {
        if scope_id != 1 {
            return Ok(Vec::new());
        }
        let stop = self
            .stop
            .as_ref()
            .ok_or_else(|| "JVM não está pausada".to_owned())?;
        Ok(stop
            .variables
            .iter()
            .map(|variable| DapVar {
                name: variable.name.clone(),
                value: variable.value.clone(),
                var_type: variable.type_name.clone(),
            })
            .collect())
    }

    fn evaluate(&mut self, expression: &str, _frame_id: Option<u64>) -> Result<String, String> {
        let session = self.require_session()?;
        session.send(DebugCommand::Evaluate {
            expression: expression.to_owned(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let drained: Vec<DebugEvent> = {
                let session = self.require_session()?;
                let mut drained = Vec::new();
                while let Some(event) = session.poll() {
                    drained.push(event);
                }
                drained
            };
            for event in drained {
                match event {
                    DebugEvent::EvaluateResult {
                        expression: returned,
                        value,
                        error,
                    } if returned == expression => {
                        if let Some(error) = error {
                            return Err(error);
                        }
                        return Ok(value.unwrap_or_default());
                    }
                    DebugEvent::Stopped(info) => {
                        self.stop = Some(*info);
                    }
                    DebugEvent::Exited { .. } | DebugEvent::Error(_) => {
                        self.exited = true;
                        self.session = None;
                        return Err("sessão encerrada durante o evaluate".to_owned());
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("evaluate excedeu 10 s sem resposta".to_owned())
    }

    fn step(&mut self, thread_id: u64, kind: DapStep) -> Result<(), String> {
        let session = self.require_session()?;
        match kind {
            DapStep::Continue => {
                session.send(DebugCommand::Resume);
            }
            DapStep::Next => {
                session.send(DebugCommand::Step(StepKind::Over));
            }
            DapStep::In => {
                session.send(DebugCommand::Step(StepKind::Into));
            }
            DapStep::Out => {
                session.send(DebugCommand::Step(StepKind::Out));
            }
        }
        let _ = thread_id;
        Ok(())
    }

    fn pause(&mut self, _thread_id: u64) -> Result<(), String> {
        self.require_session()?.send(DebugCommand::Pause);
        Ok(())
    }

    fn disconnect(&mut self) {
        self.session = None;
        self.stop = None;
        self.exited = true;
    }

    fn take_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        // Drain without holding a borrow on `self.session` across `ingest`.
        let drained: Vec<DebugEvent> = {
            let Some(session) = &self.session else {
                return events;
            };
            let mut drained = Vec::new();
            while let Some(event) = session.poll() {
                drained.push(event);
            }
            drained
        };
        for event in drained {
            match event {
                DebugEvent::Stopped(info) => {
                    let reason = match info.reason {
                        reclipse_debug::StopReason::Breakpoint => "breakpoint",
                        reclipse_debug::StopReason::Step => "step",
                        reclipse_debug::StopReason::Pause => "pause",
                        reclipse_debug::StopReason::Exception => "exception",
                        reclipse_debug::StopReason::Entry => "entry",
                    }
                    .to_owned();
                    let thread_id = info.thread.id;
                    self.stop = Some(*info);
                    events.push(BackendEvent::Stopped { reason, thread_id });
                }
                DebugEvent::Resumed => {
                    let thread_id = self.stop.as_ref().map(|stop| stop.thread.id).unwrap_or(0);
                    events.push(BackendEvent::Continued { thread_id });
                }
                DebugEvent::Exited { message } => {
                    self.exited = true;
                    events.push(BackendEvent::Output {
                        text: format!("{message}\n"),
                    });
                    events.push(BackendEvent::Terminated);
                }
                DebugEvent::Output(line) => events.push(BackendEvent::Output {
                    text: format!("{line}\n"),
                }),
                DebugEvent::Error(message) => {
                    events.push(BackendEvent::Output {
                        text: format!("error: {message}\n"),
                    });
                    events.push(BackendEvent::Terminated);
                }
                other => self.ingest(other),
            }
        }
        events
    }
}

impl ReclipseBackend {
    fn ingest(&mut self, event: DebugEvent) {
        match event {
            DebugEvent::Stopped(info) => {
                self.stop = Some(*info);
            }
            DebugEvent::Exited { .. } => {
                self.exited = true;
                self.session = None;
            }
            DebugEvent::Error(_) => {
                self.session = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_class_name_from_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("App.java");
        std::fs::write(&path, "package com.acme;\npublic class App {\n}\n").unwrap();
        assert_eq!(
            class_name_for_source(&path).as_deref(),
            Some("com.acme.App")
        );
        let plain = dir.path().join("Plain.java");
        std::fs::write(&plain, "class Plain {\n}\n").unwrap();
        assert_eq!(class_name_for_source(&plain).as_deref(), Some("Plain"));
        assert!(class_name_for_source(&dir.path().join("Missing.java")).is_none());
    }
}
