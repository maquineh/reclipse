//! Index of classes and public members found in JARs/JMODs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Instant;

use zip::ZipArchive;

use crate::classfile::{parse_class, ClassInfo, MemberInfo};
use crate::resolve::ClasspathResolution;

#[derive(Debug, Clone, Default)]
pub struct ClasspathStats {
    pub archives: usize,
    pub classes: usize,
    pub elapsed_ms: u128,
    pub jdk_target: Option<u32>,
    pub jdk_version: Option<String>,
}

#[derive(Debug, Default)]
pub struct ClasspathIndex {
    pub classes: BTreeMap<String, ClassInfo>,
    pub by_simple: HashMap<String, Vec<String>>,
    pub packages: HashSet<String>,
    pub type_names: Vec<String>,
    pub stats: ClasspathStats,
    pub warnings: Vec<String>,
}

impl ClasspathIndex {
    pub fn get(&self, name: &str) -> Option<&ClassInfo> {
        self.classes.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.classes.contains_key(name)
    }

    pub fn package_exists(&self, package: &str) -> bool {
        self.packages.contains(package)
    }

    /// Resolve nested class names written with dots (`Map.Entry`) against the
    /// `$`-separated names stored in class files.
    pub fn get_loose(&self, name: &str) -> Option<&ClassInfo> {
        if let Some(class) = self.classes.get(name) {
            return Some(class);
        }
        let mut candidate = name.to_owned();
        while let Some(position) = candidate.rfind('.') {
            candidate.replace_range(position..position + 1, "$");
            if let Some(class) = self.classes.get(&candidate) {
                return Some(class);
            }
        }
        None
    }

    pub fn simple_matches(&self, simple: &str) -> &[String] {
        self.by_simple.get(simple).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn members_named(&self, owner: &str, name: &str) -> Vec<&MemberInfo> {
        let Some(class) = self.classes.get(owner) else {
            return Vec::new();
        };
        class
            .fields
            .iter()
            .chain(class.methods.iter())
            .filter(|member| member.name == name)
            .collect()
    }

    pub fn member_names(&self, owner: &str) -> Vec<String> {
        let Some(class) = self.classes.get(owner) else {
            return Vec::new();
        };
        class
            .fields
            .iter()
            .chain(class.methods.iter())
            .map(|member| member.name.clone())
            .collect()
    }

    pub fn source_location(&self, name: &str) -> Option<crate::source::SourceLocation> {
        let class = self.get_loose(name)?;
        crate::source::source_location(class)
    }
}

pub fn build(
    resolution: &ClasspathResolution,
    progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> ClasspathIndex {
    let start = Instant::now();
    let mut index = ClasspathIndex::default();
    index.warnings.extend(resolution.warnings.iter().cloned());
    index.stats.jdk_target = resolution.jdk_target;
    index.stats.jdk_version = resolution.jdk.as_ref().map(|jdk| jdk.version.clone());

    let mut archives: Vec<(std::path::PathBuf, bool)> = resolution
        .jars
        .iter()
        .cloned()
        .map(|path| (path, false))
        .collect();
    if let Some(jdk) = &resolution.jdk {
        if let Some(rt_jar) = &jdk.rt_jar {
            archives.push((rt_jar.clone(), false));
        }
        for jmod in &jdk.jmods {
            archives.push((jmod.clone(), true));
        }
    }

    let total = archives.len();
    for (position, (path, is_jmod)) in archives.into_iter().enumerate() {
        let origin = path.display().to_string();
        let result = if is_jmod {
            index_jmod(&path, &origin, &mut index)
        } else {
            index_jar(&path, &origin, &mut index)
        };
        if let Err(error) = result {
            push_warning(&mut index, format!("{}: {error:#}", path.display()));
        }
        index.stats.archives += 1;
        if let Some(progress) = progress {
            progress(position + 1, total);
        }
    }

    index.type_names = index
        .classes
        .values()
        .filter(|class| class.is_public)
        .map(|class| class.name.clone())
        .collect();
    index.type_names.sort();
    index.type_names.dedup();
    for class in index.classes.values() {
        index.packages.insert(class.package.clone());
        if class.is_public {
            index
                .by_simple
                .entry(class.simple_name().to_owned())
                .or_default()
                .push(class.name.clone());
        }
    }
    for names in index.by_simple.values_mut() {
        names.sort();
        names.dedup();
    }
    index.stats.classes = index.classes.len();
    index.stats.elapsed_ms = start.elapsed().as_millis();
    index
}

fn index_jar(path: &Path, origin: &str, index: &mut ClasspathIndex) -> anyhow::Result<()> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    for entry_index in 0..archive.len() {
        let mut entry = archive.by_index(entry_index)?;
        let name = entry.name().to_owned();
        if !name.ends_with(".class")
            || name.starts_with("META-INF/")
            || name.ends_with("module-info.class")
            || name.ends_with("package-info.class")
        {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        insert_class(&bytes, origin, index);
    }
    Ok(())
}

fn index_jmod(path: &Path, origin: &str, index: &mut ClasspathIndex) -> anyhow::Result<()> {
    let bytes = std::fs::read(path)?;
    let payload = if bytes.starts_with(b"JM\x01\x00") {
        &bytes[4..]
    } else {
        &bytes
    };
    let mut archive = ZipArchive::new(Cursor::new(payload))?;
    for entry_index in 0..archive.len() {
        let mut entry = archive.by_index(entry_index)?;
        let name = entry.name().to_owned();
        if !name.ends_with(".class")
            || !name.starts_with("classes/")
            || name.ends_with("module-info.class")
            || name.ends_with("package-info.class")
        {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        insert_class(&data, origin, index);
    }
    Ok(())
}

fn insert_class(bytes: &[u8], origin: &str, index: &mut ClasspathIndex) {
    let Ok(class) = parse_class(bytes, origin) else {
        return;
    };
    match index.classes.get(&class.name) {
        Some(existing) if existing.is_public || !class.is_public => return,
        _ => {}
    }
    index.classes.insert(class.name.clone(), class);
}

fn push_warning(index: &mut ClasspathIndex, warning: String) {
    eprintln!("[reclipse] classpath: {warning}");
    if index.warnings.len() < 200 {
        index.warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_classpath_index_is_valid() {
        let index = build(
            &ClasspathResolution {
                sources: Vec::new(),
                jars: Vec::new(),
                jdk: None,
                jdk_target: None,
                warnings: Vec::new(),
                build_system: reclipse_core::BuildSystem::Plain,
            },
            None,
        );
        assert_eq!(index.stats.classes, 0);
        assert!(index.type_names.is_empty());
    }

    #[test]
    fn indexes_jdk_string_when_available() {
        let Some(jdk) = crate::resolve::find_jdk() else {
            return;
        };
        let index = build(
            &ClasspathResolution {
                sources: Vec::new(),
                jars: Vec::new(),
                jdk: Some(jdk),
                jdk_target: None,
                warnings: Vec::new(),
                build_system: reclipse_core::BuildSystem::Plain,
            },
            None,
        );
        assert!(index.get("java.lang.String").is_some());
        assert!(index
            .type_names
            .iter()
            .any(|name| name == "java.lang.String"));
    }
}
