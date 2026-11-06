//! Gradle project model via an init script ("Tooling API light").
//!
//! The real Gradle Tooling API needs a JVM-side client. Instead the IDE asks
//! Gradle itself for its model: an init script (`-I`) registers a task that
//! prints one JSON line per project with source roots and external
//! dependencies (`RECLIPSE_MODEL {...}`). The result is cached in
//! `<root>/.reclipse/gradle-model.json` and merged by `resolve_gradle` on
//! every later resolve, so indexing stays fast and offline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::maven::Coordinate;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GradleProjectModel {
    pub path: String,
    pub root: String,
    pub sources: Vec<String>,
    pub deps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GradleModel {
    pub projects: Vec<GradleProjectModel>,
}

pub fn model_cache_path(root: &Path) -> PathBuf {
    root.join(".reclipse").join("gradle-model.json")
}

pub fn load_model_cache(root: &Path) -> Option<GradleModel> {
    let bytes = std::fs::read(model_cache_path(root)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_model_cache(root: &Path, model: &GradleModel) -> Result<PathBuf, String> {
    let path = model_cache_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("falha ao criar .reclipse: {error}"))?;
    }
    let json = serde_json::to_vec_pretty(model)
        .map_err(|error| format!("falha ao serializar modelo: {error}"))?;
    std::fs::write(&path, json).map_err(|error| format!("falha ao gravar modelo: {error}"))?;
    Ok(path)
}

/// Init script text: prints `RECLIPSE_MODEL {...}` per evaluated project.
pub fn model_init_script() -> String {
    r#"allprojects {
    afterEvaluate { p ->
        def srcs = []
        try {
            p.extensions.findByName('sourceSets')?.main?.java?.srcDirs?.each { srcs << it.absolutePath }
        } catch (Exception ignored) {}
        def coords = []
        try {
            p.configurations.findByName('runtimeClasspath')?.incoming?.dependencies?.each { d ->
                if (d.group && d.name && d.version) coords << "${d.group}:${d.name}:${d.version}"
            }
        } catch (Exception ignored) {}
        println('RECLIPSE_MODEL ' + groovy.json.JsonOutput.toJson([path: p.path, root: p.projectDir.absolutePath, sources: srcs, deps: coords]))
    }
}
"#
    .to_owned()
}

/// Parse `RECLIPSE_MODEL {...}` lines from Gradle output.
pub fn parse_model_output(output: &str) -> GradleModel {
    let mut projects = Vec::new();
    for line in output.lines() {
        let Some(json) = line.trim().strip_prefix("RECLIPSE_MODEL ") else {
            continue;
        };
        if let Ok(project) = serde_json::from_str::<GradleProjectModel>(json) {
            projects.push(project);
        }
    }
    GradleModel { projects }
}

/// Run Gradle to dump the model and cache it. Best effort with a deadline;
/// returns the model on success. Network stays off (`--offline`): only the
/// local Gradle cache and already-resolved state are used.
pub fn fetch_model(
    root: &Path,
    gradle_bin: &Path,
    timeout: Duration,
    on_output: &mut dyn FnMut(&str),
) -> Result<GradleModel, String> {
    let script_path = std::env::temp_dir().join(format!(
        "reclipse-gradle-model-{}.gradle",
        std::process::id()
    ));
    std::fs::write(&script_path, model_init_script())
        .map_err(|error| format!("falha ao gerar init script: {error}"))?;
    let mut child = Command::new(gradle_bin)
        .args([
            "-q",
            "--offline",
            "--console=plain",
            "-I",
            &script_path.display().to_string(),
            "help",
        ])
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("falha ao iniciar Gradle: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Gradle excedeu {} s ao exportar o modelo",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("falha ao aguardar Gradle: {error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("falha ao ler saída do Gradle: {error}"))?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let _ = std::fs::remove_file(&script_path);
    for line in combined.lines() {
        on_output(line);
    }
    let model = parse_model_output(&combined);
    if model.projects.is_empty() {
        return Err("Gradle não retornou nenhum projeto (verifique o build)".to_owned());
    }
    save_model_cache(root, &model)?;
    Ok(model)
}

/// Merge a cached model: source roots plus parsed coordinates.
pub fn model_sources(model: &GradleModel) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for project in &model.projects {
        for source in &project.sources {
            let path = PathBuf::from(source);
            if path.is_dir() {
                sources.push(path);
            }
        }
    }
    sources
}

pub fn model_coordinates(model: &GradleModel) -> Vec<Coordinate> {
    let mut seen = std::collections::HashSet::new();
    let mut coordinates = Vec::new();
    for project in &model.projects {
        for dep in &project.deps {
            let parts: Vec<&str> = dep.split(':').collect();
            if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
                continue;
            }
            let key = (*dep).to_owned();
            if seen.insert(key) {
                coordinates.push(Coordinate {
                    group: parts[0].to_owned(),
                    artifact: parts[1].to_owned(),
                    version: parts[2].to_owned(),
                    classifier: None,
                });
            }
        }
    }
    coordinates
}

/// Source roots that exist on disk but live outside the conventional
/// `src/main|test/java` (Kotlin/Groovy folders are skipped by name).
pub fn extra_source_roots(root: &Path) -> Vec<PathBuf> {
    let mut extra = Vec::new();
    for candidate in [
        root.join("src/main/kotlin"),
        root.join("src/test/kotlin"),
        root.join("src/main/groovy"),
        root.join("src/test/groovy"),
        root.join("src/main/resources"),
    ] {
        if candidate.is_dir() {
            extra.push(candidate);
        }
    }
    extra
}

pub fn gradle_binaries(root: &Path) -> Vec<PathBuf> {
    let mut binaries = Vec::new();
    for name in ["gradlew.bat", "gradlew"] {
        let candidate = root.join(name);
        if candidate.is_file() {
            binaries.push(candidate);
        }
    }
    for name in ["gradle.bat", "gradle"] {
        binaries.push(PathBuf::from(name));
    }
    binaries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_lines_and_ignores_noise() {
        let output = "Starting Gradle...\nRECLIPSE_MODEL {\"path\":\":\",\"root\":\"/proj\",\"sources\":[\"/proj/src/main/java\"],\"deps\":[\"com.acme:app:1.0\"]}\nRECLIPSE_MODEL {\"path\":\":app\",\"root\":\"/proj/app\",\"sources\":[],\"deps\":[\"bad\",\"com.acme:app:1.0\"]}\nBUILD SUCCESSFUL\n";
        let model = parse_model_output(output);
        assert_eq!(model.projects.len(), 2);
        let coordinates = model_coordinates(&model);
        assert_eq!(coordinates.len(), 1);
        assert_eq!(coordinates[0].group, "com.acme");
    }

    #[test]
    fn roundtrips_model_cache() {
        let dir = tempfile::tempdir().unwrap();
        let model = GradleModel {
            projects: vec![GradleProjectModel {
                path: ":".to_owned(),
                root: dir.path().display().to_string(),
                sources: vec![dir.path().join("src").display().to_string()],
                deps: vec!["com.acme:app:1.0".to_owned()],
            }],
        };
        let path = save_model_cache(dir.path(), &model).unwrap();
        assert!(path.ends_with("gradle-model.json"));
        let loaded = load_model_cache(dir.path()).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].deps, vec!["com.acme:app:1.0".to_owned()]);
    }

    #[test]
    fn init_script_requests_model_dump() {
        let script = model_init_script();
        assert!(script.contains("RECLIPSE_MODEL"));
        assert!(script.contains("runtimeClasspath"));
        assert!(script.contains("JsonOutput"));
    }
}
