//! Minimal POM model and transitive dependency resolution against the local
//! Maven repository.
//!
//! There is deliberately no network access: dependencies are resolved only when
//! their JAR/POM already exists under `~/.m2/repository` (or `M2_REPO`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct MavenResolution {
    pub jars: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub classifier: Option<String>,
}

impl Coordinate {
    pub fn key(&self) -> String {
        format!("{}:{}", self.group, self.artifact)
    }

    pub fn full(&self) -> String {
        match &self.classifier {
            Some(classifier) => format!(
                "{}:{}:{}:{}",
                self.group, self.artifact, self.version, classifier
            ),
            None => format!("{}:{}:{}", self.group, self.artifact, self.version),
        }
    }
}

#[derive(Debug, Clone)]
struct Dependency {
    coordinate: Coordinate,
    scope: String,
    optional: bool,
    dependency_type: String,
    exclusions: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
struct Managed {
    version: Option<String>,
    scope: Option<String>,
    optional: bool,
    exclusions: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
struct Pom {
    coordinate: Coordinate,
    packaging: String,
    properties: HashMap<String, String>,
    management: HashMap<String, Managed>,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone)]
struct QueueItem {
    dependency: Dependency,
    depth: usize,
    exclusions: HashSet<(String, String)>,
}

/// Resolve the dependencies of a project `pom.xml`.
pub fn resolve_pom(pom_path: &Path, repo: &Path) -> MavenResolution {
    let mut resolver = Resolver::new(repo);
    let Some(root) = resolver.load_pom_file(pom_path, 0) else {
        return MavenResolution {
            warnings: resolver.warnings,
            ..MavenResolution::default()
        };
    };
    resolver.resolve_from_root(&root)
}

/// Resolve a list of Gradle-style coordinates (direct roots) transitively.
pub fn resolve_coordinates(repo: &Path, coordinates: &[Coordinate]) -> MavenResolution {
    resolve_gradle(repo, coordinates, &[])
}

/// Resolve Gradle coordinates, applying dependency management from platform BOMs.
pub fn resolve_gradle(
    repo: &Path,
    coordinates: &[Coordinate],
    boms: &[Coordinate],
) -> MavenResolution {
    let mut resolver = Resolver::new(repo);
    let mut management = HashMap::new();
    for bom in boms {
        if let Some(pom) = resolver.load_pom_coordinate(bom, 0) {
            for (key, managed) in pom.management {
                management.insert(key, managed);
            }
        } else {
            resolver.warn(format!(
                "BOM Gradle não encontrado no repositório local: {}",
                bom.full()
            ));
        }
    }
    let root = Pom {
        packaging: "jar".to_owned(),
        dependencies: coordinates
            .iter()
            .cloned()
            .map(|coordinate| Dependency {
                coordinate,
                scope: "compile".to_owned(),
                optional: false,
                dependency_type: "jar".to_owned(),
                exclusions: Vec::new(),
            })
            .collect(),
        management,
        ..Pom::default()
    };
    resolver.resolve_from_root(&root)
}

struct Resolver {
    repo: PathBuf,
    warnings: Vec<String>,
    poms: HashMap<PathBuf, Option<Pom>>,
    loading: HashSet<PathBuf>,
}

impl Resolver {
    fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            warnings: Vec::new(),
            poms: HashMap::new(),
            loading: HashSet::new(),
        }
    }

    fn resolve_from_root(mut self, root: &Pom) -> MavenResolution {
        let root_management = root.management.clone();
        let mut queue: VecDeque<QueueItem> = root
            .effective_dependencies(&root_management, false)
            .into_iter()
            .map(|dependency| QueueItem {
                dependency,
                depth: 0,
                exclusions: HashSet::new(),
            })
            .collect();

        let mut resolution = MavenResolution::default();
        let mut visited = HashSet::new();
        while let Some(item) = queue.pop_front() {
            let dependency = item.dependency;
            let coordinate = &dependency.coordinate;
            if item
                .exclusions
                .contains(&(coordinate.group.clone(), coordinate.artifact.clone()))
            {
                continue;
            }
            let scope = dependency.scope.as_str();
            let allowed = if item.depth == 0 {
                matches!(scope, "compile" | "runtime" | "provided")
            } else {
                matches!(scope, "compile" | "runtime")
            };
            if !allowed || (dependency.optional && item.depth > 0) {
                continue;
            }
            if coordinate.version.is_empty() || coordinate.version.contains("${") {
                self.warn(format!(
                    "dependência sem versão resolvida: {}",
                    coordinate.full()
                ));
                continue;
            }
            if !visited.insert(coordinate.key()) {
                continue;
            }

            let pom = self.load_pom_coordinate(coordinate, 1);
            let packaging = pom
                .as_ref()
                .map(|pom| pom.packaging.as_str())
                .unwrap_or("jar");
            if packaging != "pom" && dependency.dependency_type != "pom" {
                match locate_jar(&self.repo, coordinate) {
                    Some(jar) => resolution.jars.push(jar),
                    None => self.warn(format!("JAR Maven não encontrado: {}", coordinate.full())),
                }
            }
            resolution.dependencies.push(coordinate.full());

            let Some(pom) = pom else {
                if item.depth > 0 {
                    self.warn(format!(
                        "POM de {} não encontrado; dependências transitivas ignoradas",
                        coordinate.full()
                    ));
                }
                continue;
            };
            if !matches!(scope, "compile" | "runtime") {
                continue;
            }

            let mut child_exclusions = item.exclusions.clone();
            for (group, artifact) in &dependency.exclusions {
                child_exclusions.insert((group.clone(), artifact.clone()));
            }
            for child in pom.effective_dependencies(&root_management, true) {
                if child_exclusions.contains(&(
                    child.coordinate.group.clone(),
                    child.coordinate.artifact.clone(),
                )) {
                    continue;
                }
                queue.push_back(QueueItem {
                    dependency: child,
                    depth: item.depth + 1,
                    exclusions: child_exclusions.clone(),
                });
            }
        }

        resolution.warnings = self.warnings;
        resolution
    }

    fn load_pom_coordinate(&mut self, coordinate: &Coordinate, depth: usize) -> Option<Pom> {
        if coordinate.version.is_empty() {
            return None;
        }
        let base = self
            .repo
            .join(coordinate.group.replace('.', "/"))
            .join(&coordinate.artifact)
            .join(&coordinate.version);
        let path = base.join(format!(
            "{}-{}.pom",
            coordinate.artifact, coordinate.version
        ));
        self.load_pom_file(&path, depth)
    }

    fn load_pom_file(&mut self, path: &Path, depth: usize) -> Option<Pom> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(cached) = self.poms.get(&path) {
            return cached.clone();
        }
        if depth > 10 {
            self.warn(format!(
                "hierarquia de POM muito profunda: {}",
                path.display()
            ));
            return None;
        }
        if !self.loading.insert(path.clone()) {
            self.warn(format!("ciclo de POM detectado em {}", path.display()));
            return None;
        }
        let parsed = self.parse_pom_file(&path, depth);
        self.loading.remove(&path);
        self.poms.insert(path, parsed.clone());
        parsed
    }

    fn parse_pom_file(&mut self, path: &Path, depth: usize) -> Option<Pom> {
        let xml = std::fs::read_to_string(path).ok()?;
        let content = project_inner(&xml)?;
        let mut properties = parse_properties(content);

        let parent = self.load_parent(content, &properties, path, depth);
        let parent_pom = parent.as_ref().map(|(pom, _)| pom);
        if let Some(parent) = parent_pom {
            let mut merged = parent.properties.clone();
            merged.extend(properties);
            properties = merged;
            insert_project_properties(
                &mut properties,
                &parent.coordinate.group,
                &parent.coordinate.artifact,
                &parent.coordinate.version,
            );
        }

        let group = child_text(content, "groupId")
            .map(|value| substitute(&value, &properties))
            .filter(|value| !value.is_empty())
            .or_else(|| parent_pom.map(|pom| pom.coordinate.group.clone()))
            .unwrap_or_default();
        let artifact = child_text(content, "artifactId")
            .map(|value| substitute(&value, &properties))
            .unwrap_or_default();
        let version = child_text(content, "version")
            .map(|value| substitute(&value, &properties))
            .filter(|value| !value.is_empty())
            .or_else(|| parent_pom.map(|pom| pom.coordinate.version.clone()))
            .unwrap_or_default();
        insert_project_properties(&mut properties, &group, &artifact, &version);

        let packaging = child_text(content, "packaging").unwrap_or_else(|| "jar".to_owned());
        let mut management = parent_pom
            .map(|pom| pom.management.clone())
            .unwrap_or_default();

        let mut declared_management = HashMap::new();
        for block in direct_children(content, "dependencyManagement") {
            for dependencies in direct_children(&block, "dependencies") {
                for dependency in direct_children(&dependencies, "dependency") {
                    let scope = child_text(&dependency, "scope").unwrap_or_default();
                    let dependency_type =
                        child_text(&dependency, "type").unwrap_or_else(|| "jar".to_owned());
                    if scope == "import" && dependency_type == "pom" {
                        let coordinate = coordinate_from_block(&dependency, &properties);
                        if let Some(bom) = self.load_pom_coordinate(&coordinate, depth + 1) {
                            for (key, managed) in bom.management {
                                management.insert(key, managed);
                            }
                        } else {
                            self.warn(format!(
                                "BOM não encontrado no repositório local: {}",
                                coordinate.full()
                            ));
                        }
                    } else if let Some((key, managed)) =
                        managed_from_block(&dependency, &properties)
                    {
                        declared_management.insert(key, managed);
                    }
                }
            }
        }
        // Explicit dependencyManagement entries win over imported BOMs.
        management.extend(declared_management);

        let mut dependencies = parent_pom
            .map(|pom| pom.dependencies.clone())
            .unwrap_or_default();
        self.apply_active_profiles(
            content,
            &mut properties,
            &mut management,
            &mut dependencies,
            depth,
        );
        for dependencies_block in direct_children(content, "dependencies") {
            for dependency in direct_children(&dependencies_block, "dependency") {
                let coordinate = coordinate_from_block(&dependency, &properties);
                if coordinate.group.is_empty() || coordinate.artifact.is_empty() {
                    continue;
                }
                if coordinate.version.contains("${") {
                    self.warn(format!(
                        "dependência Maven sem versão resolvida: {}",
                        coordinate.full()
                    ));
                }
                dependencies.push(Dependency {
                    coordinate,
                    scope: child_text(&dependency, "scope")
                        .map(|value| substitute(&value, &properties))
                        .unwrap_or_else(|| "compile".to_owned()),
                    optional: child_text(&dependency, "optional")
                        .is_some_and(|value| value == "true"),
                    dependency_type: child_text(&dependency, "type")
                        .unwrap_or_else(|| "jar".to_owned()),
                    exclusions: parse_exclusions(&dependency, &properties),
                });
            }
        }

        Some(Pom {
            coordinate: Coordinate {
                group,
                artifact,
                version,
                classifier: None,
            },
            packaging,
            properties,
            management,
            dependencies,
        })
    }

    /// Evaluate `<profiles>` and merge the active ones into the model.
    /// Activation supports `activeByDefault`, `jdk` ranges, `os` and
    /// `property` (checked against effective properties, then env vars).
    /// `RECLIPSE_JDK_VERSION` overrides the detected `java -version` major
    /// (used by tests). Without a detectable JDK, `jdk` activation does not
    /// match.
    fn apply_active_profiles(
        &mut self,
        content: &str,
        properties: &mut HashMap<String, String>,
        management: &mut HashMap<String, Managed>,
        dependencies: &mut Vec<Dependency>,
        depth: usize,
    ) {
        let mut candidates: Vec<(String, bool, bool, String)> = Vec::new();
        for profiles in direct_children(content, "profiles") {
            for profile in direct_children(&profiles, "profile") {
                let id = child_text(&profile, "id").unwrap_or_default();
                let activation = direct_children(&profile, "activation")
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let active_by_default = activation_block_flag(&activation, "activeByDefault");
                let has_conditions = ["jdk", "os", "property", "file"]
                    .iter()
                    .any(|tag| activation.contains(&format!("<{tag}")));
                let active = if has_conditions {
                    self.profile_active(&activation, properties)
                } else {
                    // No explicit conditions: active only via activeByDefault.
                    active_by_default
                };
                candidates.push((id, active, active_by_default, profile));
            }
        }
        // JLS-ish `activeByDefault` rule: a default-active profile stays off
        // when any sibling profile activates through other conditions.
        let any_non_default_active = candidates
            .iter()
            .any(|(_, active, by_default, _)| *active && !by_default);
        for (id, active, by_default, profile) in candidates {
            if !active {
                continue;
            }
            if by_default && any_non_default_active && !profile_has_conditions(&profile) {
                continue;
            }
            // Profile properties win over inherited ones for substitution below.
            for (key, value) in parse_properties(&profile) {
                properties.insert(key, value);
            }
            let mut imported: HashMap<String, Managed> = HashMap::new();
            let mut declared: HashMap<String, Managed> = HashMap::new();
            for block in direct_children(&profile, "dependencyManagement") {
                for dependencies_block in direct_children(&block, "dependencies") {
                    for dependency in direct_children(&dependencies_block, "dependency") {
                        let scope = child_text(&dependency, "scope").unwrap_or_default();
                        let dependency_type =
                            child_text(&dependency, "type").unwrap_or_else(|| "jar".to_owned());
                        if scope == "import" && dependency_type == "pom" {
                            let coordinate = coordinate_from_block(&dependency, properties);
                            if let Some(bom) = self.load_pom_coordinate(&coordinate, depth + 1) {
                                for (key, managed) in bom.management {
                                    imported.insert(key, managed);
                                }
                            } else {
                                self.warn(format!(
                                    "BOM de perfil não encontrado no repositório local: {}",
                                    coordinate.full()
                                ));
                            }
                        } else if let Some((key, managed)) =
                            managed_from_block(&dependency, properties)
                        {
                            declared.insert(key, managed);
                        }
                    }
                }
            }
            for (key, managed) in imported {
                management.insert(key, managed);
            }
            // Explicit entries (profile or main) win over imported BOMs.
            management.extend(declared);
            for dependencies_block in direct_children(&profile, "dependencies") {
                for dependency in direct_children(&dependencies_block, "dependency") {
                    let coordinate = coordinate_from_block(&dependency, properties);
                    if coordinate.group.is_empty() || coordinate.artifact.is_empty() {
                        continue;
                    }
                    dependencies.push(Dependency {
                        coordinate,
                        scope: child_text(&dependency, "scope")
                            .map(|value| substitute(&value, properties))
                            .unwrap_or_else(|| "compile".to_owned()),
                        optional: child_text(&dependency, "optional")
                            .is_some_and(|value| value == "true"),
                        dependency_type: child_text(&dependency, "type")
                            .unwrap_or_else(|| "jar".to_owned()),
                        exclusions: parse_exclusions(&dependency, properties),
                    });
                }
            }
            let _ = id;
        }
    }

    fn profile_active(&mut self, activation: &str, properties: &HashMap<String, String>) -> bool {
        if activation.contains("<file>") {
            // File-based activation (exists/missing) is relative to ad-hoc
            // working directories here; conservatively not matched.
            return false;
        }
        if let Some(jdk) = child_text(activation, "jdk") {
            let Some(major) = self.jdk_major_for_profiles() else {
                return false;
            };
            if !jdk_range_matches(jdk.trim(), major) {
                return false;
            }
        }
        if let Some(os) = direct_children(activation, "os").into_iter().next() {
            if !os_matches(&os) {
                return false;
            }
        }
        if let Some(property) = direct_children(activation, "property").into_iter().next() {
            let name = child_text(&property, "name").unwrap_or_default();
            if name.is_empty() {
                return false;
            }
            let expected = child_text(&property, "value");
            let actual = properties
                .get(&name)
                .cloned()
                .or_else(|| std::env::var(&name).ok());
            match (actual, expected) {
                (Some(actual), Some(expected)) => {
                    if substitute(&actual, properties) != substitute(&expected, properties) {
                        return false;
                    }
                }
                (Some(_), None) => {}
                (None, _) => return false,
            }
        }
        true
    }

    fn jdk_major_for_profiles(&mut self) -> Option<u32> {
        if let Ok(override_value) = std::env::var("RECLIPSE_JDK_VERSION") {
            if let Some(major) = parse_java_major(&override_value) {
                return Some(major);
            }
        }
        let output = std::process::Command::new("java")
            .arg("-version")
            .output()
            .ok()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // `openjdk version "17.0.9" ...` or `java version "1.8.0_361"`.
        let quoted = text.split('"').nth(1).unwrap_or("");
        parse_java_major(quoted)
    }

    fn load_parent(
        &mut self,
        content: &str,
        properties: &HashMap<String, String>,
        pom_path: &Path,
        depth: usize,
    ) -> Option<(Pom, PathBuf)> {
        let parent = direct_children(content, "parent").into_iter().next()?;
        let coordinate = Coordinate {
            group: child_text(&parent, "groupId")
                .map(|value| substitute(&value, properties))
                .unwrap_or_default(),
            artifact: child_text(&parent, "artifactId")
                .map(|value| substitute(&value, properties))
                .unwrap_or_default(),
            version: child_text(&parent, "version")
                .map(|value| substitute(&value, properties))
                .unwrap_or_default(),
            classifier: None,
        };
        if coordinate.group.is_empty()
            || coordinate.artifact.is_empty()
            || coordinate.version.is_empty()
        {
            return None;
        }
        let relative = child_text(&parent, "relativePath");
        if let Some(relative) = relative.filter(|value| !value.trim().is_empty()) {
            let candidate = pom_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(relative);
            if let Some(pom) = self.load_pom_file(&candidate, depth + 1) {
                if parent_matches(&pom, &coordinate) {
                    return Some((pom, candidate));
                }
            }
        } else if !parent.contains("<relativePath/>") && !parent.contains("<relativePath />") {
            let candidate = pom_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("../pom.xml");
            if let Some(pom) = self.load_pom_file(&candidate, depth + 1) {
                if parent_matches(&pom, &coordinate) {
                    return Some((pom, candidate));
                }
            }
        }
        let path = self.pom_path(&coordinate);
        let pom = self.load_pom_file(&path, depth + 1)?;
        Some((pom, path))
    }

    fn pom_path(&self, coordinate: &Coordinate) -> PathBuf {
        self.repo
            .join(coordinate.group.replace('.', "/"))
            .join(&coordinate.artifact)
            .join(&coordinate.version)
            .join(format!(
                "{}-{}.pom",
                coordinate.artifact, coordinate.version
            ))
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < 200 {
            self.warnings.push(message);
        }
    }
}

