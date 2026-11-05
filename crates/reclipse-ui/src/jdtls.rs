//! Optional JDT Language Server integration (precision mode).
//!
//! Disabled unless `RECLIPSE_JDTLS` points at a `jdtls` launcher executable.
//! When enabled, a background worker owns the server process and speaks LSP
//! over stdio with `Content-Length` framing. The UI polls
//! [`JdtlsEvent`]s and shows server diagnostics in the Problems panel.
//! The native engine stays the default; JDT only *adds* diagnostics.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// How to reach the server. Only `RECLIPSE_JDTLS` opts in.
#[derive(Debug, Clone)]
pub struct JdtlsConfig {
    /// Launcher executable (script or binary accepting `-data <dir>`).
    pub command: String,
    /// Workspace storage directory for the server.
    pub workspace: PathBuf,
}

impl JdtlsConfig {
    pub fn from_env(project_root: &Path) -> Option<Self> {
        let command = std::env::var("RECLIPSE_JDTLS").ok()?;
        let command = command.trim().to_owned();
        if command.is_empty() {
            return None;
        }
        let workspace = std::env::var("RECLIPSE_JDTLS_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root.join(".reclipse-jdtls"));
        Some(Self { command, workspace })
    }
}

#[derive(Debug, Clone)]
pub struct JdtDiagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub severity: String,
    pub message: String,
}

#[derive(Debug)]
pub enum JdtlsCommand {
    DidOpen {
        path: PathBuf,
        text: String,
        version: i32,
    },
    DidChange {
        path: PathBuf,
        text: String,
        version: i32,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum JdtlsEvent {
    Log(String),
    Ready,
    Diagnostics {
        file: PathBuf,
        items: Vec<JdtDiagnostic>,
    },
    Exited(String),
}

pub struct JdtlsHandle {
    pub events: Receiver<JdtlsEvent>,
    commands: Sender<JdtlsCommand>,
}

impl JdtlsHandle {
    pub fn send(&self, command: JdtlsCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for JdtlsHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(JdtlsCommand::Shutdown);
    }
}

/// Spawn the server worker. The worker owns the child process and its stdio;
/// dropping the handle asks it to shut down.
pub fn spawn(config: &JdtlsConfig, root_uri: &str) -> Result<JdtlsHandle, String> {
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let config = config.clone();
    let root_uri = root_uri.to_owned();
    thread::spawn(move || {
        run_server(&config, &root_uri, command_rx, &event_tx);
    });
    Ok(JdtlsHandle {
        events: event_rx,
        commands: command_tx,
    })
}

fn run_server(
    config: &JdtlsConfig,
    root_uri: &str,
    commands: Receiver<JdtlsCommand>,
    events: &Sender<JdtlsEvent>,
) {
    if let Err(error) = std::fs::create_dir_all(&config.workspace) {
        let _ = events.send(JdtlsEvent::Exited(format!(
            "JDT: não foi possível criar workspace {}: {error}",
            config.workspace.display()
        )));
        return;
    }
    let mut child = match spawn_child(&config.command, &config.workspace) {
        Ok(child) => child,
        Err(error) => {
            let _ = events.send(JdtlsEvent::Exited(format!("JDT: {error}")));
            return;
        }
    };
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = events.send(JdtlsEvent::Exited("JDT: stdin indisponível".to_owned()));
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = events.send(JdtlsEvent::Exited("JDT: stdout indisponível".to_owned()));
            return;
        }
    };

    let (incoming_tx, incoming_rx) = mpsc::channel();
    thread::spawn(move || {
        read_loop(stdout, incoming_tx);
    });

    let mut writer = FramedWriter::new(stdin);
    let mut next_id: u64 = 1;
    // Initialize handshake.
    let init_id = next_id;
    next_id += 1;
    let mut initialized = false;
    if writer.send(&initialize_params(init_id, root_uri)).is_err() {
        let _ = events.send(JdtlsEvent::Exited(
            "JDT: falha ao enviar initialize".to_owned(),
        ));
        kill(&mut child);
        return;
    }

