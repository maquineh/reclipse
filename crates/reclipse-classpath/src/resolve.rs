//! Project classpath resolution: Eclipse, Maven, Gradle, plain folders and JDK.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use reclipse_core::{BuildSystem, Project};

#[derive(Debug, Clone)]
pub struct Jdk {
    pub home: PathBuf,
    pub version: String,
    pub jmods: Vec<PathBuf>,
    pub rt_jar: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ClasspathResolution {
    pub sources: Vec<PathBuf>,
    pub jars: Vec<PathBuf>,
    pub jdk: Option<Jdk>,
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
    let jdk = find_jdk();

    for dir in ["lib", "libs"] {
        collect_jars_from_dir(&project.root.join(dir), &mut jars, false);
    }

    dedup(&mut sources);
    dedup(&mut jars);

    Ok(ClasspathResolution {
        sources,
        jars,
        jdk,
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
    let Ok(text) = std::fs::read_to_string(project.root.join("pom.xml")) else {
        return;
    };
    let Some(repo) = maven_repository() else {
        warnings.push("repositório Maven local não encontrado (~/.m2/repository)".to_owned());
        return;
    };

    let mut properties = parse_xml_properties(&text);
    let project_group = element_text(&text, "groupId").unwrap_or_default();
    let project_artifact = element_text(&text, "artifactId").unwrap_or_default();
    let project_version = element_text(&text, "version").unwrap_or_default();
    properties
        .entry("project.groupId".to_owned())
        .or_insert(project_group);
    properties
        .entry("project.artifactId".to_owned())
        .or_insert(project_artifact);
    properties
        .entry("project.version".to_owned())
        .or_insert(project_version);

    for dependency in dependency_blocks(&text) {
        let group = element_text(&dependency, "groupId").unwrap_or_default();
        let artifact = element_text(&dependency, "artifactId").unwrap_or_default();
        let version = element_text(&dependency, "version").unwrap_or_default();
        if group.is_empty() || artifact.is_empty() {
            continue;
        }
        let version = substitute(&version, &properties);
        if version.is_empty() || version.contains("${") {
            warnings.push(format!(
                "dependência Maven sem versão resolvida: {group}:{artifact}:{version}"
            ));
            continue;
        }
        let classifier = element_text(&dependency, "classifier")
            .map(|value| substitute(&value, &properties))
            .filter(|value| !value.is_empty());
        match locate_maven_jar(&repo, &group, &artifact, &version, classifier.as_deref()) {
            Some(jar) => jars.push(jar),
            None => warnings.push(format!(
                "JAR Maven não encontrado: {group}:{artifact}:{version}"
            )),
        }
    }

    // A sourceDirectory outside the conventional layout.
    for (tag, fallback) in [
        ("sourceDirectory", "src/main/java"),
        ("testSourceDirectory", "src/test/java"),
    ] {
        if let Some(path) = element_text(&text, tag) {
            let path = substitute(&path, &properties);
            let candidate = project.root.join(path);
            if candidate.is_dir() {
                sources.push(candidate);
            } else if project.root.join(fallback).is_dir() {
                sources.push(project.root.join(fallback));
            }
        }
    }
}

fn dependency_blocks(xml: &str) -> Vec<String> {
    let management = block_ranges(xml, "dependencyManagement");
    let mut blocks = Vec::new();
    let mut position = 0;
    while let Some(start) = xml[position..].find("<dependency>") {
        let start = position + start;
        if management
            .iter()
            .any(|(from, to)| start >= *from && start < *to)
        {
            position = start + "<dependency>".len();
            continue;
        }
        let Some(end) = xml[start..].find("</dependency>") else {
            break;
        };
        let end = start + end + "</dependency>".len();
        blocks.push(xml[start..end].to_owned());
        position = end;
    }
    blocks
}

fn block_ranges(xml: &str, tag: &str) -> Vec<(usize, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut ranges = Vec::new();
    let mut position = 0;
    while let Some(start) = xml[position..].find(&open) {
        let start = position + start;
        let Some(end) = xml[start..].find(&close) else {
            break;
        };
        let end = start + end + close.len();
        ranges.push((start, end));
        position = end;
    }
    ranges
}

fn parse_xml_properties(xml: &str) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    let Some(start) = xml.find("<properties>") else {
        return properties;
    };
    let Some(end) = xml[start..].find("</properties>") else {
        return properties;
    };
    let block = &xml[start + "<properties>".len()..start + end];
    let mut position = 0;
    while let Some(open) = block[position..].find('<') {
        let open = position + open;
        let Some(close) = block[open..].find('>') else {
            break;
        };
        let close = open + close;
        let name = block[open + 1..close].trim();
        if name.starts_with('/') || name.starts_with('?') || name.starts_with('!') {
            position = close + 1;
            continue;
        }
        let Some(end_tag) = block[close..].find(&format!("</{name}>")) else {
            position = close + 1;
            continue;
        };
        let value = block[close + 1..close + end_tag].trim().to_owned();
        properties.insert(name.to_owned(), value);
        position = close + end_tag + name.len() + 3;
    }
    properties
}

fn substitute(value: &str, properties: &HashMap<String, String>) -> String {
    let mut result = value.to_owned();
    for _ in 0..5 {
        let Some(start) = result.find("${") else {
            break;
        };
        let Some(end) = result[start..].find('}') else {
            break;
        };
        let key = &result[start + 2..start + end];
        let Some(replacement) = properties.get(key) else {
            break;
        };
        result.replace_range(start..start + end + 1, replacement);
    }
    result
}

fn locate_maven_jar(
    repo: &Path,
    group: &str,
    artifact: &str,
    version: &str,
    classifier: Option<&str>,
) -> Option<PathBuf> {
    let base = repo
        .join(group.replace('.', "/"))
        .join(artifact)
        .join(version);
    if let Some(classifier) = classifier {
        let path = base.join(format!("{artifact}-{version}-{classifier}.jar"));
        return path.is_file().then_some(path);
    }
    let primary = base.join(format!("{artifact}-{version}.jar"));
    if primary.is_file() {
        return Some(primary);
    }
    let entries = std::fs::read_dir(&base).ok()?;
    let mut fallback = None;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy();
        if name.ends_with(".jar")
            && !name.ends_with("-sources.jar")
            && !name.ends_with("-javadoc.jar")
            && fallback.is_none()
        {
            fallback = Some(path);
        }
    }
    fallback
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
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        let Some(quote) = trimmed.find(['\'', '"']) else {
            continue;
        };
        let value = &trimmed[quote + 1..];
        let Some(end) = value.find(['\'', '"']) else {
            continue;
        };
        let coordinate = &value[..end];
        let parts: Vec<&str> = coordinate.split(':').collect();
        if parts.len() != 3 && parts.len() != 4 {
            continue;
        }
        let (group, artifact, version) = (parts[0], parts[1], parts[2]);
        let classifier = parts.get(3).copied();
        if group.is_empty() || artifact.is_empty() || version.is_empty() {
            continue;
        }
        if let Some(jar) = locate_maven_jar(&repo, group, artifact, version, classifier) {
            jars.push(jar);
        } else {
            warnings.push(format!(
                "JAR Gradle não encontrado: {group}:{artifact}:{version}"
            ));
        }
    }
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

pub fn find_jdk() -> Option<Jdk> {
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

    let mut jdks: Vec<Jdk> = candidates
        .into_iter()
        .filter_map(|home| jdk_from_home(&home))
        .collect();
    jdks.sort_by_key(|jdk| std::cmp::Reverse(version_key(&jdk.version)));
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
    Some(Jdk {
        home: home.to_path_buf(),
        version,
        jmods,
        rt_jar,
    })
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split(['.', '_', '-'])
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
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
    fn resolves_maven_property_version_and_jar() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let jar_dir = repo.join("com/acme/app/1.2.3");
        fs::create_dir_all(&jar_dir).unwrap();
        fs::write(jar_dir.join("app-1.2.3.jar"), b"jar").unwrap();
        assert!(locate_maven_jar(&repo, "com.acme", "app", "1.2.3", None).is_some());

        let xml = r#"<project>
            <properties><app.version>1.2.3</app.version></properties>
            <dependencies>
              <dependency>
                <groupId>com.acme</groupId><artifactId>app</artifactId>
                <version>${app.version}</version>
              </dependency>
            </dependencies>
        </project>"#;
        let mut properties = parse_xml_properties(xml);
        assert_eq!(substitute("${app.version}", &properties), "1.2.3");
        properties.insert("project.version".to_owned(), "1.2.3".to_owned());
        assert_eq!(dependency_blocks(xml).len(), 1);
    }

    #[test]
    fn ignores_dependency_management_blocks() {
        let xml = r#"<project>
            <dependencyManagement><dependencies><dependency><groupId>a</groupId></dependency></dependencies></dependencyManagement>
            <dependencies><dependency><groupId>b</groupId></dependency></dependencies>
        </project>"#;
        let blocks = dependency_blocks(xml);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("<groupId>b</groupId>"));
    }
}