impl Pom {
    fn effective_dependencies(
        &self,
        root_management: &HashMap<String, Managed>,
        transitive: bool,
    ) -> Vec<Dependency> {
        let mut result = Vec::new();
        for raw in &self.dependencies {
            let mut dependency = raw.clone();
            dependency.coordinate.group =
                substitute(&dependency.coordinate.group, &self.properties);
            dependency.coordinate.artifact =
                substitute(&dependency.coordinate.artifact, &self.properties);
            dependency.coordinate.version =
                substitute(&dependency.coordinate.version, &self.properties);
            dependency.scope = substitute(&dependency.scope, &self.properties);
            dependency.dependency_type = substitute(&dependency.dependency_type, &self.properties);

            let key = dependency.coordinate.key();
            let root = root_management.get(&key);
            let local = self.management.get(&key);
            let managed = root.or(local);
            if let Some(managed) = managed {
                if dependency.coordinate.version.is_empty() {
                    if let Some(version) = &managed.version {
                        dependency.coordinate.version = version.clone();
                    }
                }
                if transitive {
                    if let Some(version) = root.and_then(|managed| managed.version.clone()) {
                        dependency.coordinate.version = version;
                    }
                }
                if let Some(scope) = &managed.scope {
                    if dependency.scope.is_empty() {
                        dependency.scope = scope.clone();
                    }
                }
                dependency.optional |= managed.optional;
                for exclusion in &managed.exclusions {
                    if !dependency.exclusions.contains(exclusion) {
                        dependency.exclusions.push(exclusion.clone());
                    }
                }
            }
            if dependency.scope.is_empty() {
                dependency.scope = "compile".to_owned();
            }
            result.push(dependency);
        }
        result
    }
}

