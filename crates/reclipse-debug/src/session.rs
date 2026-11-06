//! High-level JDWP debug session running on a dedicated thread.
//!
//! The UI never blocks on the socket: commands are sent through a channel and
//! events are polled from another channel.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::{
    BreakpointSpec, DebugCommand, DebugEvent, FrameInfo, LaunchSpec, SourceLocation, StepKind,
    StopInfo, StopReason, ThreadInfo, VariableInfo,
};
use crate::protocol::{Incoming, JdwpClient, Location, Reader, Value, Writer};

const SUSPEND_ALL: u8 = 2;
const EVENT_SINGLE_STEP: u8 = 1;
const EVENT_BREAKPOINT: u8 = 2;
const EVENT_EXCEPTION: u8 = 4;
const EVENT_CLASS_PREPARE: u8 = 8;
const MOD_CLASS_MATCH: u8 = 5;
const MOD_LOCATION_ONLY: u8 = 7;
const MOD_EXCEPTION_ONLY: u8 = 8;
const MOD_STEP: u8 = 10;
const STEP_LINE: u32 = 1;
const STEP_INTO: u32 = 0;
const STEP_OVER: u32 = 1;
const STEP_OUT: u32 = 2;

pub(crate) fn run_launch(
    spec: LaunchSpec,
    breakpoints: Vec<BreakpointSpec>,
    pause_on_exceptions: bool,
    commands: Receiver<DebugCommand>,
    events: Sender<DebugEvent>,
) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("falha ao reservar porta JDWP: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("falha ao obter porta JDWP: {error}"))?
        .port();
    drop(listener);

    let mut command = Command::new(&spec.java);
    command
        .arg(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:{port}"
        ))
        .arg("-cp")
        .arg(&spec.classpath);
    command.args(&spec.jvm_args);
    command.arg(&spec.main_class);
    command.args(&spec.program_args);
    command
        .current_dir(&spec.workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "falha ao iniciar JVM de debug ({}): {error}",
            spec.java.display()
        )
    })?;
    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(stdout, events.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(stderr, events.clone());
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = None;
    while Instant::now() < deadline {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => {
                if let Ok(Some(status)) = child.try_wait() {
                    let _ = events.send(DebugEvent::Exited {
                        message: format!(
                            "JVM encerrou antes de conectar ao JDWP (código {:?})",
                            status.code()
                        ),
                    });
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let Some(stream) = stream else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "timeout conectando ao JDWP em 127.0.0.1:{port} (a JVM iniciou?)"
        ));
    };
    let client = match JdwpClient::connect(stream) {
        Ok(client) => client,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

    let mut worker = Worker {
        client,
        commands,
        events,
        child: Some(child),
        own_process: true,
        resume_after_start: true,
        pending: Vec::new(),
        requests: HashMap::new(),
        class_prepare: HashMap::new(),
        exception_request: None,
        pause_on_exceptions,
        step_request: None,
        stop: None,
        method_cache: HashMap::new(),
        field_cache: HashMap::new(),
        line_cache: HashMap::new(),
        source_cache: HashMap::new(),
        signature_cache: HashMap::new(),
        object_type_cache: HashMap::new(),
        string_cache: HashMap::new(),
    };
    worker.run(breakpoints)
}

pub(crate) fn run_attach(
    address: String,
    breakpoints: Vec<BreakpointSpec>,
    pause_on_exceptions: bool,
    commands: Receiver<DebugCommand>,
    events: Sender<DebugEvent>,
) -> Result<(), String> {
    let address = normalize_address(&address);
    let mut last_error = None;
    let mut stream = None;
    for socket in address
        .to_socket_addrs()
        .map_err(|error| format!("endereço de debug inválido ({address}): {error}"))?
    {
        match TcpStream::connect_timeout(&socket, Duration::from_secs(5)) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let Some(stream) = stream else {
        return Err(format!(
            "falha ao conectar em {address}: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "nenhum endereço resolvido".to_owned())
        ));
    };
    let client = JdwpClient::connect(stream)?;
    let mut worker = Worker {
        client,
        commands,
        events,
        child: None,
        own_process: false,
        resume_after_start: false,
        pending: Vec::new(),
        requests: HashMap::new(),
        class_prepare: HashMap::new(),
        exception_request: None,
        pause_on_exceptions,
        step_request: None,
        stop: None,
        method_cache: HashMap::new(),
        field_cache: HashMap::new(),
        line_cache: HashMap::new(),
        source_cache: HashMap::new(),
        signature_cache: HashMap::new(),
        object_type_cache: HashMap::new(),
        string_cache: HashMap::new(),
    };
    worker.run(breakpoints)
}

fn spawn_output_reader(reader: impl Read + Send + 'static, events: Sender<DebugEvent>) {
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(Result::ok) {
            if events.send(DebugEvent::Output(line)).is_err() {
                return;
            }
        }
    });
}

fn normalize_address(address: &str) -> String {
    let address = address.trim();
    if let Some(port) = address.strip_prefix(':') {
        format!("127.0.0.1:{port}")
    } else if address.chars().all(|character| character.is_ascii_digit()) {
        format!("127.0.0.1:{address}")
    } else if address.contains(':') {
        address.to_owned()
    } else {
        format!("{address}:5005")
    }
}

struct StopContext {
    thread_id: u64,
    frames: Vec<FrameInfo>,
    variables: Vec<VariableInfo>,
}

