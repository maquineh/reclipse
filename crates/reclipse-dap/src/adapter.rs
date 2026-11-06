//! DAP request dispatch against a [`Backend`].
//!
//! Supported flow: `initialize` → `launch`|`attach` → `setBreakpoints` →
//! `configurationDone` → `threads`/`stackTrace`/`scopes`/`variables`/
//! `evaluate`/`continue`/`next`/`stepIn`/`stepOut`/`pause` → `disconnect`.
//! Unknown commands fail cleanly instead of hanging the client.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct DapThread {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DapFrame {
    pub id: u64,
    pub name: String,
    pub path: Option<String>,
    pub line: u64,
}

#[derive(Debug, Clone)]
pub struct DapScope {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DapVar {
    pub name: String,
    pub value: String,
    pub var_type: String,
}

#[derive(Debug, Clone)]
pub struct DapBreakpoint {
    pub id: u64,
    pub verified: bool,
    pub line: u64,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Stopped { reason: String, thread_id: u64 },
    Continued { thread_id: u64 },
    Terminated,
    Output { text: String },
}

/// Synchronous JDWP-backed behavior. The bridge polls the async session and
/// blocks only for `evaluate` (bounded wait).
pub trait Backend {
    fn launch(&mut self, main: &str, classpath: &str, cwd: &str) -> Result<(), String>;
    fn attach(&mut self, host: &str, port: u16) -> Result<(), String>;
    fn set_breakpoints(&mut self, path: &str, lines: &[u32]) -> Vec<DapBreakpoint>;
    fn configuration_done(&mut self) -> Result<(), String>;
    fn threads(&mut self) -> Vec<DapThread>;
    fn stack_trace(&mut self, thread_id: u64) -> Result<Vec<DapFrame>, String>;
    fn scopes(&mut self, frame_id: u64) -> Result<Vec<DapScope>, String>;
    fn variables(&mut self, scope_id: u64) -> Result<Vec<DapVar>, String>;
    fn evaluate(&mut self, expression: &str, frame_id: Option<u64>) -> Result<String, String>;
    fn step(&mut self, thread_id: u64, kind: StepKind) -> Result<(), String>;
    fn pause(&mut self, thread_id: u64) -> Result<(), String>;
    fn disconnect(&mut self);
    fn take_events(&mut self) -> Vec<BackendEvent>;
}

#[derive(Debug, Clone, Copy)]
pub enum StepKind {
    Continue,
    Next,
    In,
    Out,
}

pub struct Adapter<B: Backend> {
    backend: B,
    seq: u64,
    next_breakpoint_id: u64,
}

impl<B: Backend> Adapter<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            seq: 0,
            next_breakpoint_id: 1,
        }
    }

    fn response(&mut self, request_seq: u64, command: &str, body: Result<Value, String>) -> Value {
        let (success, payload) = match body {
            Ok(body) => (true, json!({ "body": body })),
            Err(message) => (false, json!({ "message": message })),
        };
        let mut response = json!({
            "seq": self.next_seq(),
            "type": "response",
            "request_seq": request_seq,
            "success": success,
            "command": command,
        });
        response.as_object_mut().unwrap().extend(
            payload
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        response
    }

    fn event(&mut self, name: &str, body: Value) -> Value {
        json!({
            "seq": self.next_seq(),
            "type": "event",
            "event": name,
            "body": body,
        })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn arg<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
        request.get("arguments")?.get(name)
    }

    fn arg_str(request: &Value, name: &str) -> String {
        Self::arg(request, name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn arg_u64(request: &Value, name: &str) -> u64 {
        Self::arg(request, name)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// Handle one request; returns the response plus any queued events.
    pub fn handle(&mut self, request: Value) -> Vec<Value> {
        let seq = request.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let command = request
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut out = Vec::new();
        let response = match command.as_str() {
            "initialize" => self.response(
                seq,
                &command,
                Ok(json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsEvaluateForHovers": false,
                    "supportsStepBack": false,
                    "supportsSetVariable": false,
                })),
            ),
            "launch" => {
                let main = Self::arg_str(&request, "mainClass");
                let classpath = Self::arg_str(&request, "classPath");
                let cwd = Self::arg_str(&request, "cwd");
                let result = if main.is_empty() {
                    Err("launch exige mainClass".to_owned())
                } else {
                    self.backend.launch(&main, &classpath, &cwd)
                };
                match result {
                    Ok(()) => {
                        out.push(self.event("initialized", json!({})));
                        self.response(seq, &command, Ok(json!({})))
                    }
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "attach" => {
                let host = Self::arg_str(&request, "host");
                let port = Self::arg(&request, "port")
                    .and_then(Value::as_u64)
                    .unwrap_or(5005) as u16;
                let result = if host.is_empty() {
                    Err("attach exige host".to_owned())
                } else {
                    self.backend.attach(&host, port)
                };
                match result {
                    Ok(()) => {
                        out.push(self.event("initialized", json!({})));
                        self.response(seq, &command, Ok(json!({})))
                    }
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "setBreakpoints" => {
                let path = Self::arg(&request, "source")
                    .and_then(|source| source.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let lines: Vec<u32> = Self::arg(&request, "breakpoints")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("line").and_then(Value::as_u64))
                            .map(|line| line as u32)
                            .collect()
                    })
                    .unwrap_or_default();
                if path.is_empty() {
                    self.response(
                        seq,
                        &command,
                        Err("setBreakpoints exige source.path".to_owned()),
                    )
                } else {
                    let mut breakpoints = Vec::new();
                    for mut breakpoint in self.backend.set_breakpoints(&path, &lines) {
                        breakpoint.id = self.next_breakpoint_id;
                        self.next_breakpoint_id += 1;
                        breakpoints.push(json!({
                            "id": breakpoint.id,
                            "verified": breakpoint.verified,
                            "line": breakpoint.line,
                        }));
                    }
                    self.response(seq, &command, Ok(json!({ "breakpoints": breakpoints })))
                }
            }
            "configurationDone" => {
                let result = self.backend.configuration_done();
                self.response(seq, &command, result.map(|()| json!({})))
            }
            "threads" => {
                let threads: Vec<Value> = self
                    .backend
                    .threads()
                    .into_iter()
                    .map(|thread| json!({ "id": thread.id, "name": thread.name }))
                    .collect();
                self.response(seq, &command, Ok(json!({ "threads": threads })))
            }
            "stackTrace" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                match self.backend.stack_trace(thread_id) {
                    Ok(frames) => {
                        let frames: Vec<Value> = frames
                            .into_iter()
                            .map(|frame| {
                                let mut object = json!({
                                    "id": frame.id,
                                    "name": frame.name,
                                    "line": frame.line,
                                });
                                if let Some(path) = frame.path {
                                    object["source"] = json!({ "path": path });
                                }
                                object
                            })
                            .collect();
                        self.response(
                            seq,
                            &command,
                            Ok(json!({ "stackFrames": frames, "totalFrames": frames.len() })),
                        )
                    }
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "scopes" => {
                let frame_id = Self::arg_u64(&request, "frameId");
                match self.backend.scopes(frame_id) {
                    Ok(scopes) => {
                        let scopes: Vec<Value> = scopes
                            .into_iter()
                            .map(|scope| {
                                json!({ "variablesReference": scope.id, "name": scope.name })
                            })
                            .collect();
                        self.response(seq, &command, Ok(json!({ "scopes": scopes })))
                    }
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "variables" => {
                let scope_id = Self::arg_u64(&request, "variablesReference");
                match self.backend.variables(scope_id) {
                    Ok(vars) => {
                        let vars: Vec<Value> = vars
                            .into_iter()
                            .map(|var| {
                                json!({ "name": var.name, "value": var.value, "type": var.var_type, "variablesReference": 0 })
                            })
                            .collect();
                        self.response(seq, &command, Ok(json!({ "variables": vars })))
                    }
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "evaluate" => {
                let expression = Self::arg_str(&request, "expression");
                let frame_id = Self::arg(&request, "frameId").and_then(Value::as_u64);
                match self.backend.evaluate(&expression, frame_id) {
                    Ok(result) => self.response(
                        seq,
                        &command,
                        Ok(json!({ "result": result, "variablesReference": 0 })),
                    ),
                    Err(error) => self.response(seq, &command, Err(error)),
                }
            }
            "continue" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                let result = self.backend.step(thread_id, StepKind::Continue);
                self.response(
                    seq,
                    &command,
                    result.map(|()| json!({ "allThreadsContinued": false })),
                )
            }
            "next" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                let result = self.backend.step(thread_id, StepKind::Next);
                self.response(seq, &command, result.map(|()| json!({})))
            }
            "stepIn" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                let result = self.backend.step(thread_id, StepKind::In);
                self.response(seq, &command, result.map(|()| json!({})))
            }
            "stepOut" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                let result = self.backend.step(thread_id, StepKind::Out);
                self.response(seq, &command, result.map(|()| json!({})))
            }
            "pause" => {
                let thread_id = Self::arg_u64(&request, "threadId");
                let result = self.backend.pause(thread_id);
                self.response(seq, &command, result.map(|()| json!({})))
            }
            "disconnect" => {
                self.backend.disconnect();
                out.push(self.event("terminated", json!({})));
                self.response(seq, &command, Ok(json!({})))
            }
            other => self.response(
                seq,
                other,
                Err(format!("comando DAP não suportado: {other}")),
            ),
        };
        out.push(response);
        for event in self.backend.take_events() {
            out.push(match event {
                BackendEvent::Stopped { reason, thread_id } => self.event(
                    "stopped",
                    json!({ "reason": reason, "threadId": thread_id, "allThreadsStopped": true }),
                ),
                BackendEvent::Continued { thread_id } => self.event(
                    "continued",
                    json!({ "threadId": thread_id, "allThreadsContinued": false }),
                ),
                BackendEvent::Terminated => self.event("terminated", json!({})),
                BackendEvent::Output { text } => {
                    self.event("output", json!({ "category": "console", "output": text }))
                }
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockBackend {
        events: Vec<BackendEvent>,
        launched: bool,
        breakpoints: Vec<(String, Vec<u32>)>,
        continued: u64,
        disconnected: bool,
    }

    impl Backend for MockBackend {
        fn launch(&mut self, main: &str, _classpath: &str, _cwd: &str) -> Result<(), String> {
            if main.is_empty() {
                return Err("sem main".to_owned());
            }
            self.launched = true;
            Ok(())
        }

        fn attach(&mut self, host: &str, _port: u16) -> Result<(), String> {
            if host.is_empty() {
                return Err("sem host".to_owned());
            }
            self.launched = true;
            Ok(())
        }

        fn set_breakpoints(&mut self, path: &str, lines: &[u32]) -> Vec<DapBreakpoint> {
            self.breakpoints.push((path.to_owned(), lines.to_vec()));
            lines
                .iter()
                .map(|line| DapBreakpoint {
                    id: 0,
                    verified: true,
                    line: u64::from(*line),
                })
                .collect()
        }

        fn configuration_done(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn threads(&mut self) -> Vec<DapThread> {
            vec![DapThread {
                id: 1,
                name: "main".to_owned(),
            }]
        }

        fn stack_trace(&mut self, thread_id: u64) -> Result<Vec<DapFrame>, String> {
            if thread_id != 1 {
                return Err("thread desconhecida".to_owned());
            }
            Ok(vec![DapFrame {
                id: 0,
                name: "main".to_owned(),
                path: Some("/proj/App.java".to_owned()),
                line: 7,
            }])
        }

        fn scopes(&mut self, _frame_id: u64) -> Result<Vec<DapScope>, String> {
            Ok(vec![DapScope {
                id: 1,
                name: "Locals".to_owned(),
            }])
        }

        fn variables(&mut self, _scope_id: u64) -> Result<Vec<DapVar>, String> {
            Ok(vec![DapVar {
                name: "x".to_owned(),
                value: "1".to_owned(),
                var_type: "int".to_owned(),
            }])
        }

        fn evaluate(&mut self, expression: &str, _frame_id: Option<u64>) -> Result<String, String> {
            Ok(format!("eval:{expression}"))
        }

        fn step(&mut self, _thread_id: u64, _kind: StepKind) -> Result<(), String> {
            self.continued += 1;
            Ok(())
        }

        fn pause(&mut self, _thread_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn disconnect(&mut self) {
            self.disconnected = true;
        }

        fn take_events(&mut self) -> Vec<BackendEvent> {
            std::mem::take(&mut self.events)
        }
    }

    fn request(seq: u64, command: &str, arguments: Value) -> Value {
        json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })
    }

    fn response_of(messages: &[Value]) -> &Value {
        messages
            .iter()
            .find(|message| message.get("type").and_then(Value::as_str) == Some("response"))
            .expect("sem resposta")
    }

    #[test]
    fn full_session_flow() {
        let mut adapter = Adapter::new(MockBackend::default());
        // initialize
        let messages = adapter.handle(request(1, "initialize", json!({})));
        assert!(response_of(&messages)["success"].as_bool().unwrap());
        // launch emits initialized + response
        let messages = adapter.handle(request(
            2,
            "launch",
            json!({ "mainClass": "com.acme.App", "classPath": "c", "cwd": "w" }),
        ));
        assert!(messages
            .iter()
            .any(|message| message.get("event").and_then(Value::as_str) == Some("initialized")));
        assert!(response_of(&messages)["success"].as_bool().unwrap());
        // breakpoints get sequential ids
        let messages = adapter.handle(request(
            3,
            "setBreakpoints",
            json!({ "source": { "path": "/proj/App.java" },
                     "breakpoints": [{ "line": 7 }, { "line": 9 }] }),
        ));
        let body = &response_of(&messages)["body"];
        assert_eq!(body["breakpoints"][0]["id"], json!(1));
        assert_eq!(body["breakpoints"][1]["line"], json!(9));
        // threads / frames / scopes / variables
        let messages = adapter.handle(request(4, "threads", json!({})));
        assert_eq!(
            response_of(&messages)["body"]["threads"][0]["name"],
            json!("main")
        );
        let messages = adapter.handle(request(5, "stackTrace", json!({ "threadId": 1 })));
        assert_eq!(
            response_of(&messages)["body"]["stackFrames"][0]["line"],
            json!(7)
        );
        let messages = adapter.handle(request(6, "scopes", json!({ "frameId": 0 })));
        assert_eq!(
            response_of(&messages)["body"]["scopes"][0]["name"],
            json!("Locals")
        );
        let messages = adapter.handle(request(7, "variables", json!({ "variablesReference": 1 })));
        assert_eq!(
            response_of(&messages)["body"]["variables"][0]["value"],
            json!("1")
        );
        // evaluate + continue + disconnect
        let messages = adapter.handle(request(8, "evaluate", json!({ "expression": "x + 1" })));
        assert_eq!(
            response_of(&messages)["body"]["result"],
            json!("eval:x + 1")
        );
        adapter.handle(request(9, "continue", json!({ "threadId": 1 })));
        let messages = adapter.handle(request(10, "disconnect", json!({})));
        assert!(messages
            .iter()
            .any(|message| message.get("event").and_then(Value::as_str) == Some("terminated")));
    }

    #[test]
    fn rejects_bad_requests_cleanly() {
        let mut adapter = Adapter::new(MockBackend::default());
        let messages = adapter.handle(request(1, "launch", json!({})));
        assert!(!response_of(&messages)["success"].as_bool().unwrap());
        let messages = adapter.handle(request(2, "frobnicate", json!({})));
        let response = response_of(&messages);
        assert!(!response["success"].as_bool().unwrap());
        let messages = adapter.handle(request(3, "stackTrace", json!({ "threadId": 99 })));
        assert!(!response_of(&messages)["success"].as_bool().unwrap());
        let messages = adapter.handle(request(4, "setBreakpoints", json!({})));
        assert!(!response_of(&messages)["success"].as_bool().unwrap());
    }

    #[test]
    fn surfaces_backend_events() {
        let mut backend = MockBackend::default();
        backend.events.push(BackendEvent::Stopped {
            reason: "breakpoint".to_owned(),
            thread_id: 1,
        });
        backend.events.push(BackendEvent::Output {
            text: "hi\n".to_owned(),
        });
        let mut adapter = Adapter::new(backend);
        let messages = adapter.handle(request(1, "threads", json!({})));
        assert!(messages
            .iter()
            .any(|message| message.get("event").and_then(Value::as_str) == Some("stopped")));
        assert!(messages
            .iter()
            .any(|message| message.get("event").and_then(Value::as_str) == Some("output")));
    }
}