fn activation_block_flag(activation: &str, flag: &str) -> bool {
    child_text(activation, flag).is_some_and(|value| value.trim() == "true")
}

fn profile_has_conditions(profile: &str) -> bool {
    let Some(activation) = direct_children(profile, "activation").into_iter().next() else {
        return false;
    };
    ["jdk", "os", "property", "file"]
        .iter()
        .any(|tag| activation.contains(&format!("<{tag}")))
}

fn parse_java_major(version: &str) -> Option<u32> {
    let numbers: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    match numbers.as_slice() {
        [] => None,
        [1, second, ..] => Some(*second),
        [first, ..] => Some(*first),
    }
}

/// Match Maven `jdk` activation ranges (`17`, `[17,)`, `[11,17)`, `1.8`).
fn jdk_range_matches(range: &str, major: u32) -> bool {
    let range = range.trim();
    if range.is_empty() {
        return false;
    }
    if !(range.starts_with('[') || range.starts_with('(')) {
        return parse_java_major(range) == Some(major);
    }
    let lower_inclusive = range.starts_with('[');
    let upper_inclusive = range.ends_with(']');
    let inner = range
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')']);
    let (lower, upper) = match inner.split_once(',') {
        Some((lower, upper)) => (lower.trim(), upper.trim()),
        None => (inner.trim(), ""),
    };
    if !lower.is_empty() {
        if let Some(low) = parse_java_major(lower) {
            if major < low || (major == low && !lower_inclusive) {
                return false;
            }
        }
    }
    if !upper.is_empty() {
        if let Some(high) = parse_java_major(upper) {
            if major > high || (major == high && !upper_inclusive) {
                return false;
            }
        }
    }
    true
}

