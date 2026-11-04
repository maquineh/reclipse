//! Project classpath resolution: Eclipse, Maven, Gradle, plain folders and JDK.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use reclipse_core::{BuildSystem, Project};

use crate::maven::{self, Coordinate};

#[derive(Debug, Clone)]
pub struct Jdk {
    pub home: PathBuf,
    pub version: String,
    pub major: u32,
    pub jmods: Vec<PathBuf>,
    pub rt_jar: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ClasspathResolution {
    pub sources: Vec<PathBuf>,
    pub jars: Vec<PathBuf>,
    pub jdk: Option<Jdk>,
    pub jdk_target: Option<u32>,
    pub warnings: Vec<String>,
    pub build_system: BuildSystem,
}

pub fn resolve(project: &Project) -> Result<ClasspathResolution> {
    let mut sources = project.source_roots.clone();
    let mut jars = Vec::new();
    let mut warnings = Vec::new();

    match project.build_system {
        BuildSystem::Eclipse => {
            resolve_eclipse(project, &mut sources, &mut jars, &mut warnings);
        }
        BuildSystem::Maven => {
            resolve_maven(project, &mut sources, &mut jars, &mut warnings);
        }
        BuildSystem::Gradle => {
            resolve_gradle(project, &mut jars, &mut warnings);
        }
        BuildSystem::Plain => {}
    }
    let jdk_target = detect_jdk_target(project);
    let jdk = find_jdk_for_target(jdk_target);
    if let (Some(target), Some(jdk)) = (jdk_target, &jdk) {
        if jdk.major < target {
            warnings.push(format!(
                "JDK {} é menor que o target Java {target}; usando o maior JDK disponível",
                jdk.version
            ));
        }
    }

    for dir in ["lib", "libs"] {
        collect_jars_from_dir(&project.root.join(dir), &mut jars, false);
    }

    dedup(&mut sources);
    dedup(&mut jars);

    Ok(ClasspathResolution {
        sources,
        jars,
        jdk,
        jdk_target,
        warnings,
        build_system: project.build_system,
    })
}

fn resolve_eclipse(
    project: &Project,
    sources: &mut Vec<PathBuf>,
    jars: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) {
    let Ok(classpath) = std::fs::read_to_string(project.root.join(".classpath")) else {
        return;
    };
    for line in classpath.lines() {
        if !line.contains("<classpathentry") {
            continue;
        }
        let kind = xml_attr(line, "kind").unwrap_or_default();
        let path = xml_attr(line, "path").unwrap_or_default();
        match kind.as_str() {
            "src" => {
                if path.is_empty() || path.starts_with('/') {
                    continue;
                }
                let candidate = project.root.join(&path);
                if candidate.is_dir() {
                    sources.push(candidate);
                }
            }
            "lib" => {
                if path.is_empty() {
                    continue;
                }
                if path.starts_with('/') {
                    warnings.push(format!(
                        "classpath Eclipse com projeto externo não resolvido: {path}"
                    ));
                    continue;
                }
                let candidate = project.root.join(&path);
                if candidate.is_file() {
                    jars.push(candidate);
                } else if candidate.is_dir() {
                    collect_jars_from_dir(&candidate, jars, true);
                } else {
                    warnings.push(format!("entrada de classpath não encontrada: {path}"));
                }
            }
            "con" => {
                if path.contains("JRE_CONTAINER") {
                    // The JDK is discovered separately.
                } else if !path.is_empty() {
                    warnings.push(format!("container Eclipse não resolvido: {path}"));
                }
            }
            "var" => warnings.push(format!(
                "variável de classpath Eclipse não resolvida: {path}"
            )),
            "output" => {}
            other => {
                if !other.is_empty() {
                    warnings.push(format!("entrada de classpath Eclipse ignorada ({other})"));
                }
            }
        }
    }
}

fn resolve_maven(
    project: &Project,
    sources: &mut Vec<PathBuf>,
    jars: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) {
    let pom = project.root.join("pom.xml");
    let Ok(text) = std::fs::read_to_string(&pom) else {
        return;
    };
    let Some(repo) = maven_repository() else {
        warnings.push("repositório Maven local não encontrado (~/.m2/repository)".to_owned());
        return;
    };

    let resolution = maven::resolve_pom(&pom, &repo);
    jars.extend(resolution.jars);
    warnings.extend(resolution.warnings);

    for (tag, fallback) in [
        ("sourceDirectory", "src/main/java"),
        ("testSourceDirectory", "src/test/java"),
    ] {
        if let Some(path) = element_text(&text, tag) {
            let candidate = project.root.join(path);
            if candidate.is_dir() {
                sources.push(candidate);
            } else if project.root.join(fallback).is_dir() {
                sources.push(project.root.join(fallback));
            }
        }
    }
}

fn resolve_gradle(project: &Project, jars: &mut Vec<PathBuf>, warnings: &mut Vec<String>) {
    let Some(repo) = maven_repository() else {
        warnings.push("repositório Maven local não encontrado para dependências Gradle".to_owned());
        return;
    };
    let mut text = String::new();
    for name in ["build.gradle", "build.gradle.kts"] {
        if let Ok(contents) = std::fs::read_to_string(project.root.join(name)) {
            text.push_str(&contents);
            text.push('\n');
        }
    }

    let mut coordinates = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(coordinate) = gradle_map_coordinate(trimmed) {
            coordinates.push(coordinate);
            continue;
        }
        for quote in ['\'', '"'] {
            let Some(start) = trimmed.find(quote) else {
                continue;
            };
            let Some(end) = trimmed[start + 1..].find(quote) else {
                continue;
            };
            let value = &trimmed[start + 1..start + 1 + end];
            let parts: Vec<&str> = value.split(':').collect();
            if parts.len() != 3 && parts.len() != 4 {
                continue;
            }
            let (group, artifact, version) = (parts[0], parts[1], parts[2]);
            if group.is_empty() || artifact.is_empty() || version.is_empty() {
                continue;
            }
            if version.contains("${") || version.contains("$(") {
                warnings.push(format!(
                    "versão Gradle dinâmica não resolvida: {group}:{artifact}:{version}"
                ));
                continue;
            }
            coordinates.push(Coordinate {
                group: group.to_owned(),
                artifact: artifact.to_owned(),
                version: version.to_owned(),
                classifier: parts.get(3).map(|value| (*value).to_owned()),
            });
        }
    }
    dedup_coordinates(&mut coordinates);
    if coordinates.is_empty() {
        return;
    }
    let resolution = maven::resolve_coordinates(&repo, &coordinates);
    jars.extend(resolution.jars);
    warnings.extend(resolution.warnings);
}

