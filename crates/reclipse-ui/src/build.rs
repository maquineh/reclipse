//! Minimal M3 build/run layer: compile the project with `javac` and run a
//! detected `main` class with `java`.
//!
//! This is intentionally a *fallback*: it uses the resolved classpath and the
//! project source files, does not interpret build lifecycles (Maven/Gradle
//! plugins, resources, tests) and expects a JDK with `javac`/`java`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

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

pub fn compile(project: &Project, env: &BuildEnv) -> Result<CompileResult, String> {
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
    let output = Command::new(&javac)
        .arg(format!("@{}", argfile.display()))
        .output()
        .map_err(|error| format!("falha ao executar {}: {error}", javac.display()))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(CompileResult {
        success: output.status.success(),
        diagnostics: parse_javac_output(&combined),
        output: combined,
        elapsed_ms: started.elapsed().as_millis(),
        classes_dir,
    })
}

pub fn run_main(
    main_class: &str,
    env: &BuildEnv,
    compiled: &CompileResult,
) -> Result<RunResult, String> {
    let started = Instant::now();
    let java = jdk_binary(env.jdk_home.as_deref(), "java")?;
    let mut entries = Vec::with_capacity(env.jars.len() + 1);
    entries.push(compiled.classes_dir.path().to_path_buf());
    entries.extend(env.jars.iter().cloned());
    let output = Command::new(&java)
        .arg("-cp")
        .arg(join_classpath(&entries))
        .arg(main_class)
        .output()
        .map_err(|error| format!("falha ao executar {}: {error}", java.display()))?;
    Ok(RunResult {
        success: output.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        elapsed_ms: started.elapsed().as_millis(),
    })
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

fn is_test_source(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    normalized.contains("/src/test/") || normalized.contains("/src/test/java/")
}

fn quote_path(path: &Path) -> String {
    // javac argfiles treat backslash as an escape, so use forward slashes
    // (accepted on Windows) and quote paths with spaces.
    let display = path.display().to_string().replace('\\', "/");
    format!("\"{display}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let compiled = compile(&project, &env).unwrap();
        assert!(compiled.success, "javac output: {}", compiled.output);
        let run = run_main("Hello", &env, &compiled).unwrap();
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
}