struct Worker {
    client: JdwpClient,
    commands: Receiver<DebugCommand>,
    events: Sender<DebugEvent>,
    child: Option<Child>,
    own_process: bool,
    resume_after_start: bool,
    pending: Vec<BreakpointSpec>,
    /// Breakpoint request IDs keyed by `(path, line)`.
    requests: HashMap<(PathBuf, usize), Vec<u32>>,
    /// ClassPrepare request IDs keyed by binary class name.
    class_prepare: HashMap<String, u32>,
    exception_request: Option<u32>,
    pause_on_exceptions: bool,
    step_request: Option<u32>,
    stop: Option<StopContext>,
    method_cache: HashMap<u64, Vec<MethodInfo>>,
    field_cache: HashMap<u64, Vec<FieldInfo>>,
    line_cache: HashMap<(u64, u64), Vec<(u64, u32)>>,
    source_cache: HashMap<u64, Option<String>>,
    signature_cache: HashMap<u64, String>,
    object_type_cache: HashMap<u64, u64>,
    string_cache: HashMap<u64, String>,
}

#[derive(Clone)]
struct MethodInfo {
    id: u64,
    name: String,
    #[allow(dead_code)]
    signature: String,
}

#[derive(Clone)]
struct FieldInfo {
    id: u64,
    name: String,
    signature: String,
    is_static: bool,
}

struct StopRequest {
    reason: StopReason,
    thread_id: u64,
    location: Location,
    exception: Option<String>,
}

