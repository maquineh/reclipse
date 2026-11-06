//! M3 build/run/test layer: javac fallback, launch configs, cancellable
//! processes, resources and Maven/Gradle test parsing.
//!
//! This is intentionally a *fallback*: it uses the resolved classpath and the
//! project source files, does not interpret build lifecycles (Maven/Gradle
//! plugins, resources, tests) and expects a JDK with `javac`/`java`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reclipse_classpath::ClasspathIndex;
use reclipse_core::Project;
use reclipse_java::{ProjectIndex, SymbolKind};

#[derive(Debug, Clone, Default)]
pub struct BuildEnv {
    pub jars: Vec<PathBuf>,
    pub jdk_home: Option<PathBuf>,
    pub jdk_target: Option<u32>,
}

impl BuildEnv {
    pub fn from_classpath(index: &ClasspathIndex) -> Self {
        Self {
            jars: index.jars.clone(),
            jdk_home: index.jdk_home.clone(),
            jdk_target: index.stats.jdk_target,
        }
    }
}

/// Launch configuration: which main to run and with what args/env.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    pub main_class: String,
    pub program_args: String,
    pub jvm_args: String,
    pub env_vars: String,
    /// `host:port` for attaching instead of launching a JVM. Empty launches.
    pub debug_attach: String,
}

impl LaunchConfig {
    pub fn program_args_list(&self) -> Vec<String> {
        split_args(&self.program_args)
    }

    pub fn jvm_args_list(&self) -> Vec<String> {
        split_args(&self.jvm_args)
    }

    pub fn env_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for line in self.env_vars.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                if !key.is_empty() {
                    map.insert(key.to_owned(), value.trim().to_owned());
                }
            }
        }
        map
    }
}