fn gradle_map_coordinate(line: &str) -> Option<Coordinate> {
    if !line.contains("group:") || !line.contains("name:") || !line.contains("version:") {
        return None;
    }
    let group = quoted_after(line, "group:")?;
    let artifact = quoted_after(line, "name:")?;
    let version = quoted_after(line, "version:")?;
    if version.contains("${") || version.contains("$(") {
        return None;
    }
    Some(Coordinate {
        group,
        artifact,
        version,
        classifier: None,
    })
}

fn quoted_after(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = line[start..].trim_start();
    let quote = rest
        .chars()
        .next()
        .filter(|value| *value == '\'' || *value == '"')?;
    let rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

fn dedup_coordinates(coordinates: &mut Vec<Coordinate>) {
    let mut seen = HashSet::new();
    coordinates.retain(|coordinate| seen.insert(coordinate.full()));
}

fn collect_jars_from_dir(dir: &Path, jars: &mut Vec<PathBuf>, recursive: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_jars_from_dir(&path, jars, true);
            }
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        {
            jars.push(path);
        }
    }
}

fn maven_repository() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("M2_REPO") {
        return Some(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(home).join(".m2").join("repository"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join(".m2").join("repository"));
    }
    None
}

fn xml_attr(line: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{name}={quote}");
        if let Some(start) = line.find(&pattern) {
            let value_start = start + pattern.len();
            if let Some(length) = line[value_start..].find(quote) {
                return Some(line[value_start..value_start + length].to_owned());
            }
        }
    }
    None
}

fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_owned())
}

fn dedup(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

/// Detect the Java target used by the project (Maven, Gradle or Eclipse).
pub fn detect_jdk_target(project: &Project) -> Option<u32> {
    match project.build_system {
        BuildSystem::Maven => maven_target(&project.root.join("pom.xml")),
        BuildSystem::Gradle => gradle_target(&project.root),
        BuildSystem::Eclipse => eclipse_target(&project.root),
        BuildSystem::Plain => None,
    }
}

fn maven_target(pom: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pom).ok()?;
    for tag in [
        "maven.compiler.release",
        "maven.compiler.target",
        "maven.compiler.source",
        "release",
        "target",
        "source",
    ] {
        if let Some(value) = element_text(&text, tag) {
            let value = resolve_property(&text, &value);
            if let Some(major) = parse_java_version(&value) {
                return Some(major);
            }
        }
    }
    None
}

