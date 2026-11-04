//! Crash recovery backups for dirty editor buffers.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reclipse_editor::Document;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryMeta {
    pub path: PathBuf,
    pub cursor: usize,
    pub anchor: usize,
    pub saved_at: u64,
    #[serde(default)]
    pub size: usize,
}

#[derive(Debug, Clone)]
pub struct RecoveryEntry {
    pub meta: RecoveryMeta,
    pub content_path: PathBuf,
    pub meta_path: PathBuf,
}

impl RecoveryEntry {
    pub fn content(&self) -> Option<String> {
        fs::read_to_string(&self.content_path).ok()
    }
}

pub fn backup_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RECLIPSE_RECOVERY_DIR") {
        return Some(PathBuf::from(path));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata).join("Reclipse").join("backups"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("reclipse")
                .join("backups"),
        );
    }
    None
}

pub fn write_backup(doc: &Document) -> Result<()> {
    let Some(dir) = backup_dir() else {
        return Ok(());
    };
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let key = key_for(&doc.path);
    let content_path = dir.join(format!("{key}.txt"));
    let meta_path = dir.join(format!("{key}.json"));
    let content = doc.rope.to_string();
    fs::write(&content_path, &content)
        .with_context(|| format!("failed to write {}", content_path.display()))?;
    let meta = RecoveryMeta {
        path: doc.path.clone(),
        cursor: doc.cursor_char,
        anchor: doc.anchor_char,
        saved_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        size: content.len(),
    };
    fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;
    Ok(())
}

pub fn list() -> Vec<RecoveryEntry> {
    let Some(dir) = backup_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut recovery = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<RecoveryMeta>(&bytes) else {
            continue;
        };
        let content_path = path.with_extension("txt");
        if content_path.is_file() {
            recovery.push(RecoveryEntry {
                meta,
                content_path,
                meta_path: path,
            });
        }
    }
    recovery.sort_by(|left, right| {
        right
            .meta
            .saved_at
            .cmp(&left.meta.saved_at)
            .then_with(|| left.meta.path.cmp(&right.meta.path))
    });
    recovery
}

pub fn discard(path: &Path) -> Result<()> {
    let Some(dir) = backup_dir() else {
        return Ok(());
    };
    let key = key_for(path);
    for extension in ["json", "txt"] {
        let target = dir.join(format!("{key}.{extension}"));
        if target.exists() {
            fs::remove_file(&target)
                .with_context(|| format!("failed to remove {}", target.display()))?;
        }
    }
    Ok(())
}

pub fn discard_entry(entry: &RecoveryEntry) -> Result<()> {
    for path in [&entry.meta_path, &entry.content_path] {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn key_for(path: &Path) -> String {
    // FNV-1a is enough for a local filename; the full path lives in the JSON.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn writes_lists_and_discards_backup() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("RECLIPSE_RECOVERY_DIR", dir.path());
        let mut doc = Document::open(recovery_fixture(dir.path())).unwrap();
        doc.set_cursor(0, false);
        doc.insert("dirty");
        write_backup(&doc).unwrap();

        let entries = list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meta.path, doc.path);
        assert_eq!(entries[0].content().unwrap(), doc.text());

        discard(&doc.path).unwrap();
        assert!(list().is_empty());
        std::env::remove_var("RECLIPSE_RECOVERY_DIR");
    }

    fn recovery_fixture(dir: &Path) -> PathBuf {
        let path = dir.join("Test.java");
        fs::write(&path, "class Test {}").unwrap();
        path
    }
}