pub fn split_args(raw: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(character) = chars.next() {
        match character {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if chars.peek().is_some() => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            character if character.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavacDiagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
    pub is_error: bool,
}

#[derive(Debug)]
pub struct CompileResult {
    pub success: bool,
    pub diagnostics: Vec<JavacDiagnostic>,
    pub output: String,
    pub elapsed_ms: u128,
    /// Kept alive so `run_main` can use the compiled classes.
    pub classes_dir: tempfile::TempDir,
}

#[derive(Debug)]
pub struct RunResult {
    pub success: bool,
    pub output: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Default)]
pub struct TestFailure {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
    pub class_name: Option<String>,
    pub method: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TestSummary {
    pub total: usize,
    pub failures: usize,
    pub errors: usize,
    pub skipped: usize,
    pub failures_detail: Vec<TestFailure>,
}

#[derive(Debug)]
pub struct TestResult {
    pub success: bool,
    pub tool: String,
    pub summary: TestSummary,
    pub output: String,
    pub elapsed_ms: u128,
}

pub fn compile(
    project: &Project,
    env: &BuildEnv,
    cancel: &AtomicBool,
) -> Result<CompileResult, String> {
    compile_inner(project, env, cancel)
}

fn compile_inner(
    project: &Project,
    env: &BuildEnv,
    cancel: &AtomicBool,
) -> Result<CompileResult, String> {
    let started = Instant::now();
    let javac = jdk_binary(env.jdk_home.as_deref(), "javac")?;
    let classes_dir = tempfile::Builder::new()
        .prefix("reclipse-build-")
        .tempdir()
        .map_err(|error| format!("não foi possível criar diretório de build: {error}"))?;
    let argfile = classes_dir.path().join("javac.args");
    let mut args = vec![
        "-encoding".to_owned(),
        "UTF-8".to_owned(),
        "-proc:none".to_owned(),
        "-g".to_owned(),
        "-d".to_owned(),
        quote_path(classes_dir.path()),
    ];
    if let Some(target) = env.jdk_target {
        args.push("--release".to_owned());
        args.push(target.to_string());
    }
    if !env.jars.is_empty() {
        args.push("-classpath".to_owned());
        args.push(quote_path(&join_classpath(&env.jars)));
    }
    let mut source_count = 0;
    for source in project
        .java_files
        .iter()
        .filter(|source| !is_test_source(source))
    {
        if cancel.load(Ordering::SeqCst) {
            return Err("compilação cancelada".to_owned());
        }
        args.push(quote_path(source));
        source_count += 1;
    }
    if source_count == 0 {
        for source in &project.java_files {
            args.push(quote_path(source));
        }
    }
    fs::write(&argfile, args.join("\n"))
        .map_err(|error| format!("não foi possível escrever argumentos do javac: {error}"))?;
    let output = run_piped(
        &javac,
        &[format!("@{}", argfile.display())],
        &HashMap::new(),
        &project.root,
        cancel,
    )
    .map_err(|error| format!("falha ao executar {}: {error}", javac.display()))?;
    if cancel.load(Ordering::SeqCst) {
        return Err("compilação cancelada".to_owned());
    }
    let copied = copy_resources(project, classes_dir.path()).unwrap_or(0);
    let combined = output.combined;
    let mut result = CompileResult {
        success: output.status_success,
        diagnostics: parse_javac_output(&combined),
        output: if copied > 0 {
            format!("{combined}\nresources: {copied} arquivo(s) copiado(s)")
        } else {
            combined
        },
        elapsed_ms: started.elapsed().as_millis(),
        classes_dir,
    };
    if cancel.load(Ordering::SeqCst) {
        result.success = false;
        result.output.push_str("\ncompilação cancelada");
        return Err("compilação cancelada".to_owned());
    }
    Ok(result)
}

/// Resource roots copied next to compiled classes (`src/main/resources`).
pub fn resource_roots(project: &Project) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for candidate in ["src/main/resources", "src/test/resources", "resources"] {
        let path = project.root.join(candidate);
        if path.is_dir() {
            roots.push(path);
        }
    }
    roots
}

/// Copy `src/main/resources` into `classes_dir`, preserving relative paths.
pub fn copy_resources(project: &Project, classes_dir: &Path) -> Result<usize, String> {
    let mut copied = 0;
    for root in resource_roots(project) {
        if !root.ends_with("src/test/resources") && !root.ends_with("resources") {
            // Copy main resources; test resources only matter for tests.
        }
        if root.ends_with("src/test/resources") {
            continue;
        }
        let entries = walkdir_files(&root);
        for file in entries {
            let Ok(relative) = file.strip_prefix(&root) else {
                continue;
            };
            let target = classes_dir.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("falha ao criar {}: {error}", parent.display()))?;
            }
            fs::copy(&file, &target)
                .map_err(|error| format!("falha ao copiar {}: {error}", file.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn walkdir_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

pub fn run_main(
    main_class: &str,
    launch: &LaunchConfig,
    env: &BuildEnv,
    compiled: &CompileResult,
    project_root: &Path,
    cancel: &AtomicBool,
) -> Result<RunResult, String> {
    let started = Instant::now();
    let java = jdk_binary(env.jdk_home.as_deref(), "java")?;
    let classpath = runtime_classpath(compiled, env, project_root);
    let mut args = vec!["-cp".to_owned(), classpath.display().to_string()];
    args.extend(launch.jvm_args_list());
    args.push(main_class.to_owned());
    args.extend(launch.program_args_list());
    let output = run_piped(&java, &args, &launch.env_map(), project_root, cancel)
        .map_err(|error| format!("falha ao executar {}: {error}", java.display()))?;
    Ok(RunResult {
        success: output.status_success && !cancel.load(Ordering::SeqCst),
        output: output.combined,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Runtime classpath used by `java` (compiled classes, project classes and jars).
pub(crate) fn runtime_classpath(
    compiled: &CompileResult,
    env: &BuildEnv,
    project_root: &Path,
) -> std::path::PathBuf {
    let mut entries = Vec::with_capacity(env.jars.len() + 2);
    entries.push(compiled.classes_dir.path().to_path_buf());
    let target_classes = project_root.join("target/classes");
    if target_classes.is_dir() {
        entries.push(target_classes);
    }
    entries.extend(env.jars.iter().cloned());
    join_classpath(&entries)
}

/// `java` binary for the configured JDK.
pub(crate) fn java_binary(env: &BuildEnv) -> Result<PathBuf, String> {
    jdk_binary(env.jdk_home.as_deref(), "java")
}

/// Classpath string for launching a JVM.
pub(crate) fn runtime_classpath_string(
    compiled: &CompileResult,
    env: &BuildEnv,
    project_root: &Path,
) -> String {
    runtime_classpath(compiled, env, project_root)
        .display()
        .to_string()
}

pub(crate) struct PipedOutput {
    pub(crate) status_success: bool,
    pub(crate) combined: String,
}

fn run_piped(
    program: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    workdir: &Path,
    cancel: &AtomicBool,
) -> Result<PipedOutput, String> {
    run_piped_stream(program, args, env, workdir, cancel, &mut |_| {})
}

pub(crate) fn run_piped_stream(
    program: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    workdir: &Path,
    cancel: &AtomicBool,
    on_output: &mut dyn FnMut(&str),
) -> Result<PipedOutput, String> {
    let out_file = tempfile::Builder::new()
        .prefix("reclipse-out-")
        .tempfile()
        .map_err(|error| format!("falha ao criar arquivo de saída: {error}"))?;
    let out_path = out_file.path().to_path_buf();
    // Keep the handle alive until the child exits.
    let _keep = out_file;
    let stdout = std::fs::File::create(&out_path)
        .map_err(|error| format!("falha ao criar saída: {error}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("falha ao clonar saída: {error}"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(workdir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("falha ao iniciar {}: {error}", program.display()))?;
    let mut position = 0usize;
    loop {
        if let Ok(bytes) = fs::read(&out_path) {
            if bytes.len() > position {
                let chunk = String::from_utf8_lossy(&bytes[position..]);
                on_output(&chunk);
                position = bytes.len();
            }
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let combined = fs::read_to_string(&out_path).unwrap_or_default();
            return Ok(PipedOutput {
                status_success: false,
                combined: format!("{combined}\nexecução cancelada"),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Ok(bytes) = fs::read(&out_path) {
                    if bytes.len() > position {
                        let chunk = String::from_utf8_lossy(&bytes[position..]);
                        on_output(&chunk);
                    }
                }
                let combined = fs::read_to_string(&out_path).unwrap_or_default();
                return Ok(PipedOutput {
                    status_success: status.success(),
                    combined,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("falha ao aguardar processo: {error}")),
        }
    }
}

/// Read compiled `.class` files for basic hot swap.
pub(crate) fn compiled_classes(compiled: &CompileResult) -> Result<Vec<(String, Vec<u8>)>, String> {
    let root = compiled.classes_dir.path();
    let mut classes = Vec::new();
    for file in walkdir_files(root) {
        if file.extension().and_then(|extension| extension.to_str()) != Some("class") {
            continue;
        }
        let relative = match file.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let mut name = relative.to_string_lossy().replace(['\\', '/'], ".");
        if let Some(stripped) = name.strip_suffix(".class") {
            name = stripped.to_owned();
        }
        if name.is_empty() || name == "module-info" || name == "package-info" {
            continue;
        }
        let bytes =
            fs::read(&file).map_err(|error| format!("falha ao ler {}: {error}", file.display()))?;
        classes.push((name, bytes));
    }
    Ok(classes)
}

/// Discover fully-qualified `main` classes from the syntax index.
pub fn main_classes(index: &ProjectIndex) -> Vec<String> {
    let mut classes = Vec::new();
    for (path, file) in &index.files {
        if is_test_source(path) {
            continue;
        }
        let package = file.package.as_deref().unwrap_or("");
        for symbol in &file.symbols {
            if symbol.kind != SymbolKind::Method || symbol.name != "main" {
                continue;
            }
            let Some(detail) = &symbol.detail else {
                continue;
            };
            if !detail.contains("static") || !detail.contains("void main") {
                continue;
            }
            let Some(container) = &symbol.container else {
                continue;
            };
            let fqn = if package.is_empty() {
                container.clone()
            } else {
                format!("{package}.{container}")
            };
            classes.push(fqn);
        }
    }
    classes.sort();
    classes.dedup();
    classes
}

pub fn parse_javac_output(output: &str) -> Vec<JavacDiagnostic> {
    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let (marker, is_error) = if let Some(position) = line.find(": error:") {
            (position, true)
        } else if let Some(position) = line.find(": warning:") {
            (position, false)
        } else {
            continue;
        };
        let head = &line[..marker];
        let message = line[marker + if is_error { 8 } else { 10 }..].trim();
        let Some((file, line_number)) = head.rsplit_once(':') else {
            continue;
        };
        let Ok(line_number) = line_number.trim().parse::<usize>() else {
            continue;
        };
        let file = PathBuf::from(file.trim());
        if file.as_os_str().is_empty() || message.is_empty() {
            continue;
        }
        diagnostics.push(JavacDiagnostic {
            file,
            line: line_number,
            message: message.to_owned(),
            is_error,
        });
    }
    diagnostics
}

/// Try to find `File.java:123` inside a log line for clickable hyperlinks.
pub fn parse_file_link(line: &str, project_root: Option<&Path>) -> Option<(PathBuf, usize)> {
    let mut best: Option<(usize, PathBuf, usize)> = None;
    let bytes = line.as_bytes();
    let mut index = 0;
    while let Some(found) = line[index..].find(".java") {
        let java_end = index + found + ".java".len();
        let mut cursor = java_end;
        let mut line_number: Option<usize> = None;
        if bytes.get(cursor) == Some(&b':') {
            cursor += 1;
            let start = cursor;
            while cursor < line.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if start != cursor {
                line_number = line[start..cursor].parse().ok();
            }
        }
        let Some(line_number) = line_number else {
            index = java_end;
            continue;
        };
        let mut start = java_end.saturating_sub(".java".len());
        while start > 0 {
            let character = line.as_bytes()[start - 1] as char;
            if character.is_whitespace() || matches!(character, '"' | '\'' | '(' | '[' | '<' | '|')
            {
                break;
            }
            start -= 1;
        }
        let mut file = line[start..java_end].trim().to_owned();
        file = file
            .trim_start_matches('[')
            .trim_start_matches('(')
            .to_owned();
        if file.is_empty() {
            index = cursor;
            continue;
        }
        let path = PathBuf::from(&file);
        let resolved = if path.is_absolute() {
            path
        } else if let Some(root) = project_root {
            root.join(&path)
        } else {
            path
        };
        // Prefer the rightmost (most specific) match, like stack traces.
        if best
            .as_ref()
            .is_none_or(|(position, _, _)| java_end >= *position)
        {
            best = Some((java_end, resolved, line_number));
        }
        index = cursor.max(java_end);
        if index >= line.len() {
            break;
        }
    }
    best.map(|(_, path, line)| (path, line))
}

pub fn join_classpath(entries: &[PathBuf]) -> PathBuf {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut joined = OsString::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            joined.push(separator);
        }
        joined.push(entry.as_os_str());
    }
    PathBuf::from(joined)
}

fn jdk_binary(home: Option<&Path>, name: &str) -> Result<PathBuf, String> {
    if let Some(home) = home {
        let candidate = home
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from(name))
}

pub fn is_test_source(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    normalized.contains("/src/test/") || normalized.contains("/src/test/java/")
}

fn quote_path(path: &Path) -> String {
    // javac argfiles treat backslash as an escape, so use forward slashes
    // (accepted on Windows) and quote paths with spaces.
    let display = path.display().to_string().replace('\\', "/");
    format!("\"{display}\"")
}

// ---------------------------------------------------------------------------
// Maven / Gradle tests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestTool {
    Maven,
    Gradle,
}

pub fn detect_test_tool(project: &Project) -> Option<(TestTool, PathBuf)> {
    if project.root.join("pom.xml").is_file() {
        if let Some(binary) =
            find_wrapper(&project.root, &["mvnw.cmd", "mvnw"], &["mvn.cmd", "mvn"])
        {
            return Some((TestTool::Maven, binary));
        }
        return Some((TestTool::Maven, PathBuf::from("mvn")));
    }
    if project.root.join("build.gradle").is_file()
        || project.root.join("build.gradle.kts").is_file()
    {
        if let Some(binary) = find_wrapper(
            &project.root,
            &["gradlew.bat", "gradlew"],
            &["gradle.bat", "gradle"],
        ) {
            return Some((TestTool::Gradle, binary));
        }
        return Some((TestTool::Gradle, PathBuf::from("gradle")));
    }
    None
}

fn find_wrapper(root: &Path, wrappers: &[&str], fallbacks: &[&str]) -> Option<PathBuf> {
    for name in wrappers {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for name in fallbacks {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Format Java source through an external formatter.
///
/// Opt-in via `RECLIPSE_FORMATTER` (executable that reads stdin, e.g. a
/// `google-java-format` wrapper) or `RECLIPSE_FORMATTER_JAR` (invoked with
/// `java -jar <jar> -`). The native engine does not reimplement formatting.
pub fn format_source(source: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;

    let (program, args) = if let Ok(program) = std::env::var("RECLIPSE_FORMATTER") {
        (PathBuf::from(program), vec!["-".to_owned()])
    } else if let Ok(jar) = std::env::var("RECLIPSE_FORMATTER_JAR") {
        (
            jdk_binary(None, "java")?,
            vec!["-jar".to_owned(), jar, "-".to_owned()],
        )
    } else {
        return Err(
            "formatter não configurado: defina RECLIPSE_FORMATTER ou RECLIPSE_FORMATTER_JAR"
                .to_owned(),
        );
    };
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("falha ao iniciar formatter {}: {error}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .map_err(|error| format!("falha ao enviar código ao formatter: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("formatter falhou: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "formatter retornou {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("formatter retornou saída inválida (UTF-8): {error}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFilter {
    pub class_name: String,
    pub method: Option<String>,
}

pub fn run_tests_filtered(
    project: &Project,
    env: &HashMap<String, String>,
    cancel: &AtomicBool,
    filter: Option<&TestFilter>,
    on_output: &mut dyn FnMut(&str),
) -> Result<TestResult, String> {
    let started = Instant::now();
    let Some((tool, binary)) = detect_test_tool(project) else {
        return Err("nenhum pom.xml ou build.gradle encontrado para testes".to_owned());
    };
    let (args, label) = test_command(tool, filter);
    let output = run_piped_stream(&binary, &args, env, &project.root, cancel, on_output)
        .map_err(|error| format!("falha ao executar {label}: {error}"))?;
    let summary = parse_test_output(&output.combined);
    Ok(TestResult {
        success: output.status_success
            && summary.errors == 0
            && summary.failures == 0
            && !cancel.load(Ordering::SeqCst),
        tool: label,
        summary,
        output: output.combined,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn test_command(tool: TestTool, filter: Option<&TestFilter>) -> (Vec<String>, String) {
    match tool {
        TestTool::Maven => {
            let mut args = vec!["-B".to_owned(), "test".to_owned()];
            let mut label = "mvn test".to_owned();
            if let Some(filter) = filter {
                let class_name = filter
                    .class_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(&filter.class_name);
                let target = match &filter.method {
                    Some(method) => format!("{class_name}#{method}"),
                    None => class_name.to_owned(),
                };
                args.push(format!("-Dtest={target}"));
                args.push("-DfailIfNoTests=false".to_owned());
                label = format!("mvn test ({target})");
            }
            (args, label)
        }
        TestTool::Gradle => {
            let mut args = vec![
                "test".to_owned(),
                "--console=plain".to_owned(),
                "-q".to_owned(),
            ];
            let mut label = "gradle test".to_owned();
            if let Some(filter) = filter {
                let target = match &filter.method {
                    Some(method) => format!("{}.{}", filter.class_name, method),
                    None => filter.class_name.clone(),
                };
                args.push("--tests".to_owned());
                args.push(target.clone());
                label = format!("gradle test ({target})");
            }
            (args, label)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    Compile,
    Package,
    Clean,
}

impl LifecyclePhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Package => "package",
            Self::Clean => "clean",
        }
    }
}

pub fn run_lifecycle(
    project: &Project,
    env: &HashMap<String, String>,
    cancel: &AtomicBool,
    phase: LifecyclePhase,
    on_output: &mut dyn FnMut(&str),
) -> Result<ResolveResult, String> {
    let started = Instant::now();
    let Some((tool, binary)) = detect_test_tool(project) else {
        return Err("lifecycle requer Maven (pom.xml) ou Gradle (build.gradle)".to_owned());
    };
    let (args, label) = match (tool, phase) {
        (TestTool::Maven, LifecyclePhase::Compile) => (
            vec!["-B".to_owned(), "compile".to_owned()],
            "mvn compile".to_owned(),
        ),
        (TestTool::Maven, LifecyclePhase::Package) => (
            vec![
                "-B".to_owned(),
                "package".to_owned(),
                "-DskipTests".to_owned(),
            ],
            "mvn package".to_owned(),
        ),
        (TestTool::Maven, LifecyclePhase::Clean) => (
            vec!["-B".to_owned(), "clean".to_owned()],
            "mvn clean".to_owned(),
        ),
        (TestTool::Gradle, LifecyclePhase::Compile) => (
            vec![
                "compileJava".to_owned(),
                "--console=plain".to_owned(),
                "-q".to_owned(),
            ],
            "gradle compileJava".to_owned(),
        ),
        (TestTool::Gradle, LifecyclePhase::Package) => (
            vec![
                "jar".to_owned(),
                "-x".to_owned(),
                "test".to_owned(),
                "--console=plain".to_owned(),
                "-q".to_owned(),
            ],
            "gradle jar".to_owned(),
        ),
        (TestTool::Gradle, LifecyclePhase::Clean) => (
            vec![
                "clean".to_owned(),
                "--console=plain".to_owned(),
                "-q".to_owned(),
            ],
            "gradle clean".to_owned(),
        ),
    };
    let output = run_piped_stream(&binary, &args, env, &project.root, cancel, on_output)
        .map_err(|error| format!("falha ao executar {label}: {error}"))?;
    Ok(ResolveResult {
        success: output.status_success && !cancel.load(Ordering::SeqCst),
        tool: label,
        output: output.combined,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

#[derive(Debug)]
pub struct ResolveResult {
    pub success: bool,
    pub tool: String,
    pub output: String,
    pub elapsed_ms: u128,
}

/// Download dependencies into the local repository using the project's own
/// build tool (`mvn dependency:resolve` / `gradle dependencies`).
/// Offline-first: this is the only operation that touches the network, and it
/// only runs on explicit user request.
pub fn resolve_dependencies(
    project: &Project,
    env: &HashMap<String, String>,
    cancel: &AtomicBool,
    on_output: &mut dyn FnMut(&str),
) -> Result<ResolveResult, String> {
    let started = Instant::now();
    let Some((tool, binary)) = detect_test_tool(project) else {
        return Err("nenhum pom.xml ou build.gradle encontrado".to_owned());
    };
    let (args, label) = match tool {
        TestTool::Maven => (
            vec![
                "-B".to_owned(),
                "dependency:resolve".to_owned(),
                "-q".to_owned(),
            ],
            "mvn dependency:resolve".to_owned(),
        ),
        TestTool::Gradle => (
            vec![
                "dependencies".to_owned(),
                "--configuration".to_owned(),
                "runtimeClasspath".to_owned(),
                "--console=plain".to_owned(),
                "-q".to_owned(),
            ],
            "gradle dependencies".to_owned(),
        ),
    };
    let output = run_piped_stream(&binary, &args, env, &project.root, cancel, on_output)
        .map_err(|error| format!("falha ao executar {label}: {error}"))?;
    Ok(ResolveResult {
        success: output.status_success && !cancel.load(Ordering::SeqCst),
        tool: label,
        output: output.combined,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

pub fn parse_test_output(output: &str) -> TestSummary {
    let mut summary = TestSummary::default();
    for line in output.lines() {
        if let Some(parsed) = parse_surefire_summary(line) {
            summary.total += parsed.0;
            summary.failures += parsed.1;
            summary.errors += parsed.2;
            summary.skipped += parsed.3;
        }
        if let Some(failure) = parse_test_failure(line) {
            summary.failures_detail.push(failure);
        }
        if summary.failures_detail.len() >= 200 {
            break;
        }
    }
    summary
}

fn parse_surefire_summary(line: &str) -> Option<(usize, usize, usize, usize)> {
    let marker = line.find("Tests run:")?;
    let rest = &line[marker + "Tests run:".len()..];
    let mut numbers = Vec::new();
    let mut current = String::new();
    for character in rest.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<usize>() {
                numbers.push(value);
            }
            current.clear();
            if numbers.len() >= 4 {
                break;
            }
        }
    }
    if !current.is_empty() {
        if let Ok(value) = current.parse::<usize>() {
            numbers.push(value);
        }
    }
    if numbers.len() < 4 {
        return None;
    }
    Some((numbers[0], numbers[1], numbers[2], numbers[3]))
}

fn parse_test_failure(line: &str) -> Option<TestFailure> {
    let trimmed = line.trim();
    let stripped = trimmed.strip_prefix("[ERROR]").unwrap_or(trimmed).trim();
    // Maven surefire: `com.FooTest.test -- Time elapsed ... <<< FAILURE!`
    // Gradle: `com.FooTest > test FAILED`
    // Stack: `at com.FooTest.test(FooTest.java:42)`
    let candidate = if stripped.contains("FAILURE")
        || stripped.contains("FAILED")
        || stripped.starts_with("at ")
    {
        stripped.to_owned()
    } else {
        return None;
    };
    let (class_name, method) = parse_test_identity(&candidate);
    let (file, line_number) = parse_file_link(&candidate, None).unwrap_or_default();
    if file.as_os_str().is_empty() && class_name.is_none() {
        return None;
    }
    Some(TestFailure {
        file,
        line: line_number,
        message: candidate.chars().take(240).collect(),
        class_name,
        method,
    })
}

fn parse_test_identity(candidate: &str) -> (Option<String>, Option<String>) {
    if let Some((left, right)) = candidate.split_once(" > ") {
        let class_name = left.trim().to_owned();
        let method = right
            .trim()
            .trim_end_matches(" FAILED")
            .split_whitespace()
            .next()
            .map(str::to_owned);
        return ((!class_name.is_empty()).then_some(class_name), method);
    }
    if let Some((left, _)) = candidate.split_once(" -- ") {
        let left = left.trim();
        if let Some((class_name, method)) = left.rsplit_once('.') {
            return (Some(class_name.to_owned()), Some(method.to_owned()));
        }
    }
    if let Some(rest) = candidate.strip_prefix("at ") {
        let rest = rest.split('(').next().unwrap_or(rest).trim();
        if let Some((class_name, method)) = rest.rsplit_once('.') {
            return (Some(class_name.to_owned()), Some(method.to_owned()));
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_test_failure_identity() {
        let gradle = parse_test_failure("com.acme.AppTest > works FAILED").unwrap();
        assert_eq!(gradle.class_name.as_deref(), Some("com.acme.AppTest"));
        assert_eq!(gradle.method.as_deref(), Some("works"));
        let maven =
            parse_test_failure("[ERROR] com.acme.AppTest.works -- Time elapsed: 1 s <<< FAILURE!")
                .unwrap();
        assert_eq!(maven.class_name.as_deref(), Some("com.acme.AppTest"));
        assert_eq!(maven.method.as_deref(), Some("works"));
        let stack = parse_test_failure("at com.acme.AppTest.works(AppTest.java:42)").unwrap();
        assert_eq!(stack.file, PathBuf::from("AppTest.java"));
        assert_eq!(stack.line, 42);
        assert_eq!(stack.method.as_deref(), Some("works"));
    }

    #[test]
    fn builds_filtered_test_commands() {
        let filter = TestFilter {
            class_name: "com.acme.AppTest".to_owned(),
            method: Some("works".to_owned()),
        };
        let (maven, label) = test_command(TestTool::Maven, Some(&filter));
        assert!(maven.contains(&"test".to_owned()));
        assert!(maven.contains(&"-Dtest=AppTest#works".to_owned()));
        assert!(label.contains("AppTest#works"));
        let (gradle, _) = test_command(TestTool::Gradle, Some(&filter));
        assert!(gradle.contains(&"--tests".to_owned()));
        assert!(gradle.contains(&"com.acme.AppTest.works".to_owned()));
    }

    #[test]
    fn parses_javac_diagnostics_with_windows_paths() {
        let output = r#"
C:\work\demo\src\App.java:12: error: cannot find symbol
    missing();
    ^
C:\work\demo\src\App.java:20: warning: [deprecation] old() in Legacy has been deprecated
"#;
        let diagnostics = parse_javac_output(output);
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics[0].is_error);
        assert_eq!(diagnostics[0].line, 12);
        assert_eq!(
            diagnostics[0].file,
            PathBuf::from(r"C:\work\demo\src\App.java")
        );
        assert!(diagnostics[0].message.contains("cannot find symbol"));
        assert!(!diagnostics[1].is_error);
        assert!(diagnostics[1].message.contains("deprecation"));
    }

    #[test]
    fn compiles_and_runs_tiny_project_when_jdk_available() {
        let Some(jdk) = reclipse_classpath::find_jdk() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("Hello.java"),
            r#"
public class Hello {
    public static void main(String[] args) {
        System.out.println("reclipse-ok");
    }
}
"#,
        )
        .unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let env = BuildEnv {
            jars: Vec::new(),
            jdk_home: Some(jdk.home),
            jdk_target: None,
        };
        let cancel = AtomicBool::new(false);
        let compiled = compile(&project, &env, &cancel).unwrap();
        assert!(compiled.success, "javac output: {}", compiled.output);
        let launch = LaunchConfig::default();
        let run = run_main("Hello", &launch, &env, &compiled, dir.path(), &cancel).unwrap();
        assert!(run.success, "java output: {}", run.output);
        assert!(run.output.contains("reclipse-ok"));
    }

    #[test]
    fn finds_static_main_classes() {
        let app = reclipse_java::parse_source(
            Path::new("com/acme/App.java"),
            "package com.acme; public class App { public static void main(String[] args) {} }",
        );
        let other = reclipse_java::parse_source(
            Path::new("com/acme/Other.java"),
            "package com.acme; public class Other { void main() {} }",
        );
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![app.path.clone(), other.path.clone()],
            all_files: vec![app.path.clone(), other.path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![app, other], 0);
        assert_eq!(main_classes(&index), vec!["com.acme.App".to_owned()]);
    }

    #[test]
    fn splits_launch_args_respecting_quotes() {
        assert_eq!(
            split_args(r#"--name "Ada Lovelace" -Dkey='a b'"#),
            vec!["--name", "Ada Lovelace", "-Dkey=a b"]
        );
        assert!(split_args("").is_empty());
    }

    #[test]
    fn parses_launch_env_map() {
        let launch = LaunchConfig {
            env_vars: "JAVA_OPTS=-Xmx1g\n# comment\nEMPTY=\nINVALID".to_owned(),
            ..LaunchConfig::default()
        };
        let map = launch.env_map();
        assert_eq!(map.get("JAVA_OPTS").map(String::as_str), Some("-Xmx1g"));
        assert_eq!(map.get("EMPTY").map(String::as_str), Some(""));
        assert!(!map.contains_key("INVALID"));
    }

    #[test]
    fn finds_file_hyperlinks_in_logs() {
        let (file, line) = parse_file_link("at com.acme.App.main(App.java:42)", None).unwrap();
        assert_eq!(file, PathBuf::from("App.java"));
        assert_eq!(line, 42);
        let (file, line) = parse_file_link(
            r"C:\work\demo\src\App.java:12: error: cannot find symbol",
            None,
        )
        .unwrap();
        assert_eq!(line, 12);
        assert!(file.ends_with("App.java"));
    }

    #[test]
    fn parses_surefire_summary_and_failures() {
        let output = r#"
[INFO] Tests run: 5, Failures: 1, Errors: 0, Skipped: 1
[ERROR]   com.acme.AppTest.shouldWork -- Time elapsed: 0.01 s <<< FAILURE!
[ERROR] at com.acme.AppTest.shouldWork(AppTest.java:27)
"#;
        let summary = parse_test_output(output);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.failures, 1);
        assert_eq!(summary.skipped, 1);
        assert!(!summary.failures_detail.is_empty());
    }

    #[test]
    fn detects_maven_and_gradle_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::write(dir.path().join("mvnw.cmd"), "mvnw").unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let (tool, binary) = detect_test_tool(&project).unwrap();
        assert_eq!(tool, TestTool::Maven);
        assert!(binary.ends_with("mvnw.cmd"));
    }
}