fn resolve_property(text: &str, value: &str) -> String {
    let trimmed = value.trim();
    if let Some(key) = trimmed
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        return element_text(text, key).unwrap_or_else(|| trimmed.to_owned());
    }
    trimmed.to_owned()
}

fn gradle_target(root: &Path) -> Option<u32> {
    let mut text = String::new();
    for name in ["build.gradle", "build.gradle.kts"] {
        if let Ok(contents) = std::fs::read_to_string(root.join(name)) {
            text.push_str(&contents);
            text.push('\n');
        }
    }
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains("languageVersion") || trimmed.contains("toolchain") {
            if let Some(major) = version_in(trimmed) {
                return Some(major);
            }
        }
        if trimmed.contains("options.release")
            || trimmed.contains("sourceCompatibility")
            || trimmed.contains("targetCompatibility")
            || trimmed.contains("JavaVersion.VERSION_")
        {
            if let Some(major) = version_in(trimmed) {
                return Some(major);
            }
        }
    }
    None
}

fn eclipse_target(root: &Path) -> Option<u32> {
    if let Ok(prefs) = std::fs::read_to_string(root.join(".settings/org.eclipse.jdt.core.prefs")) {
        for key in [
            "org.eclipse.jdt.core.compiler.release",
            "org.eclipse.jdt.core.compiler.compliance",
            "org.eclipse.jdt.core.compiler.codegen.targetPlatform",
            "org.eclipse.jdt.core.compiler.source",
        ] {
            if let Some(value) = prefs
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")))
            {
                if let Some(major) = parse_java_version(value) {
                    return Some(major);
                }
            }
        }
    }
    if let Ok(classpath) = std::fs::read_to_string(root.join(".classpath")) {
        if let Some(start) = classpath.find("JavaSE-") {
            let value = &classpath[start + "JavaSE-".len()..];
            let digits: String = value
                .chars()
                .take_while(|character| character.is_ascii_digit() || *character == '.')
                .collect();
            if let Some(major) = parse_java_version(&digits) {
                return Some(major);
            }
        }
    }
    None
}

fn version_in(line: &str) -> Option<u32> {
    let mut token = String::new();
    for character in line.chars() {
        if character.is_ascii_digit() || character == '.' {
            token.push(character);
        } else if !token.is_empty() {
            if let Some(major) = parse_java_version(&token) {
                return Some(major);
            }
            token.clear();
        }
    }
    parse_java_version(&token)
}

pub fn parse_java_version(value: &str) -> Option<u32> {
    let value = value.trim().trim_matches(['"', '\'']);
    let numbers: Vec<u32> = value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    match numbers.as_slice() {
        [] => None,
        [1, second, ..] => Some(*second),
        [first, ..] => Some(*first),
    }
}

/// Find the best installed JDK, preferring one compatible with `target`.
pub fn find_jdk_for_target(target: Option<u32>) -> Option<Jdk> {
    let mut candidates = Vec::new();
    for env in ["RECLIPSE_JDK", "JAVA_HOME"] {
        if let Some(value) = std::env::var_os(env) {
            candidates.push(PathBuf::from(value));
        }
    }
    if cfg!(windows) {
        candidates.extend(scan_jdk_dirs(Path::new(r"C:\Program Files\Java")));
        candidates.extend(scan_jdk_dirs(Path::new(
            r"C:\Program Files\Eclipse Adoptium",
        )));
        candidates.extend(scan_jdk_dirs(Path::new(r"C:\Program Files\Microsoft")));
    } else {
        candidates.extend(scan_jdk_dirs(Path::new("/usr/lib/jvm")));
    }

    let jdks: Vec<Jdk> = candidates
        .into_iter()
        .filter_map(|home| jdk_from_home(&home))
        .collect();
    select_jdk(jdks, target)
}

/// Kept for callers that just need the newest installed JDK.
pub fn find_jdk() -> Option<Jdk> {
    find_jdk_for_target(None)
}

fn select_jdk(mut jdks: Vec<Jdk>, target: Option<u32>) -> Option<Jdk> {
    if jdks.is_empty() {
        return None;
    }
    jdks.sort_by_key(|jdk| std::cmp::Reverse(jdk.major));
    let Some(target) = target else {
        return jdks.into_iter().next();
    };
    if let Some(exact) = jdks.iter().find(|jdk| jdk.major == target) {
        return Some(exact.clone());
    }
    if let Some(next) = jdks
        .iter()
        .filter(|jdk| jdk.major > target)
        .min_by_key(|jdk| jdk.major)
    {
        return Some(next.clone());
    }
    jdks.into_iter().next()
}