fn os_matches(os: &str) -> bool {
    const OS: &str = std::env::consts::OS;
    const ARCH: &str = std::env::consts::ARCH;
    let family = if OS == "windows" { "windows" } else { "unix" };
    let mut ok = true;
    if let Some(name) = child_text(os, "name") {
        ok &= OS.contains(&name.to_lowercase());
    }
    if let Some(family_expected) = child_text(os, "family") {
        let expected = family_expected.to_lowercase();
        ok &= family == expected
            || (expected == "mac" && OS == "macos")
            || (expected == "unix" && (OS == "linux" || OS == "macos"));
    }
    if let Some(arch) = child_text(os, "arch") {
        let arch = arch.to_lowercase();
        let normalized = ARCH.to_lowercase();
        ok &= normalized.contains(&arch)
            || (arch.contains("amd64") && normalized == "x86_64")
            || (arch.contains("x86_64") && normalized == "amd64")
            || (arch.contains("aarch64") && normalized == "aarch64");
    }
    if let Some(version) = child_text(os, "version") {
        // Best-effort: only the OS family name is compared here.
        ok &= OS.contains(&version.to_lowercase());
    }
    ok
}

fn coordinate_from_block(block: &str, properties: &HashMap<String, String>) -> Coordinate {
    Coordinate {
        group: child_text(block, "groupId")
            .map(|value| substitute(&value, properties))
            .unwrap_or_default(),
        artifact: child_text(block, "artifactId")
            .map(|value| substitute(&value, properties))
            .unwrap_or_default(),
        version: child_text(block, "version")
            .map(|value| substitute(&value, properties))
            .unwrap_or_default(),
        classifier: child_text(block, "classifier")
            .map(|value| substitute(&value, properties))
            .filter(|value| !value.is_empty()),
    }
}