    let mut opened: HashMap<PathBuf, i32> = HashMap::new();
    loop {
        // Prefer server messages to keep diagnostics flowing; poll commands.
        match incoming_rx.try_recv() {
            Ok(message) => {
                handle_server_message(&message, &mut writer, events, &mut initialized);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
        match commands.try_recv() {
            Ok(JdtlsCommand::DidOpen {
                path,
                text,
                version,
            }) => {
                let already = opened.insert(path.clone(), version);
                if already == Some(version) {
                    continue;
                }
                // didOpen with the same URI re-registers the document.
                let uri = file_uri(&path);
                writer.send_notification(
                    "textDocument/didOpen",
                    &serde_json::json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": "java",
                            "version": version,
                            "text": text,
                        }
                    }),
                );
            }
            Ok(JdtlsCommand::DidChange {
                path,
                text,
                version,
            }) => {
                opened.insert(path.clone(), version);
                let uri = file_uri(&path);
                writer.send_notification(
                    "textDocument/didChange",
                    &serde_json::json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                );
            }
            Ok(JdtlsCommand::Shutdown) => {
                let id = next_id;
                next_id += 1;
                writer.send_request(id, "shutdown", &serde_json::Value::Null);
                // Best effort: give the server a moment, then exit.
                thread::sleep(Duration::from_millis(800));
                writer.send_notification("exit", &serde_json::Value::Null);
                thread::sleep(Duration::from_millis(400));
                kill(&mut child);
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Check liveness without busy-looping.
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = events.send(JdtlsEvent::Exited(format!(
                            "JDT: processo encerrou ({status})"
                        )));
                        return;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(30)),
                    Err(_) => {
                        kill(&mut child);
                        return;
                    }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                writer.send_notification("exit", &serde_json::Value::Null);
                kill(&mut child);
                return;
            }
        }
    }
    kill(&mut child);
}

fn spawn_child(command: &str, workspace: &Path) -> Result<Child, String> {
    let mut child = if command.ends_with(".cmd") || command.ends_with(".bat") {
        let mut child = Command::new("cmd");
        child.args(["/C", command, "-data", &workspace.display().to_string()]);
        child
    } else {
        let mut child = Command::new(command);
        child.arg("-data").arg(workspace);
        child
    };
    child
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("falha ao iniciar '{command}': {error} (instale o jdt-language-server e defina RECLIPSE_JDTLS)"))
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn initialize_params(id: u64, root_uri: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": false }
                },
                "workspace": { "workspaceFolders": true }
            }
        }
    })
    .to_string()
}

fn handle_server_message(
    raw: &str,
    writer: &mut FramedWriter,
    events: &Sender<JdtlsEvent>,
    initialized: &mut bool,
) {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    if let Some(result) = message.get("result") {
        let _ = result;
        if !*initialized {
            *initialized = true;
            writer.send_notification("initialized", &serde_json::json!({}));
            let _ = events.send(JdtlsEvent::Ready);
        }
        return;
    }
    if message.get("error").is_some() && !*initialized {
        *initialized = true;
        writer.send_notification("initialized", &serde_json::json!({}));
        let _ = events.send(JdtlsEvent::Ready);
        return;
    }
    let Some(method) = message.get("method").and_then(|method| method.as_str()) else {
        return;
    };
    match method {
        "window/logMessage" | "window/showMessage" => {
            if let Some(text) = message
                .get("params")
                .and_then(|params| params.get("message"))
                .and_then(|message| message.as_str())
            {
                let _ = events.send(JdtlsEvent::Log(format!("JDT: {text}")));
            }
        }
        "textDocument/publishDiagnostics" => {
            if let Some((file, items)) = parse_diagnostics(&message) {
                let _ = events.send(JdtlsEvent::Diagnostics { file, items });
            }
        }
        "window/workDoneProgress/create"
        | "workspace/configuration"
        | "client/registerCapability" => {
            if let Some(id) = message.get("id").cloned() {
                let reply = if method == "workspace/configuration" {
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": [null]})
                } else {
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})
                };
                writer.send_raw(&reply.to_string());
            }
        }
        _ => {
            // Unknown server request: answer null so the server never blocks.
            if let Some(id) = message.get("id").cloned() {
                writer.send_raw(
                    &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}).to_string(),
                );
            }
        }
    }
}