fn scan_jdk_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn jdk_from_home(home: &Path) -> Option<Jdk> {
    if !home.is_dir() {
        return None;
    }
    let jmods_dir = home.join("jmods");
    let mut jmods = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&jmods_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jmod"))
            {
                jmods.push(path);
            }
        }
    }
    jmods.sort();
    let rt_jar = [home.join("jre/lib/rt.jar"), home.join("lib/rt.jar")]
        .into_iter()
        .find(|path| path.is_file());
    if jmods.is_empty() && rt_jar.is_none() {
        return None;
    }
    let version = std::fs::read_to_string(home.join("release"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("JAVA_VERSION=")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| {
            home.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let major = parse_java_version(&version).unwrap_or(0);
    Some(Jdk {
        home: home.to_path_buf(),
        version,
        major,
        jmods,
        rt_jar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_eclipse_library_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("dep.jar"), b"jar").unwrap();
        fs::write(
            root.join(".classpath"),
            r#"<classpath>
              <classpathentry kind="src" path="src"/>
              <classpathentry kind="lib" path="lib/dep.jar"/>
              <classpathentry kind="con" path="org.eclipse.jdt.launching.JRE_CONTAINER"/>
            </classpath>"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("pom.xml"), "").unwrap();
        let project = reclipse_core::discover_project(root).unwrap();
        let mut jars = Vec::new();
        let mut warnings = Vec::new();
        resolve_eclipse(&project, &mut Vec::new(), &mut jars, &mut warnings);
        assert_eq!(jars.len(), 1);
        assert!(jars[0].ends_with("lib/dep.jar"));
    }

    #[test]
    fn detects_java_versions() {
        assert_eq!(parse_java_version("1.8"), Some(8));
        assert_eq!(parse_java_version("17"), Some(17));
        assert_eq!(parse_java_version("VERSION_21"), Some(21));
        assert_eq!(parse_java_version("25.0.2+10"), Some(25));
        assert_eq!(parse_java_version("nope"), None);
    }

    #[test]
    fn detects_maven_and_gradle_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("pom.xml"),
            r#"<project><properties>
              <maven.compiler.release>21</maven.compiler.release>
            </properties></project>"#,
        )
        .unwrap();
        let maven = reclipse_core::discover_project(root).unwrap();
        assert_eq!(detect_jdk_target(&maven), Some(21));

        fs::remove_file(root.join("pom.xml")).unwrap();
        fs::write(
            root.join("build.gradle.kts"),
            "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }\n",
        )
        .unwrap();
        let gradle = reclipse_core::discover_project(root).unwrap();
        assert_eq!(detect_jdk_target(&gradle), Some(17));
    }

    #[test]
    fn selects_closest_jdk_for_target() {
        let jdk = |major: u32| Jdk {
            home: PathBuf::from(format!("jdk-{major}")),
            version: major.to_string(),
            major,
            jmods: Vec::new(),
            rt_jar: None,
        };
        assert_eq!(
            select_jdk(vec![jdk(8), jdk(17), jdk(21), jdk(25)], Some(17))
                .unwrap()
                .major,
            17
        );
        assert_eq!(
            select_jdk(vec![jdk(8), jdk(21), jdk(25)], Some(17))
                .unwrap()
                .major,
            21
        );
        assert_eq!(
            select_jdk(vec![jdk(8), jdk(11)], Some(17)).unwrap().major,
            11
        );
        assert_eq!(
            select_jdk(vec![jdk(8), jdk(21), jdk(25)], None)
                .unwrap()
                .major,
            25
        );
    }

    #[test]
    fn detects_eclipse_compliance() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join(".settings");
        fs::create_dir_all(&settings).unwrap();
        fs::write(
            settings.join("org.eclipse.jdt.core.prefs"),
            "org.eclipse.jdt.core.compiler.compliance=17\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(".classpath"),
            r#"<classpath><classpathentry kind="con" path="org.eclipse.jdt.launching.JRE_CONTAINER/org.eclipse.jdt.internal.debug.ui.launcher.StandardVMType/JavaSE-17"/></classpath>"#,
        )
        .unwrap();
        assert_eq!(eclipse_target(dir.path()), Some(17));
    }
}