fn managed_from_block(
    block: &str,
    properties: &HashMap<String, String>,
) -> Option<(String, Managed)> {
    let coordinate = coordinate_from_block(block, properties);
    if coordinate.group.is_empty() || coordinate.artifact.is_empty() {
        return None;
    }
    Some((
        coordinate.key(),
        Managed {
            version: Some(coordinate.version).filter(|value| !value.is_empty()),
            scope: child_text(block, "scope").map(|value| substitute(&value, properties)),
            optional: child_text(block, "optional").is_some_and(|value| value == "true"),
            exclusions: parse_exclusions(block, properties),
        },
    ))
}

fn parse_exclusions(block: &str, properties: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut exclusions = Vec::new();
    for exclusions_block in direct_children(block, "exclusions") {
        for exclusion in direct_children(&exclusions_block, "exclusion") {
            let group = child_text(&exclusion, "groupId")
                .map(|value| substitute(&value, properties))
                .unwrap_or_default();
            let artifact = child_text(&exclusion, "artifactId")
                .map(|value| substitute(&value, properties))
                .unwrap_or_default();
            if !group.is_empty() && !artifact.is_empty() {
                exclusions.push((group, artifact));
            }
        }
    }
    exclusions
}

fn parent_matches(pom: &Pom, coordinate: &Coordinate) -> bool {
    pom.coordinate.group == coordinate.group && pom.coordinate.artifact == coordinate.artifact
}

