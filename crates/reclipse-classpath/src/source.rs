//! Locate and read Java sources attached to classpath entries.
//!
//! Sources are optional: the index only stores metadata, and this module maps
//! a class back to its `-sources.jar` or the JDK `src.zip` on demand.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zip::ZipArchive;

use crate::classfile::ClassInfo;

#[derive(Debug, Clone)]
pub struct SourceLocation {
    pub archive: PathBuf,
    pub entry: String,
}

pub fn source_location(class: &ClassInfo) -> Option<SourceLocation> {
    let origin = Path::new(&class.origin);
    let extension = origin
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let entry = source_entry(&class.name);
    match extension.as_str() {
        "jar" => {
            if origin
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("rt.jar"))
            {
                let archive = rt_sources_zip(origin)?;
                return Some(SourceLocation { archive, entry });
            }
            let archive = sources_jar(origin)?;
            archive
                .is_file()
                .then_some(SourceLocation { archive, entry })
        }
        "jmod" => {
            let home = origin.parent()?.parent()?;
            let archive = home.join("lib").join("src.zip");
            let module = origin.file_stem()?.to_string_lossy();
            if !archive.is_file() {
                return None;
            }
            Some(SourceLocation {
                archive,
                entry: format!("{module}/{entry}"),
            })
        }
        _ => None,
    }
}

pub fn read_source(location: &SourceLocation) -> Result<String> {
    let file = std::fs::File::open(&location.archive)
        .with_context(|| format!("abrir {}", location.archive.display()))?;
    let mut archive = ZipArchive::new(file)?;
    if let Ok(mut entry) = archive.by_name(&location.entry) {
        let mut source = String::new();
        entry.read_to_string(&mut source)?;
        return Ok(source);
    }
    // Some JDK `src.zip` layouts omit the module prefix.
    if location
        .archive
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("src.zip"))
    {
        if let Some((_, fallback)) = location.entry.split_once('/') {
            let mut entry = archive.by_name(fallback)?;
            let mut source = String::new();
            entry.read_to_string(&mut source)?;
            return Ok(source);
        }
    }
    anyhow::bail!(
        "fonte não encontrada em {} ({})",
        location.archive.display(),
        location.entry
    )
}

fn sources_jar(origin: &Path) -> Option<PathBuf> {
    let file_name = origin.file_name()?.to_string_lossy();
    if file_name.ends_with("-sources.jar") {
        return Some(origin.to_path_buf());
    }
    let stem = file_name.strip_suffix(".jar")?;
    Some(origin.with_file_name(format!("{stem}-sources.jar")))
}

fn rt_sources_zip(origin: &Path) -> Option<PathBuf> {
    let lib = origin.parent()?;
    let jre = lib.parent()?;
    let home = jre.parent()?;
    [
        lib.join("src.zip"),
        jre.join("src.zip"),
        home.join("src.zip"),
        home.join("lib/src.zip"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn source_entry(class_name: &str) -> String {
    let top_level = class_name.split('$').next().unwrap_or(class_name);
    format!("{}.java", top_level.replace('.', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classfile::TypeKind;
    use std::fs;

    fn class(name: &str, origin: &str) -> ClassInfo {
        ClassInfo {
            name: name.to_owned(),
            display_name: name.rsplit('.').next().unwrap_or(name).to_owned(),
            package: name
                .rsplit_once('.')
                .map(|(package, _)| package.to_owned())
                .unwrap_or_default(),
            kind: TypeKind::Class,
            is_public: true,
            super_name: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            origin: origin.to_owned(),
        }
    }

    #[test]
    fn maps_nested_types_to_outer_source_file() {
        assert_eq!(source_entry("java.util.Map$Entry"), "java/util/Map.java");
        assert_eq!(source_entry("com.acme.User"), "com/acme/User.java");
    }

    #[test]
    fn finds_jdk_sources_zip() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join("lib")).unwrap();
        fs::create_dir_all(home.join("jmods")).unwrap();
        fs::write(home.join("lib/src.zip"), b"zip").unwrap();
        let jmod = home.join("jmods/java.base.jmod");
        fs::write(&jmod, b"jmod").unwrap();
        let location =
            source_location(&class("java.lang.String", &jmod.display().to_string())).unwrap();
        assert!(location.archive.ends_with("lib/src.zip"));
        assert_eq!(location.entry, "java.base/java/lang/String.java");
    }

    #[test]
    fn finds_jdk8_sources_zip_for_rt_jar() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join("jre/lib")).unwrap();
        fs::write(home.join("src.zip"), b"zip").unwrap();
        let rt = home.join("jre/lib/rt.jar");
        fs::write(&rt, b"jar").unwrap();
        let location =
            source_location(&class("java.lang.String", &rt.display().to_string())).unwrap();
        assert!(location.archive.ends_with("src.zip"));
        assert_eq!(location.entry, "java/lang/String.java");
    }

    #[test]
    fn finds_sources_jar_next_to_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("app-1.2.3.jar");
        let sources = dir.path().join("app-1.2.3-sources.jar");
        fs::write(&binary, b"jar").unwrap();
        fs::write(&sources, b"zip").unwrap();
        let location =
            source_location(&class("com.acme.App", &binary.display().to_string())).unwrap();
        assert_eq!(location.archive, sources);
        assert_eq!(location.entry, "com/acme/App.java");
    }
}
