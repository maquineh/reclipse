//! Persistent session and UI preferences.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Keybindings {
    pub quick_open: String,
    pub open_type: String,
    pub quick_outline: String,
    pub command_palette: String,
    pub open_file: String,
    pub open_folder: String,
    pub save: String,
    pub close_tab: String,
    pub find: String,
    pub replace: String,
    pub project_search: String,
    pub go_to_definition: String,
    pub reindex: String,
    pub find_next: String,
    pub find_previous: String,
    pub shortcut_help: String,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            // Eclipse-style defaults.
            quick_open: "Ctrl+Shift+R".to_owned(),
            open_type: "Ctrl+Shift+T".to_owned(),
            quick_outline: "Ctrl+O".to_owned(),
            command_palette: "Ctrl+3".to_owned(),
            open_file: "Ctrl+Alt+O".to_owned(),
            open_folder: "Ctrl+Shift+O".to_owned(),
            save: "Ctrl+S".to_owned(),
            close_tab: "Ctrl+W".to_owned(),
            find: "Ctrl+F".to_owned(),
            replace: "Ctrl+H".to_owned(),
            project_search: "Ctrl+Shift+F".to_owned(),
            go_to_definition: "F3".to_owned(),
            reindex: "F5".to_owned(),
            find_next: "Ctrl+K".to_owned(),
            find_previous: "Ctrl+Shift+K".to_owned(),
            shortcut_help: "Ctrl+Shift+L".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub root: Option<PathBuf>,
    pub tabs: Vec<TabSession>,
    pub active: Option<PathBuf>,
    pub show_project: bool,
    pub show_outline: bool,
    pub show_problems: bool,
    pub show_console: bool,
    pub console_history: Vec<String>,
    pub font_size: f32,
    pub theme: String,
    pub launch: crate::build::LaunchConfig,
    pub keybindings: Keybindings,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            root: None,
            tabs: Vec::new(),
            active: None,
            show_project: true,
            show_outline: true,
            show_problems: true,
            show_console: true,
            console_history: Vec::new(),
            font_size: 14.5,
            theme: "rust-oxide".to_owned(),
            launch: crate::build::LaunchConfig::default(),
            keybindings: Keybindings::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabSession {
    pub path: PathBuf,
    pub cursor: usize,
    pub anchor: usize,
    #[serde(default)]
    pub folded: Vec<(usize, usize)>,
}

pub fn load() -> Session {
    let Some(path) = config_path() else {
        return Session::default();
    };
    let Ok(bytes) = fs::read(&path) else {
        return Session::default();
    };
    // Notepad and Windows PowerShell may write a UTF-8 BOM.
    parse_session(&bytes).unwrap_or_else(|error| {
        eprintln!(
            "[reclipse] ignoring invalid session file {}: {error}",
            path.display()
        );
        Session::default()
    })
}

fn parse_session(bytes: &[u8]) -> serde_json::Result<Session> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    serde_json::from_slice(bytes)
}

pub fn save(session: &Session) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(session)?;
    fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Read only the keybindings from disk. Used when saving so external edits made
/// while the IDE was running are not overwritten.
pub fn load_keybindings() -> Option<Keybindings> {
    let path = config_path()?;
    let bytes = fs::read(path).ok()?;
    parse_session(&bytes)
        .ok()
        .map(|session| session.keybindings)
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata).join("Reclipse").join("session.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("reclipse")
                .join("session.json"),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_utf8_bom() {
        let json = serde_json::to_vec(&Session::default()).unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(json);
        let session = parse_session(&bytes).unwrap();
        assert!(session.show_project);
        assert_eq!(session.keybindings.quick_open, "Ctrl+Shift+R");
    }
}
