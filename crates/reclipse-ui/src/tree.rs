//! Lazy, cached project tree.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use egui::{RichText, Ui};
use reclipse_core::IGNORED_DIRS;

use crate::theme;

#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

#[derive(Default)]
pub struct FileTree {
    children: HashMap<PathBuf, Vec<Entry>>,
}

#[derive(Debug, Clone)]
pub enum TreeEvent {
    Open(PathBuf),
    ConfigureBuildPath(PathBuf),
}

fn build_path_menu(ui: &mut Ui, path: &Path, events: &mut Vec<TreeEvent>) {
    ui.menu_button("Build Path", |ui| {
        if ui.button("Configure Build Path…").clicked() {
            events.push(TreeEvent::ConfigureBuildPath(path.to_path_buf()));
            ui.close_menu();
        }
    });
}

impl FileTree {
    pub fn show(&mut self, ui: &mut Ui, root: &Path) -> Vec<TreeEvent> {
        let mut events = Vec::new();
        self.show_dir(ui, root, 0, &mut events);
        events
    }

    pub fn invalidate(&mut self) {
        self.children.clear();
    }

    fn show_dir(&mut self, ui: &mut Ui, dir: &Path, depth: usize, events: &mut Vec<TreeEvent>) {
        if depth > 32 {
            return;
        }
        for entry in self.entries(dir) {
            if entry.is_dir {
                let header = RichText::new(&entry.name).color(theme::text());
                let response = egui::CollapsingHeader::new(header)
                    .id_salt(("reclipse_tree_dir", &entry.path))
                    .default_open(depth < 5)
                    .show(ui, |ui| {
                        self.show_dir(ui, &entry.path, depth + 1, events);
                    });
                response.header_response.context_menu(|ui| {
                    build_path_menu(ui, &entry.path, events);
                });
            } else {
                let is_java = entry
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| extension.eq_ignore_ascii_case("java"))
                    .unwrap_or(false);
                let color = if is_java {
                    theme::text()
                } else {
                    theme::muted()
                };
                let label = format!("  {}", entry.name);
                let response = ui.selectable_label(false, RichText::new(label).color(color));
                if response.clicked() {
                    events.push(TreeEvent::Open(entry.path.clone()));
                }
                response.context_menu(|ui| {
                    build_path_menu(ui, &entry.path, events);
                });
            }
        }
    }

    fn entries(&mut self, dir: &Path) -> Vec<Entry> {
        self.children
            .entry(dir.to_path_buf())
            .or_insert_with(|| read_entries(dir))
            .clone()
    }
}

fn read_entries(dir: &Path) -> Vec<Entry> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir()
                && IGNORED_DIRS
                    .iter()
                    .any(|ignored| name.eq_ignore_ascii_case(ignored))
            {
                return None;
            }
            Some(Entry {
                name,
                path: entry.path(),
                is_dir: file_type.is_dir(),
            })
        })
        .collect();

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}