fn parse_diagnostics(message: &serde_json::Value) -> Option<(PathBuf, Vec<JdtDiagnostic>)> {
    let params = message.get("params")?;
    let uri = params.get("uri")?.as_str()?;
    let file = uri_to_path(uri)?;
    let mut items = Vec::new();
    for diagnostic in params
        .get("diagnostics")
        .and_then(|diagnostics| diagnostics.as_array())
        .into_iter()
        .flatten()
    {
        let start = diagnostic.get("range").and_then(|range| range.get("start"));
        let line = start
            .and_then(|start| start.get("line"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize
            + 1;
        let column = start
            .and_then(|start| start.get("character"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize
            + 1;
        let severity = match diagnostic
            .get("severity")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1)
        {
            1 => "erro",
            2 => "aviso",
            3 => "info",
            _ => "dica",
        }
        .to_owned();
        let message = diagnostic
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("diagnóstico JDT")
            .to_owned();
        items.push(JdtDiagnostic {
            file: file.clone(),
            line,
            column,
            severity,
            message,
        });
        if items.len() >= 100 {
            break;
        }
    }
    Some((file, items))
}

/// Convert a `file://` URI to a local path (percent-decoding basics).
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let without_scheme = uri.strip_prefix("file://")?;
    // Strip the empty authority (`file:///`) or a host (`file://host/`).
    let path = without_scheme
        .strip_prefix('/')
        .map(|rest| {
            // `file:///C:/...` -> `/C:/...`; keep the leading slash for now.
            format!("/{rest}")
        })
        .unwrap_or_else(|| without_scheme.to_owned());
    let decoded = percent_decode(&path);
    let mut decoded = decoded;
    // Windows drive letters arrive as `/C:/...`; strip the slash.
    if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        decoded.remove(0);
    }
    Some(PathBuf::from(
        decoded.replace('/', std::path::MAIN_SEPARATOR_STR),
    ))
}

/// Convert a local path to a `file://` URI (percent-encodes spaces etc.).
pub fn file_uri(path: &Path) -> String {
    let display = dunce::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
        .replace('\\', "/");
    let mut encoded = String::with_capacity(display.len() + 8);
    for byte in display.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct FramedWriter {
    stdin: ChildStdin,
}

impl FramedWriter {
    fn new(stdin: ChildStdin) -> Self {
        Self { stdin }
    }

    fn send(&mut self, body: &str) -> std::io::Result<()> {
        self.send_raw(body);
        Ok(())
    }

    fn send_raw(&mut self, body: &str) {
        let _ = write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = self.stdin.flush();
    }

    fn send_request(&mut self, id: u64, method: &str, params: &serde_json::Value) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        self.send_raw(&body);
    }

    fn send_notification(&mut self, method: &str, params: &serde_json::Value) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })
        .to_string();
        self.send_raw(&body);
        let _ = self.stdin.flush();
    }
}

fn read_loop(stdout: ChildStdout, incoming: Sender<String>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader) {
            Ok(Some(body)) => {
                if incoming.send(body).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

fn read_message(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let Some(length) = content_length else {
        // Skip bodyless or malformed frames.
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_content_length() {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "x"}).to_string();
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(std::io::Cursor::new(framed.into_bytes()));
        let parsed = read_loop_once(&mut reader);
        assert_eq!(parsed.as_deref(), Some(body.as_str()));
    }

    fn read_loop_once(reader: &mut BufReader<std::io::Cursor<Vec<u8>>>) -> Option<String> {
        // Local copy of the framing logic over a generic reader.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse().ok();
            }
        }
        let length = content_length?;
        let mut body = vec![0u8; length];
        std::io::Read::read_exact(reader, &mut body).ok()?;
        Some(String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn parses_publish_diagnostics() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///C:/proj/src/App.java",
                "diagnostics": [
                    {
                        "range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 12}},
                        "severity": 1,
                        "message": "cannot find symbol"
                    },
                    {
                        "range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 1}},
                        "severity": 2,
                        "message": "unused import"
                    }
                ]
            }
        });
        let (file, items) = parse_diagnostics(&message).unwrap();
        assert!(file.ends_with("App.java"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].line, 5);
        assert_eq!(items[0].column, 9);
        assert_eq!(items[0].severity, "erro");
    }

    #[test]
    fn converts_file_uris_and_paths() {
        let uri = file_uri(Path::new("C:/proj/src/App.java"));
        assert!(uri.starts_with("file:///"));
        let back = uri_to_path(&uri).unwrap();
        assert!(back.ends_with("App.java"));
        assert!(uri_to_path("file:///C:/a%20b/C.java")
            .unwrap()
            .ends_with("a b/C.java"));
        assert!(uri_to_path("https://example.com/x").is_none());
    }

    #[test]
    fn config_is_opt_in() {
        std::env::remove_var("RECLIPSE_JDTLS");
        assert!(JdtlsConfig::from_env(Path::new(".")).is_none());
    }
}