fn insert_project_properties(
    properties: &mut HashMap<String, String>,
    group: &str,
    artifact: &str,
    version: &str,
) {
    properties
        .entry("project.groupId".to_owned())
        .or_insert_with(|| group.to_owned());
    properties
        .entry("project.artifactId".to_owned())
        .or_insert_with(|| artifact.to_owned());
    properties
        .entry("project.version".to_owned())
        .or_insert_with(|| version.to_owned());
    properties
        .entry("pom.groupId".to_owned())
        .or_insert_with(|| group.to_owned());
    properties
        .entry("pom.artifactId".to_owned())
        .or_insert_with(|| artifact.to_owned());
    properties
        .entry("pom.version".to_owned())
        .or_insert_with(|| version.to_owned());
}

fn parse_properties(content: &str) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    if let Some(block) = direct_children(content, "properties").into_iter().next() {
        for (name, value) in direct_elements(&block) {
            properties.insert(name.to_owned(), value.trim().to_owned());
        }
    }
    properties
}

fn substitute(value: &str, properties: &HashMap<String, String>) -> String {
    let mut result = value.to_owned();
    for _ in 0..10 {
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

fn locate_jar(repo: &Path, coordinate: &Coordinate) -> Option<PathBuf> {
    let base = repo
        .join(coordinate.group.replace('.', "/"))
        .join(&coordinate.artifact)
        .join(&coordinate.version);
    if let Some(classifier) = &coordinate.classifier {
        let path = base.join(format!(
            "{}-{}-{}.jar",
            coordinate.artifact, coordinate.version, classifier
        ));
        return path.is_file().then_some(path);
    }
    let primary = base.join(format!(
        "{}-{}.jar",
        coordinate.artifact, coordinate.version
    ));
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
            && !name.ends_with("-tests.jar")
            && fallback.is_none()
        {
            fallback = Some(path);
        }
    }
    fallback
}

fn project_inner(xml: &str) -> Option<&str> {
    let tags = scan_tags(xml);
    let mut depth = 0usize;
    let mut content_start = None;
    for tag in tags {
        if tag.closing {
            if tag.name == "project" && depth == 1 {
                let start = content_start?;
                return Some(&xml[start..tag.start]);
            }
            depth = depth.saturating_sub(1);
        } else {
            if tag.name == "project" && depth == 0 && !tag.self_closing {
                content_start = Some(tag.end);
            }
            if !tag.self_closing {
                depth += 1;
            }
        }
    }
    None
}

fn child_text(xml: &str, tag: &str) -> Option<String> {
    direct_children(xml, tag)
        .into_iter()
        .next()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn direct_children(xml: &str, tag: &str) -> Vec<String> {
    direct_elements(xml)
        .into_iter()
        .filter(|(name, _)| *name == tag)
        .map(|(_, value)| value.to_owned())
        .collect()
}

fn direct_elements(xml: &str) -> Vec<(&str, &str)> {
    let tags = scan_tags(xml);
    let mut depth = 0usize;
    let mut current: Option<(&str, usize)> = None;
    let mut elements = Vec::new();
    for tag in tags {
        if tag.closing {
            if let Some((name, start)) = current {
                if tag.name == name && depth == 1 {
                    elements.push((name, &xml[start..tag.start]));
                    current = None;
                }
            }
            depth = depth.saturating_sub(1);
        } else {
            if depth == 0 && current.is_none() {
                if tag.self_closing {
                    elements.push((tag.name, ""));
                } else {
                    current = Some((tag.name, tag.end));
                }
            }
            if !tag.self_closing {
                depth += 1;
            }
        }
    }
    elements
}

struct Tag<'a> {
    name: &'a str,
    start: usize,
    end: usize,
    closing: bool,
    self_closing: bool,
}

fn scan_tags(xml: &str) -> Vec<Tag<'_>> {
    let bytes = xml.as_bytes();
    let mut tags = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        if xml[index..].starts_with("<!--") {
            index = xml[index + 4..]
                .find("-->")
                .map(|offset| index + 4 + offset + 3)
                .unwrap_or(bytes.len());
            continue;
        }
        if xml[index..].starts_with("<![CDATA[") {
            index = xml[index + 9..]
                .find("]]>")
                .map(|offset| index + 9 + offset + 3)
                .unwrap_or(bytes.len());
            continue;
        }
        if xml[index..].starts_with("<?") || xml[index..].starts_with("<!") {
            index = xml[index..]
                .find('>')
                .map(|offset| index + offset + 1)
                .unwrap_or(bytes.len());
            continue;
        }
        let Some(offset) = xml[index..].find('>') else {
            break;
        };
        let end = index + offset;
        let raw = xml[index + 1..end].trim();
        let closing = raw.starts_with('/');
        let self_closing = raw.ends_with('/');
        let trimmed = raw.trim_start_matches('/').trim();
        let name_end = trimmed
            .find(|character: char| character.is_whitespace() || character == '/')
            .unwrap_or(trimmed.len());
        let name = &trimmed[..name_end];
        if !name.is_empty() {
            tags.push(Tag {
                name,
                start: index,
                end: end + 1,
                closing,
                self_closing,
            });
        }
        index = end + 1;
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_jar(repo: &Path, group: &str, artifact: &str, version: &str) {
        let dir = repo
            .join(group.replace('.', "/"))
            .join(artifact)
            .join(version);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{artifact}-{version}.jar")), b"jar").unwrap();
    }

    fn write_pom(repo: &Path, group: &str, artifact: &str, version: &str, body: &str) {
        let dir = repo
            .join(group.replace('.', "/"))
            .join(artifact)
            .join(version);
        fs::create_dir_all(&dir).unwrap();
        let xml = format!(
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
             <version>{version}</version>{body}</project>"
        );
        fs::write(dir.join(format!("{artifact}-{version}.pom")), xml).unwrap();
    }

    #[test]
    fn resolves_transitive_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("pom.xml");
        write_jar(&repo, "com.acme", "a", "1.0");
        write_jar(&repo, "com.acme", "b", "2.0");
        write_jar(&repo, "com.acme", "c", "3.0");
        write_pom(&repo, "com.acme", "c", "3.0", "");
        write_pom(
            &repo,
            "com.acme",
            "a",
            "1.0",
            "<dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>b</artifactId><version>2.0</version></dependency></dependencies>",
        );
        write_pom(
            &repo,
            "com.acme",
            "b",
            "2.0",
            "<dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>c</artifactId><version>3.0</version></dependency></dependencies>",
        );
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>local</groupId><artifactId>app</artifactId><version>1.0</version>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>a</artifactId><version>1.0</version></dependency></dependencies></project>",
        )
        .unwrap();

        let resolution = resolve_pom(&root, &repo);
        assert_eq!(resolution.jars.len(), 3);
        assert_eq!(resolution.dependencies.len(), 3);
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn applies_dependency_management_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("pom.xml");
        write_jar(&repo, "com.acme", "managed", "4.2");
        write_jar(&repo, "com.acme", "bom-dep", "5.1");
        write_pom(
            &repo,
            "com.acme",
            "bom",
            "1.0",
            "<packaging>pom</packaging><dependencyManagement><dependencies>\
             <dependency><groupId>com.acme</groupId><artifactId>managed</artifactId>\
             <version>4.2</version></dependency></dependencies></dependencyManagement>",
        );
        write_pom(&repo, "com.acme", "bom-dep", "5.1", "");
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>local</groupId><artifactId>app</artifactId><version>1.0</version>\
             <dependencyManagement><dependencies>\
             <dependency><groupId>com.acme</groupId><artifactId>bom</artifactId>\
             <version>1.0</version><type>pom</type><scope>import</scope></dependency>\
             </dependencies></dependencyManagement>\
             <dependencies>\
             <dependency><groupId>com.acme</groupId><artifactId>managed</artifactId></dependency>\
             <dependency><groupId>com.acme</groupId><artifactId>bom-dep</artifactId>\
             <version>5.1</version></dependency>\
             </dependencies></project>",
        )
        .unwrap();

        let resolution = resolve_pom(&root, &repo);
        assert_eq!(resolution.jars.len(), 2);
        assert!(resolution
            .dependencies
            .iter()
            .any(|dep| dep.contains("managed:4.2")));
        assert!(resolution
            .dependencies
            .iter()
            .any(|dep| dep.contains("bom-dep:5.1")));
    }

    #[test]
    fn skips_optional_transitive_scope_and_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("pom.xml");
        write_jar(&repo, "com.acme", "a", "1.0");
        write_jar(&repo, "com.acme", "optional", "1.0");
        write_jar(&repo, "com.acme", "excluded", "1.0");
        write_jar(&repo, "com.acme", "test-only", "1.0");
        write_pom(
            &repo,
            "com.acme",
            "a",
            "1.0",
            "<dependencies>\
             <dependency><groupId>com.acme</groupId><artifactId>optional</artifactId>\
             <version>1.0</version><optional>true</optional></dependency>\
             <dependency><groupId>com.acme</groupId><artifactId>excluded</artifactId>\
             <version>1.0</version></dependency>\
             <dependency><groupId>com.acme</groupId><artifactId>test-only</artifactId>\
             <version>1.0</version><scope>test</scope></dependency>\
             </dependencies>",
        );
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>local</groupId><artifactId>app</artifactId><version>1.0</version>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>a</artifactId><version>1.0</version>\
             <exclusions><exclusion><groupId>com.acme</groupId>\
             <artifactId>excluded</artifactId></exclusion></exclusions>\
             </dependency></dependencies></project>",
        )
        .unwrap();

        let resolution = resolve_pom(&root, &repo);
        assert_eq!(resolution.dependencies, vec!["com.acme:a:1.0".to_owned()]);
        assert_eq!(resolution.jars.len(), 1);
    }

    #[test]
    fn resolves_gradle_platform_management() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        write_jar(&repo, "com.acme", "dep", "1.0");
        write_pom(&repo, "com.acme", "dep", "1.0", "");
        write_pom(
            &repo,
            "com.acme",
            "bom",
            "2.0",
            "<packaging>pom</packaging><dependencyManagement><dependencies>\
             <dependency><groupId>com.acme</groupId><artifactId>dep</artifactId>\
             <version>1.0</version></dependency></dependencies></dependencyManagement>",
        );
        let coordinates = vec![Coordinate {
            group: "com.acme".to_owned(),
            artifact: "dep".to_owned(),
            version: String::new(),
            classifier: None,
        }];
        let boms = vec![Coordinate {
            group: "com.acme".to_owned(),
            artifact: "bom".to_owned(),
            version: "2.0".to_owned(),
            classifier: None,
        }];
        let resolution = resolve_gradle(&repo, &coordinates, &boms);
        assert_eq!(resolution.dependencies, vec!["com.acme:dep:1.0".to_owned()]);
        assert_eq!(resolution.jars.len(), 1);
    }

    #[test]
    fn resolves_parent_properties_and_management() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("child/pom.xml");
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        write_jar(&repo, "com.acme", "dep", "7.0");
        write_pom(
            &repo,
            "com.acme",
            "parent",
            "1.0",
            "<packaging>pom</packaging><properties><dep.version>7.0</dep.version></properties>\
             <dependencyManagement><dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>dep</artifactId><version>${dep.version}</version></dependency>\
             </dependencies></dependencyManagement>",
        );
        write_pom(&repo, "com.acme", "dep", "7.0", "");
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <parent><groupId>com.acme</groupId><artifactId>parent</artifactId>\
             <version>1.0</version><relativePath/></parent>\
             <artifactId>app</artifactId>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>dep</artifactId></dependency></dependencies></project>",
        )
        .unwrap();

        let resolution = resolve_pom(&root, &repo);
        assert_eq!(resolution.dependencies, vec!["com.acme:dep:7.0".to_owned()]);
        assert_eq!(resolution.jars.len(), 1);
    }

    #[test]
    fn applies_active_by_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("pom.xml");
        write_jar(&repo, "com.acme", "prof-dep", "1.0");
        write_pom(&repo, "com.acme", "prof-dep", "1.0", "");
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>local</groupId><artifactId>app</artifactId><version>1.0</version>\
             <profiles><profile><id>extra</id>\
             <activation><activeByDefault>true</activeByDefault></activation>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>prof-dep</artifactId><version>1.0</version></dependency></dependencies>\
             </profile></profiles></project>",
        )
        .unwrap();

        let resolution = resolve_pom(&root, &repo);
        assert_eq!(
            resolution.dependencies,
            vec!["com.acme:prof-dep:1.0".to_owned()]
        );
    }

    #[test]
    fn applies_jdk_and_property_profile_activation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let root = dir.path().join("pom.xml");
        write_jar(&repo, "com.acme", "jdk-dep", "2.0");
        write_jar(&repo, "com.acme", "prop-dep", "3.0");
        write_pom(&repo, "com.acme", "jdk-dep", "2.0", "");
        write_pom(&repo, "com.acme", "prop-dep", "3.0", "");
        fs::write(
            &root,
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>local</groupId><artifactId>app</artifactId><version>1.0</version>\
             <properties><db>postgres</db></properties>\
             <profiles>\
             <profile><id>on-jdk</id><activation><jdk>[17,)</jdk></activation>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>jdk-dep</artifactId><version>2.0</version></dependency></dependencies>\
             </profile>\
             <profile><id>on-prop</id>\
             <activation><property><name>db</name><value>postgres</value></property></activation>\
             <properties><prop.version>3.0</prop.version></properties>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>prop-dep</artifactId><version>${prop.version}</version></dependency></dependencies>\
             </profile>\
             <profile><id>never</id><activation><jdk>[99,)</jdk></activation>\
             <dependencies><dependency><groupId>com.acme</groupId>\
             <artifactId>nope</artifactId><version>9.9</version></dependency></dependencies>\
             </profile>\
             </profiles></project>",
        )
        .unwrap();

        std::env::set_var("RECLIPSE_JDK_VERSION", "21");
        let resolution = resolve_pom(&root, &repo);
        std::env::remove_var("RECLIPSE_JDK_VERSION");
        assert!(resolution
            .dependencies
            .iter()
            .any(|dep| dep == "com.acme:jdk-dep:2.0"));
        assert!(resolution
            .dependencies
            .iter()
            .any(|dep| dep == "com.acme:prop-dep:3.0"));
        assert!(!resolution
            .dependencies
            .iter()
            .any(|dep| dep.contains("nope")));
    }
}