impl Worker {
    fn run(&mut self, breakpoints: Vec<BreakpointSpec>) -> Result<(), String> {
        let description = self
            .vm_version()
            .unwrap_or_else(|_| "JVM conectada via JDWP".to_owned());
        for breakpoint in breakpoints {
            if let Err(error) = self.set_breakpoint(breakpoint.clone()) {
                let _ = self.events.send(DebugEvent::BreakpointError {
                    path: breakpoint.path,
                    line: breakpoint.line,
                    message: error,
                });
            }
        }
        if self.pause_on_exceptions {
            if let Err(error) = self.set_exception_request(true) {
                let _ = self.events.send(DebugEvent::Status(format!(
                    "não foi possível pausar em exceções: {error}"
                )));
            }
        }
        let _ = self.events.send(DebugEvent::Started {
            description: description.clone(),
        });
        if self.resume_after_start {
            self.vm_resume()?;
            let _ = self.events.send(DebugEvent::Resumed);
        }

        loop {
            loop {
                match self.commands.try_recv() {
                    Ok(command) => {
                        if self.handle_command(command)? {
                            self.finish();
                            return Ok(());
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.cleanup();
                        return Ok(());
                    }
                }
            }

            match self.client.poll_event() {
                Ok(Some(Incoming::Command {
                    set: 64,
                    command: 100,
                    data,
                    ..
                })) => {
                    if self.handle_composite(&data)? {
                        self.finish();
                        return Ok(());
                    }
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => {
                    let _ = self.events.send(DebugEvent::Exited { message: error });
                    self.cleanup();
                    return Ok(());
                }
            }

            if let Some(child) = &mut self.child {
                if let Ok(Some(status)) = child.try_wait() {
                    let message = status
                        .code()
                        .map(|code| format!("JVM encerrou (código {code})"))
                        .unwrap_or_else(|| "JVM encerrou".to_owned());
                    self.child = None;
                    let _ = self.events.send(DebugEvent::Exited { message });
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn handle_command(&mut self, command: DebugCommand) -> Result<bool, String> {
        match command {
            DebugCommand::SetBreakpoint(spec) => self.set_breakpoint(spec)?,
            DebugCommand::ClearBreakpoint { path, line } => self.clear_breakpoint(&path, line)?,
            DebugCommand::Resume => {
                self.clear_step_request()?;
                self.stop = None;
                self.vm_resume()?;
                let _ = self.events.send(DebugEvent::Resumed);
            }
            DebugCommand::Step(kind) => {
                self.step(kind)?;
                let _ = self.events.send(DebugEvent::Resumed);
            }
            DebugCommand::Pause => self.pause()?,
            DebugCommand::Disconnect => return Ok(true),
            DebugCommand::SelectFrame {
                thread_id,
                frame_index,
            } => self.select_frame(thread_id, frame_index)?,
            DebugCommand::Evaluate { expression } => self.evaluate(&expression),
            DebugCommand::SetPauseOnExceptions(enable) => {
                self.set_exception_request(enable)?;
                let _ = self.events.send(DebugEvent::Status(if enable {
                    "pausa em exceções não capturadas habilitada".to_owned()
                } else {
                    "pausa em exceções desabilitada".to_owned()
                }));
            }
        }
        Ok(false)
    }

    fn vm_version(&mut self) -> Result<String, String> {
        let data = self.client.request(1, 1, &[])?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let description = reader.string()?;
        let _major = reader.u32()?;
        let _minor = reader.u32()?;
        let vm_version = reader.string()?;
        let vm_name = reader.string()?;
        let label = if vm_name.trim().is_empty() {
            description
        } else {
            format!("{vm_name} {vm_version} · {description}")
        };
        Ok(label)
    }

    fn vm_suspend(&mut self) -> Result<(), String> {
        self.client.request(1, 8, &[]).map(|_| ())
    }

    fn vm_resume(&mut self) -> Result<(), String> {
        self.client.request(1, 9, &[]).map(|_| ())
    }

    fn vm_exit(&mut self) -> Result<(), String> {
        let mut writer = Writer::default();
        writer.u32(0);
        self.client.request(1, 10, &writer.data).map(|_| ())
    }

    fn vm_dispose(&mut self) -> Result<(), String> {
        self.client.request(1, 6, &[]).map(|_| ())
    }

    fn all_threads(&mut self) -> Result<Vec<ThreadInfo>, String> {
        let data = self.client.request(1, 4, &[])?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()?.min(200) as usize;
        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            let id = reader.object()?;
            let name = self
                .thread_name(id)
                .unwrap_or_else(|_| format!("thread {id}"));
            threads.push(ThreadInfo { id, name });
        }
        Ok(threads)
    }

    fn thread_name(&mut self, thread_id: u64) -> Result<String, String> {
        let mut writer = Writer::default();
        writer.id(thread_id, self.client.sizes.object);
        let data = self.client.request(11, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        reader.string()
    }

    fn pause(&mut self) -> Result<(), String> {
        if self.stop.is_some() {
            return Ok(());
        }
        self.vm_suspend()?;
        let threads = self.all_threads().unwrap_or_default();
        let mut candidate = threads
            .iter()
            .find(|thread| thread.name.eq_ignore_ascii_case("main"))
            .map(|thread| thread.id);
        if candidate.is_none() {
            for thread in &threads {
                if self
                    .frames(thread.id)
                    .is_ok_and(|frames| !frames.is_empty())
                {
                    candidate = Some(thread.id);
                    break;
                }
            }
        }
        let Some(thread_id) = candidate else {
            let thread = threads.first().cloned().unwrap_or(ThreadInfo {
                id: 0,
                name: "main".to_owned(),
            });
            let _ = self.events.send(DebugEvent::Stopped(Box::new(StopInfo {
                reason: StopReason::Pause,
                thread,
                threads,
                frames: Vec::new(),
                variables: Vec::new(),
                location: None,
                exception: None,
            })));
            return Ok(());
        };
        let frames = self.frames(thread_id).unwrap_or_default();
        let location = frames.first().map(|frame| frame.location.clone());
        let thread = threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .cloned()
            .unwrap_or(ThreadInfo {
                id: thread_id,
                name: format!("thread {thread_id}"),
            });
        let variables = frames
            .first()
            .map(|frame| self.variables(thread_id, frame).unwrap_or_default())
            .unwrap_or_default();
        self.stop = Some(StopContext {
            thread_id,
            frames: frames.clone(),
            variables: variables.clone(),
        });
        let _ = self.events.send(DebugEvent::Stopped(Box::new(StopInfo {
            reason: StopReason::Pause,
            thread,
            threads,
            frames,
            variables,
            location,
            exception: None,
        })));
        Ok(())
    }

    fn step(&mut self, kind: StepKind) -> Result<(), String> {
        let Some(stop) = &self.stop else {
            return Err("a JVM não está pausada para dar um passo".to_owned());
        };
        let thread_id = stop.thread_id;
        let depth = match kind {
            StepKind::Into => STEP_INTO,
            StepKind::Over => STEP_OVER,
            StepKind::Out => STEP_OUT,
        };
        self.clear_step_request()?;
        let mut writer = Writer::default();
        writer.u8(EVENT_SINGLE_STEP);
        writer.u8(SUSPEND_ALL);
        writer.u32(1);
        writer.u8(MOD_STEP);
        writer.id(thread_id, self.client.sizes.object);
        writer.u32(STEP_LINE);
        writer.u32(depth);
        let data = self.client.request(15, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let request_id = reader.u32()?;
        self.step_request = Some(request_id);
        self.stop = None;
        self.vm_resume()?;
        Ok(())
    }

    fn clear_step_request(&mut self) -> Result<(), String> {
        if let Some(request_id) = self.step_request.take() {
            let mut writer = Writer::default();
            writer.u8(EVENT_SINGLE_STEP);
            writer.u32(request_id);
            let _ = self.client.request(15, 2, &writer.data);
        }
        Ok(())
    }

    fn set_exception_request(&mut self, enable: bool) -> Result<(), String> {
        if !enable {
            if let Some(request_id) = self.exception_request.take() {
                let mut writer = Writer::default();
                writer.u8(EVENT_EXCEPTION);
                writer.u32(request_id);
                let _ = self.client.request(15, 2, &writer.data);
            }
            self.pause_on_exceptions = false;
            return Ok(());
        }
        if self.exception_request.is_some() {
            self.pause_on_exceptions = true;
            return Ok(());
        }
        let mut writer = Writer::default();
        writer.u8(EVENT_EXCEPTION);
        writer.u8(SUSPEND_ALL);
        writer.u32(1);
        writer.u8(MOD_EXCEPTION_ONLY);
        writer.id(0, self.client.sizes.reference_type);
        writer.u8(0);
        writer.u8(1);
        let data = self.client.request(15, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        self.exception_request = Some(reader.u32()?);
        self.pause_on_exceptions = true;
        Ok(())
    }

    fn ensure_class_prepare(&mut self, class_name: &str) -> Result<(), String> {
        if self.class_prepare.contains_key(class_name) {
            return Ok(());
        }
        let mut writer = Writer::default();
        writer.u8(EVENT_CLASS_PREPARE);
        writer.u8(SUSPEND_ALL);
        writer.u32(1);
        writer.u8(MOD_CLASS_MATCH);
        writer.string(class_name);
        let data = self.client.request(15, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        self.class_prepare
            .insert(class_name.to_owned(), reader.u32()?);
        Ok(())
    }

    fn clear_class_prepare(&mut self, class_name: &str) {
        if let Some(request_id) = self.class_prepare.remove(class_name) {
            let mut writer = Writer::default();
            writer.u8(EVENT_CLASS_PREPARE);
            writer.u32(request_id);
            let _ = self.client.request(15, 2, &writer.data);
        }
    }

    fn set_breakpoint(&mut self, spec: BreakpointSpec) -> Result<(), String> {
        if self.pending.iter().any(|pending| {
            pending.path == spec.path
                && pending.line == spec.line
                && pending.class_name == spec.class_name
        }) {
            return Ok(());
        }
        self.pending.push(spec.clone());
        // Register the class prepare hook first so a class that loads between
        // the loaded-class query and the request cannot be missed.
        self.ensure_class_prepare(&spec.class_name)?;
        let signature = signature_for_class(&spec.class_name);
        let classes = self.classes_by_signature(&signature)?;
        for type_id in classes {
            let tag = self.reference_type_tag(type_id).unwrap_or(1);
            self.resolve_breakpoints_for_type(type_id, tag, &spec.class_name)?;
        }
        if !self
            .pending
            .iter()
            .any(|pending| pending.class_name == spec.class_name)
        {
            self.clear_class_prepare(&spec.class_name);
        }
        Ok(())
    }

    fn clear_breakpoint(&mut self, path: &PathBuf, line: usize) -> Result<(), String> {
        self.pending
            .retain(|pending| pending.path != *path || pending.line != line);
        if let Some(request_ids) = self.requests.remove(&(path.clone(), line)) {
            for request_id in request_ids {
                let mut writer = Writer::default();
                writer.u8(EVENT_BREAKPOINT);
                writer.u32(request_id);
                let _ = self.client.request(15, 2, &writer.data);
            }
        }
        let classes: HashSet<&str> = self
            .pending
            .iter()
            .map(|pending| pending.class_name.as_str())
            .collect();
        let stale: Vec<String> = self
            .class_prepare
            .keys()
            .filter(|class_name| !classes.contains(class_name.as_str()))
            .cloned()
            .collect();
        for class_name in stale {
            self.clear_class_prepare(&class_name);
        }
        Ok(())
    }

    fn classes_by_signature(&mut self, signature: &str) -> Result<Vec<u64>, String> {
        let mut writer = Writer::default();
        writer.string(signature);
        let data = self.client.request(1, 2, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()? as usize;
        let mut classes = Vec::with_capacity(count);
        for _ in 0..count {
            classes.push(reader.reference_type()?);
            let _status = reader.u32()?;
        }
        Ok(classes)
    }

    fn reference_type_tag(&mut self, type_id: u64) -> Result<u8, String> {
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        let data = self.client.request(2, 3, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let modifiers = reader.u32()?;
        Ok(if modifiers & 0x0200 != 0 { 2 } else { 1 })
    }

    fn resolve_breakpoints_for_type(
        &mut self,
        type_id: u64,
        type_tag: u8,
        class_name: &str,
    ) -> Result<(), String> {
        let targets: Vec<BreakpointSpec> = self
            .pending
            .iter()
            .filter(|pending| pending.class_name == class_name)
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let methods = self.methods(type_id)?;
        if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
            eprintln!(
                "[reclipse-debug] class {class_name} type {type_id}: {} method(s)",
                methods.len()
            );
        }
        let mut line_map: HashMap<u32, Vec<(u64, u64)>> = HashMap::new();
        for method in &methods {
            match self.line_table(type_id, method.id) {
                Ok(lines) => {
                    if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
                        let sample: Vec<u32> = lines.iter().map(|(_, line)| *line).collect();
                        eprintln!(
                            "[reclipse-debug] method {} ({}): {} line(s) {:?}",
                            method.name,
                            method.id,
                            lines.len(),
                            sample
                        );
                    }
                    for (start_pc, line) in lines {
                        if line > 0 {
                            line_map
                                .entry(line)
                                .or_default()
                                .push((method.id, start_pc));
                        }
                    }
                }
                Err(error) => {
                    if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
                        eprintln!(
                            "[reclipse-debug] line table failed for {} ({}): {error}",
                            method.name, method.id
                        );
                    }
                }
            }
        }
        let mut resolved_lines = Vec::new();
        for target in &targets {
            let Some(locations) = line_map.get(&(target.line as u32)) else {
                let _ = self.events.send(DebugEvent::BreakpointError {
                    path: target.path.clone(),
                    line: target.line,
                    message: "linha sem código executável (mova o breakpoint)".to_owned(),
                });
                resolved_lines.push(target.line);
                continue;
            };
            let mut seen = HashSet::new();
            for (method_id, index) in locations {
                if !seen.insert((*method_id, *index)) {
                    continue;
                }
                match self.set_location_breakpoint(type_id, type_tag, *method_id, *index, target) {
                    Ok(request_id) => {
                        self.requests
                            .entry((target.path.clone(), target.line))
                            .or_default()
                            .push(request_id);
                        let _ = self.events.send(DebugEvent::BreakpointResolved {
                            path: target.path.clone(),
                            line: target.line,
                        });
                    }
                    Err(error) => {
                        let _ = self.events.send(DebugEvent::BreakpointError {
                            path: target.path.clone(),
                            line: target.line,
                            message: error,
                        });
                    }
                }
            }
            resolved_lines.push(target.line);
        }
        self.pending.retain(|pending| {
            !(pending.class_name == class_name && resolved_lines.contains(&pending.line))
        });
        Ok(())
    }

    fn set_location_breakpoint(
        &mut self,
        type_id: u64,
        type_tag: u8,
        method_id: u64,
        index: u64,
        _target: &BreakpointSpec,
    ) -> Result<u32, String> {
        let mut writer = Writer::default();
        writer.u8(EVENT_BREAKPOINT);
        writer.u8(SUSPEND_ALL);
        writer.u32(1);
        writer.u8(MOD_LOCATION_ONLY);
        writer.u8(type_tag);
        writer.id(type_id, self.client.sizes.reference_type);
        writer.id(method_id, self.client.sizes.method);
        writer.u64(index);
        let data = self.client.request(15, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        reader.u32()
    }

    fn handle_composite(&mut self, data: &[u8]) -> Result<bool, String> {
        let mut reader = Reader::new(data, self.client.sizes);
        let suspend_policy = reader.u8()?;
        let count = reader.u32()? as usize;
        let mut stop_request: Option<StopRequest> = None;
        let mut class_prepared = false;
        for _ in 0..count {
            let kind = reader.u8()?;
            let _request_id = reader.u32()?;
            match kind {
                EVENT_CLASS_PREPARE => {
                    let _thread = reader.object()?;
                    let type_tag = reader.u8()?;
                    let type_id = reader.reference_type()?;
                    let signature = reader.string()?;
                    let _status = reader.u32()?;
                    if let Some(class_name) = class_name_from_signature(&signature) {
                        let _ = self.resolve_breakpoints_for_type(type_id, type_tag, &class_name);
                        if !self
                            .pending
                            .iter()
                            .any(|pending| pending.class_name == class_name)
                        {
                            self.clear_class_prepare(&class_name);
                        }
                    }
                    class_prepared = true;
                }
                EVENT_BREAKPOINT => {
                    let thread_id = reader.object()?;
                    let location = reader.location()?;
                    if stop_request.is_none() {
                        stop_request = Some(StopRequest {
                            reason: StopReason::Breakpoint,
                            thread_id,
                            location,
                            exception: None,
                        });
                    }
                }
                EVENT_SINGLE_STEP => {
                    let thread_id = reader.object()?;
                    let location = reader.location()?;
                    self.clear_step_request()?;
                    if stop_request.is_none() {
                        stop_request = Some(StopRequest {
                            reason: StopReason::Step,
                            thread_id,
                            location,
                            exception: None,
                        });
                    }
                }
                EVENT_EXCEPTION => {
                    let thread_id = reader.object()?;
                    let location = reader.location()?;
                    let exception = reader.tagged_object().ok();
                    let _catch = reader.location().ok();
                    if stop_request.is_none() {
                        stop_request = Some(StopRequest {
                            reason: StopReason::Exception,
                            thread_id,
                            location,
                            exception: exception.map(|value| self.format_value(value)),
                        });
                    }
                }
                99 => {
                    let _ = self.events.send(DebugEvent::Exited {
                        message: "JVM encerrou".to_owned(),
                    });
                    return Ok(true);
                }
                90 => {
                    let _ = reader.object()?;
                }
                _ => {}
            }
        }
        if let Some(request) = stop_request {
            self.gather_stop(request)?;
        } else if class_prepared && suspend_policy == SUSPEND_ALL {
            self.vm_resume()?;
            let _ = self.events.send(DebugEvent::Resumed);
        }
        Ok(false)
    }

    fn gather_stop(&mut self, request: StopRequest) -> Result<(), String> {
        let threads = self.all_threads().unwrap_or_default();
        let thread = threads
            .iter()
            .find(|thread| thread.id == request.thread_id)
            .cloned()
            .unwrap_or(ThreadInfo {
                id: request.thread_id,
                name: format!("thread {}", request.thread_id),
            });
        let mut frames = self.frames(request.thread_id).unwrap_or_default();
        if frames.is_empty() {
            let fallback = self.location_info(request.location);
            frames.push(FrameInfo {
                index: 0,
                thread_id: request.thread_id,
                frame_id: 0,
                location: fallback,
                type_id: request.location.type_id,
                method_id: request.location.method_id,
            });
        }
        let variables = frames
            .first()
            .filter(|frame| frame.frame_id != 0)
            .map(|frame| self.variables(request.thread_id, frame).unwrap_or_default())
            .unwrap_or_default();
        let location = frames.first().map(|frame| frame.location.clone());
        self.stop = Some(StopContext {
            thread_id: request.thread_id,
            frames: frames.clone(),
            variables: variables.clone(),
        });
        let _ = self.events.send(DebugEvent::Stopped(Box::new(StopInfo {
            reason: request.reason,
            thread,
            threads,
            frames,
            variables,
            location,
            exception: request.exception,
        })));
        Ok(())
    }

    fn select_frame(&mut self, thread_id: u64, frame_index: usize) -> Result<(), String> {
        let Some(stop) = &self.stop else {
            return Err("nenhuma parada ativa".to_owned());
        };
        let Some(frame) = stop.frames.get(frame_index).cloned() else {
            return Err("frame inválido".to_owned());
        };
        let variables = self.variables(thread_id, &frame).unwrap_or_default();
        let _ = self.events.send(DebugEvent::Variables {
            frame_index,
            variables,
        });
        Ok(())
    }

    fn frames(&mut self, thread_id: u64) -> Result<Vec<FrameInfo>, String> {
        let mut writer = Writer::default();
        writer.id(thread_id, self.client.sizes.object);
        writer.u32(0);
        writer.u32(u32::MAX);
        let data = self.client.request(11, 6, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()?.min(200) as usize;
        let mut frames = Vec::with_capacity(count);
        for index in 0..count {
            let frame_id = reader.frame()?;
            let location = reader.location()?;
            frames.push(FrameInfo {
                index,
                thread_id,
                frame_id,
                location: self.location_info(location),
                type_id: location.type_id,
                method_id: location.method_id,
            });
        }
        Ok(frames)
    }

    fn location_info(&mut self, location: Location) -> SourceLocation {
        let signature = self
            .type_signature(location.type_id)
            .unwrap_or_else(|_| "L?;".to_owned());
        let method = self
            .method_name(location.type_id, location.method_id)
            .unwrap_or_else(|| "<unknown>".to_owned());
        let line = self
            .line_for(location.type_id, location.method_id, location.index)
            .unwrap_or(0);
        let source_file = self.source_file(location.type_id).ok().flatten();
        SourceLocation {
            signature,
            source_file,
            line,
            method,
            code_index: location.index,
        }
    }

    fn type_signature(&mut self, type_id: u64) -> Result<String, String> {
        if let Some(signature) = self.signature_cache.get(&type_id) {
            return Ok(signature.clone());
        }
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        let data = self.client.request(2, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let signature = reader.string()?;
        self.signature_cache.insert(type_id, signature.clone());
        Ok(signature)
    }

    fn source_file(&mut self, type_id: u64) -> Result<Option<String>, String> {
        if let Some(source) = self.source_cache.get(&type_id) {
            return Ok(source.clone());
        }
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        let data = self.client.request(2, 7, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let source = reader.string().ok();
        self.source_cache.insert(type_id, source.clone());
        Ok(source)
    }

    fn methods(&mut self, type_id: u64) -> Result<Vec<MethodInfo>, String> {
        if let Some(methods) = self.method_cache.get(&type_id) {
            return Ok(methods.clone());
        }
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        let data = self.client.request(2, 5, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()? as usize;
        let mut methods = Vec::with_capacity(count);
        for _ in 0..count {
            methods.push(MethodInfo {
                id: reader.method()?,
                name: reader.string()?,
                signature: reader.string()?,
            });
            let _modifiers = reader.u32()?;
        }
        self.method_cache.insert(type_id, methods.clone());
        Ok(methods)
    }

    fn method_name(&mut self, type_id: u64, method_id: u64) -> Option<String> {
        let methods = self.methods(type_id).ok()?;
        methods
            .iter()
            .find(|method| method.id == method_id)
            .map(|method| {
                if method.name == "<init>" {
                    "this()".to_owned()
                } else {
                    method.name.clone()
                }
            })
    }

    fn line_table(&mut self, type_id: u64, method_id: u64) -> Result<Vec<(u64, u32)>, String> {
        if let Some(lines) = self.line_cache.get(&(type_id, method_id)) {
            return Ok(lines.clone());
        }
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        writer.id(method_id, self.client.sizes.method);
        let data = self.client.request(6, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let _start = reader.u64()?;
        let _end = reader.u64()?;
        let count = reader.u32()? as usize;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            lines.push((reader.u64()?, reader.u32()?));
        }
        self.line_cache.insert((type_id, method_id), lines.clone());
        Ok(lines)
    }

    fn line_for(&mut self, type_id: u64, method_id: u64, index: u64) -> Result<usize, String> {
        let lines = self.line_table(type_id, method_id)?;
        Ok(lines
            .iter()
            .filter(|(start, _)| *start <= index)
            .max_by_key(|(start, _)| *start)
            .map(|(_, line)| *line as usize)
            .unwrap_or(0))
    }

    fn variables(
        &mut self,
        thread_id: u64,
        frame: &FrameInfo,
    ) -> Result<Vec<VariableInfo>, String> {
        let mut variables = self.local_variables(thread_id, frame).unwrap_or_default();
        if let Ok(Value::Object { id, .. }) = self.this_object(thread_id, frame) {
            if id != 0 {
                variables.extend(self.object_fields(id).unwrap_or_default());
            }
        }
        Ok(variables)
    }

    fn local_variables(
        &mut self,
        thread_id: u64,
        frame: &FrameInfo,
    ) -> Result<Vec<VariableInfo>, String> {
        let mut writer = Writer::default();
        writer.id(frame.type_id, self.client.sizes.reference_type);
        writer.id(frame.method_id, self.client.sizes.method);
        let data = match self.client.request(6, 2, &writer.data) {
            Ok(data) => data,
            Err(error) => {
                if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
                    eprintln!(
                        "[reclipse-debug] variable table failed for type {} method {}: {error}",
                        frame.type_id, frame.method_id
                    );
                }
                return Err(error);
            }
        };
        let mut reader = Reader::new(&data, self.client.sizes);
        let _argument_slots = reader.u32()?;
        let slot_count = reader.u32()? as usize;
        if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
            eprintln!(
                "[reclipse-debug] variable table type {} method {}: {slot_count} slot(s), code index {}",
                frame.type_id, frame.method_id, frame.location.code_index
            );
        }
        let mut slots = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..slot_count {
            let start = reader.u64()?;
            let name = reader.string()?;
            let signature = reader.string()?;
            let length = reader.u32()? as u64;
            let slot = reader.u32()?;
            if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
                eprintln!(
                    "[reclipse-debug]   slot {slot} {name} {signature} start={start} length={length}"
                );
            }
            if frame.location.code_index < start {
                continue;
            }
            if length != 0 && frame.location.code_index >= start.saturating_add(length) {
                continue;
            }
            if !seen.insert(slot) {
                continue;
            }
            slots.push((name, signature, slot));
        }
        if slots.is_empty() {
            return Ok(Vec::new());
        }
        let mut values = Writer::default();
        values.id(thread_id, self.client.sizes.object);
        values.id(frame.frame_id, self.client.sizes.frame);
        values.u32(slots.len() as u32);
        for (_, signature, slot) in &slots {
            values.u32(*slot);
            values.u8(signature.as_bytes().first().copied().unwrap_or(b'L'));
        }
        let data = match self.client.request(16, 1, &values.data) {
            Ok(data) => data,
            Err(error) => {
                if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
                    eprintln!("[reclipse-debug] stack frame get values failed: {error}");
                }
                return Err(error);
            }
        };
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()? as usize;
        if std::env::var_os("RECLIPSE_DEBUG_TRACE").is_some() {
            eprintln!("[reclipse-debug]   get values returned {count} value(s)");
        }
        let mut variables = Vec::new();
        for (name, signature, _) in slots.iter().take(count) {
            let value = reader.value()?;
            variables.push(VariableInfo {
                name: name.clone(),
                value: self.format_value(value),
                type_name: type_name_from_signature(signature),
            });
            if variables.len() >= 200 {
                break;
            }
        }
        Ok(variables)
    }

    fn this_object(&mut self, thread_id: u64, frame: &FrameInfo) -> Result<Value, String> {
        let mut writer = Writer::default();
        writer.id(thread_id, self.client.sizes.object);
        writer.id(frame.frame_id, self.client.sizes.frame);
        let data = self.client.request(16, 3, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        reader.value()
    }

    fn fields(&mut self, type_id: u64) -> Result<Vec<FieldInfo>, String> {
        if let Some(fields) = self.field_cache.get(&type_id) {
            return Ok(fields.clone());
        }
        let mut writer = Writer::default();
        writer.id(type_id, self.client.sizes.reference_type);
        let data = self.client.request(2, 4, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let count = reader.u32()? as usize;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let id = reader.field()?;
            let name = reader.string()?;
            let signature = reader.string()?;
            let modifiers = reader.u32()?;
            fields.push(FieldInfo {
                id,
                name,
                signature,
                is_static: modifiers & 0x0008 != 0,
            });
        }
        self.field_cache.insert(type_id, fields.clone());
        Ok(fields)
    }

    fn object_fields(&mut self, object_id: u64) -> Result<Vec<VariableInfo>, String> {
        let mut writer = Writer::default();
        writer.id(object_id, self.client.sizes.object);
        let data = self.client.request(9, 1, &writer.data)?;
        let mut reader = Reader::new(&data, self.client.sizes);
        let _tag = reader.u8()?;
        let mut type_id = reader.reference_type()?;
        let mut groups: Vec<(Vec<u64>, Vec<FieldInfo>)> = Vec::new();
        let mut visited = HashSet::new();
        let mut depth = 0;
        while type_id != 0 && depth < 8 && visited.insert(type_id) {
            let fields = self.fields(type_id).unwrap_or_default();
            let ids: Vec<u64> = fields
                .iter()
                .filter(|field| !field.is_static)
                .map(|field| field.id)
                .collect();
            if !ids.is_empty() {
                groups.push((ids, fields));
            }
            let mut superclass = Writer::default();
            superclass.id(type_id, self.client.sizes.reference_type);
            match self.client.request(3, 1, &superclass.data) {
                Ok(data) => {
                    let mut reader = Reader::new(&data, self.client.sizes);
                    type_id = reader.reference_type().unwrap_or(0);
                }
                Err(_) => break,
            }
            depth += 1;
        }
        let mut variables = Vec::new();
        for (ids, fields) in groups {
            let mut values = Writer::default();
            values.id(object_id, self.client.sizes.object);
            values.u32(ids.len() as u32);
            for id in &ids {
                values.id(*id, self.client.sizes.field);
            }
            let Ok(data) = self.client.request(9, 2, &values.data) else {
                continue;
            };
            let mut reader = Reader::new(&data, self.client.sizes);
            let count = reader.u32().unwrap_or(0) as usize;
            for (field, _) in fields
                .iter()
                .filter(|field| !field.is_static)
                .zip(ids.iter())
                .take(count)
            {
                let Ok(value) = reader.value() else {
                    break;
                };
                variables.push(VariableInfo {
                    name: format!("this.{}", field.name),
                    value: self.format_value(value),
                    type_name: type_name_from_signature(&field.signature),
                });
                if variables.len() >= 80 {
                    return Ok(variables);
                }
            }
        }
        Ok(variables)
    }

    fn format_value(&mut self, value: Value) -> String {
        let text = match value {
            Value::Byte(value) => value.to_string(),
            Value::Boolean(value) => value.to_string(),
            Value::Char(value) => {
                char::from_u32(value as u32).map_or_else(|| value.to_string(), |c| format!("'{c}'"))
            }
            Value::Short(value) => value.to_string(),
            Value::Int(value) => value.to_string(),
            Value::Long(value) => value.to_string(),
            Value::Float(value) => value.to_string(),
            Value::Double(value) => value.to_string(),
            Value::Void => "void".to_owned(),
            Value::Object { tag, id } => self.format_object(tag, id),
        };
        truncate(&text, 240)
    }

    fn format_object(&mut self, tag: u8, id: u64) -> String {
        if id == 0 {
            return "null".to_owned();
        }
        if tag == b's' {
            if let Some(text) = self.string_cache.get(&id) {
                return format!("\"{}\"", truncate(text, 200));
            }
            let mut writer = Writer::default();
            writer.id(id, self.client.sizes.object);
            if let Ok(data) = self.client.request(10, 1, &writer.data) {
                let mut reader = Reader::new(&data, self.client.sizes);
                if let Ok(text) = reader.string() {
                    self.string_cache.insert(id, text.clone());
                    return format!("\"{}\"", truncate(&text, 200));
                }
            }
            return "\"\"".to_owned();
        }
        let signature = self
            .object_signature(id)
            .unwrap_or_else(|| "Ljava/lang/Object;".to_owned());
        if let Some(component) = signature.strip_prefix('[') {
            let mut writer = Writer::default();
            writer.id(id, self.client.sizes.object);
            if let Ok(data) = self.client.request(13, 1, &writer.data) {
                let mut reader = Reader::new(&data, self.client.sizes);
                if let Ok(length) = reader.u32() {
                    return format!("{}[{length}]", type_name_from_signature(component));
                }
            }
            return format!("{}@{id}", type_name_from_signature(&signature));
        }
        format!("{}@{id}", type_name_from_signature(&signature))
    }

    fn object_signature(&mut self, object_id: u64) -> Option<String> {
        let type_id = if let Some(type_id) = self.object_type_cache.get(&object_id) {
            *type_id
        } else {
            let mut writer = Writer::default();
            writer.id(object_id, self.client.sizes.object);
            let data = self.client.request(9, 1, &writer.data).ok()?;
            let mut reader = Reader::new(&data, self.client.sizes);
            let _tag = reader.u8().ok()?;
            let type_id = reader.reference_type().ok()?;
            self.object_type_cache.insert(object_id, type_id);
            type_id
        };
        self.type_signature(type_id).ok()
    }

    fn evaluate(&mut self, expression: &str) {
        let expression = expression.trim();
        let mut error = None;
        let mut value = None;
        if expression.is_empty() {
            error = Some("expressão vazia".to_owned());
        } else if let Some(literal) = parse_literal(expression) {
            value = Some(literal);
        } else if let Some(stop) = &self.stop {
            if let Some(variable) = stop
                .variables
                .iter()
                .find(|variable| variable.name == expression)
            {
                value = Some(variable.value.clone());
            } else {
                error = Some(format!(
                    "variável `{expression}` não encontrada no frame selecionado"
                ));
            }
        } else {
            error = Some("a JVM não está pausada".to_owned());
        }
        let _ = self.events.send(DebugEvent::EvaluateResult {
            expression: expression.to_owned(),
            value,
            error,
        });
    }

    fn finish(&mut self) {
        if self.own_process {
            let _ = self.vm_exit();
        } else {
            let _ = self.vm_dispose();
        }
        self.cleanup();
        let _ = self.events.send(DebugEvent::Exited {
            message: "sessão de debug encerrada".to_owned(),
        });
    }

    fn cleanup(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn signature_for_class(class_name: &str) -> String {
    format!("L{};", class_name.replace('.', "/"))
}

fn class_name_from_signature(signature: &str) -> Option<String> {
    let signature = signature.strip_prefix('L')?.strip_suffix(';')?;
    Some(signature.replace('/', "."))
}

fn type_name_from_signature(signature: &str) -> String {
    let signature = signature.split('<').next().unwrap_or(signature);
    match signature {
        "B" => "byte".to_owned(),
        "Z" => "boolean".to_owned(),
        "C" => "char".to_owned(),
        "S" => "short".to_owned(),
        "I" => "int".to_owned(),
        "J" => "long".to_owned(),
        "F" => "float".to_owned(),
        "D" => "double".to_owned(),
        "V" => "void".to_owned(),
        value if value.starts_with('[') => {
            format!("{}[]", type_name_from_signature(&value[1..]))
        }
        value if value.starts_with('L') && value.ends_with(';') => {
            value[1..value.len() - 1].replace(['/', '$'], ".")
        }
        value => value.replace('/', "."),
    }
}

fn parse_literal(expression: &str) -> Option<String> {
    let trimmed = expression.trim();
    if trimmed == "null" {
        return Some("null".to_owned());
    }
    if trimmed == "true" || trimmed == "false" {
        return Some(trimmed.to_owned());
    }
    if let Some(inner) = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return Some(format!("\"{inner}\""));
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return Some(format!("'{inner}'"));
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return Some(value.to_string());
    }
    if let Ok(value) = trimmed.parse::<f64>() {
        return Some(value.to_string());
    }
    None
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut output: String = text.chars().take(max.saturating_sub(1)).collect();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_class_names_and_signatures() {
        assert_eq!(signature_for_class("com.acme.App"), "Lcom/acme/App;");
        assert_eq!(
            signature_for_class("com.acme.App$Inner"),
            "Lcom/acme/App$Inner;"
        );
        assert_eq!(
            class_name_from_signature("Lcom/acme/App$Inner;").as_deref(),
            Some("com.acme.App$Inner")
        );
        assert_eq!(
            class_name_from_signature("Lcom/acme/App;").as_deref(),
            Some("com.acme.App")
        );
    }

    #[test]
    fn renders_type_names() {
        assert_eq!(type_name_from_signature("I"), "int");
        assert_eq!(type_name_from_signature("[[I"), "int[][]");
        assert_eq!(
            type_name_from_signature("Ljava/lang/String;"),
            "java.lang.String"
        );
        assert_eq!(
            type_name_from_signature("Lcom/acme/Outer$Inner;"),
            "com.acme.Outer.Inner"
        );
    }

    #[test]
    fn parses_literals() {
        assert_eq!(parse_literal("42").as_deref(), Some("42"));
        assert_eq!(parse_literal("true").as_deref(), Some("true"));
        assert_eq!(parse_literal("\"hi\"").as_deref(), Some("\"hi\""));
        assert!(parse_literal("user.name").is_none());
    }

    #[test]
    fn normalizes_attach_addresses() {
        assert_eq!(normalize_address("5005"), "127.0.0.1:5005");
        assert_eq!(normalize_address(":5005"), "127.0.0.1:5005");
        assert_eq!(normalize_address("localhost:5005"), "localhost:5005");
        assert_eq!(normalize_address("host"), "host:5005");
    }
}
