//! Application state and egui composition.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use eframe::CreationContext;
use egui::{Align, Color32, Context, Key, Layout, Modifiers, RichText, Ui};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use reclipse_classpath::{ClasspathIndex, ClasspathStats};
use reclipse_core::{discover_project, Project, IGNORED_DIRS};
use reclipse_editor::{Document, SearchMatcher, SearchOptions};
use reclipse_java::semantic::{
    resolve_type as resolve_semantic_type, unresolved_imports, ProjectTypes, ResolvedType,
};
use reclipse_java::{build_index, IndexedProblem, ProjectIndex, SymbolKind};
use reclipse_plugin_api::InvokeContext;
use reclipse_plugin_host::{PluginCommand, PluginHost};

use crate::editor_view::Tab;
use crate::keymap::{KeyAction, Keymap};
use crate::palette::{rank, Action, Palette, PaletteEntry, PaletteMode};
use crate::search::{project_files, FileSearch, ProjectSearch};
use crate::session::Keybindings;
use crate::theme;
use crate::tree::FileTree;

enum IndexMessage {
    Progress { done: usize, total: usize },
    Done(Box<IndexPayload>),
    Failed(String),
}

enum ClasspathMessage {
    Progress { done: usize, total: usize },
    Ready(Box<ClasspathIndex>),
    Failed(String),
}

#[derive(Debug)]
enum ClasspathStatus {
    Idle,
    Indexing { done: usize, total: usize },
    Ready(ClasspathStats),
    Failed(String),
}

#[derive(Debug, Clone)]
struct SemanticProblem {
    file: PathBuf,
    line: usize,
    message: String,
}

struct IndexPayload {
    project: Project,
    index: ProjectIndex,
}

#[derive(Debug)]
enum IndexStatus {
    Idle,
    Indexing {
        done: usize,
        total: usize,
    },
    Ready {
        files: usize,
        symbols: usize,
        problems: usize,
        elapsed_ms: u128,
    },
    Failed(String),
}

struct Toast {
    text: String,
    until: Instant,
}

#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
struct ScreenshotState {
    path: Option<PathBuf>,
    requested: bool,
    ready_frames: u32,
    capture_after_frames: u32,
    open_palette: bool,
    open_type: bool,
    palette_query: Option<String>,
    classpath_view: Option<String>,
    search_query: Option<String>,
    project_search_query: Option<String>,
}

struct FrameBench {
    target: usize,
    warmup: usize,
    frames: usize,
    samples: Vec<f64>,
    wall_samples: Vec<f64>,
    last_end: Option<Instant>,
    cursor: usize,
    started: bool,
}

pub struct ReclipseApp {
    root: Option<PathBuf>,
    project: Option<Project>,
    index: Option<ProjectIndex>,
    index_status: IndexStatus,
    index_rx: Option<Receiver<IndexMessage>>,
    classpath_index: Option<ClasspathIndex>,
    classpath_status: ClasspathStatus,
    classpath_rx: Option<Receiver<ClasspathMessage>>,
    project_types: Option<ProjectTypes>,
    semantic_problems: Vec<SemanticProblem>,
    classpath_view: Option<String>,
    egui_ctx: Context,
    watcher: Option<RecommendedWatcher>,
    watch_rx: Option<Receiver<PathBuf>>,
    watch_ignored: HashMap<PathBuf, Instant>,
    tree: FileTree,
    tabs: Vec<Tab>,
    active_tab: Option<usize>,
    palette: Palette,
    keymap: Keymap,
    keybindings: Keybindings,
    file_search: FileSearch,
    project_search: ProjectSearch,
    show_project: bool,
    show_outline: bool,
    show_problems: bool,
    plugins: PluginHost,
    plugin_commands: Vec<PluginCommand>,
    status_message: Option<Toast>,
    about_open: bool,
    shortcuts_open: bool,
    project_replace_confirm: bool,
    close_confirm: Option<usize>,
    font_size: f32,
    recovery_entries: Vec<crate::recovery::RecoveryEntry>,
    recovery_open: bool,
    backup_state: HashMap<PathBuf, (u64, Instant)>,
    frame_bench: Option<FrameBench>,
    screenshot: ScreenshotState,
}

impl ReclipseApp {
    pub fn new(cc: &CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        theme::configure(&cc.egui_ctx);

        let mut plugins = PluginHost::new();
        plugins.load_default_paths();
        let plugin_commands = plugins.commands();
        let session = crate::session::load();
        let recovery_entries = crate::recovery::list();
        let recovery_open = !recovery_entries.is_empty();

        let mut app = Self {
            root: None,
            project: None,
            index: None,
            index_status: IndexStatus::Idle,
            index_rx: None,
            classpath_index: None,
            classpath_status: ClasspathStatus::Idle,
            classpath_rx: None,
            project_types: None,
            semantic_problems: Vec::new(),
            classpath_view: None,
            egui_ctx: cc.egui_ctx.clone(),
            watcher: None,
            watch_rx: None,
            watch_ignored: HashMap::new(),
            tree: FileTree::default(),
            tabs: Vec::new(),
            active_tab: None,
            palette: Palette::default(),
            keymap: Keymap::from_config(&session.keybindings),
            keybindings: session.keybindings.clone(),
            file_search: FileSearch::default(),
            project_search: ProjectSearch::default(),
            show_project: session.show_project,
            show_outline: session.show_outline,
            show_problems: session.show_problems,
            plugins,
            plugin_commands,
            status_message: None,
            about_open: false,
            shortcuts_open: false,
            project_replace_confirm: false,
            close_confirm: None,
            font_size: session.font_size.clamp(10.0, 28.0),
            recovery_entries,
            recovery_open,
            backup_state: HashMap::new(),
            frame_bench: {
                #[cfg(feature = "frame-bench")]
                {
                    std::env::var("RECLIPSE_FRAME_BENCH_FRAMES")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok())
                        .map(|target| FrameBench {
                            target,
                            warmup: 30,
                            frames: 0,
                            samples: Vec::with_capacity(target),
                            wall_samples: Vec::with_capacity(target),
                            last_end: None,
                            cursor: 0,
                            started: false,
                        })
                }
                #[cfg(not(feature = "frame-bench"))]
                {
                    None
                }
            },
            screenshot: ScreenshotState {
                path: std::env::var_os("RECLIPSE_SCREENSHOT_TO").map(PathBuf::from),
                requested: false,
                ready_frames: 0,
                capture_after_frames: std::env::var("RECLIPSE_SCREENSHOT_FRAMES")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(20),
                open_palette: std::env::var_os("RECLIPSE_SCREENSHOT_PALETTE").is_some(),
                open_type: std::env::var_os("RECLIPSE_SCREENSHOT_OPEN_TYPE").is_some(),
                palette_query: std::env::var("RECLIPSE_SCREENSHOT_PALETTE_QUERY").ok(),
                classpath_view: std::env::var("RECLIPSE_SCREENSHOT_CLASSPATH_VIEW").ok(),
                search_query: std::env::var("RECLIPSE_SCREENSHOT_SEARCH").ok(),
                project_search_query: std::env::var("RECLIPSE_SCREENSHOT_PROJECT_SEARCH").ok(),
            },
        };

        if let Some(path) = initial {
            app.open_initial(path);
        } else {
            app.restore_session(&session);
        }
        app
    }

    fn restore_session(&mut self, session: &crate::session::Session) {
        if let Some(root) = &session.root {
            if root.is_dir() {
                self.start_project(root.clone());
            }
        }

        for saved in session.tabs.iter().take(100) {
            if !saved.path.is_file() {
                continue;
            }
            let path = dunce::canonicalize(&saved.path).unwrap_or_else(|_| saved.path.clone());
            if !self.tabs.iter().any(|tab| tab.doc.path == path) {
                self.open_file(path.clone());
            }
            if let Some(index) = self.tabs.iter().position(|tab| tab.doc.path == path) {
                let tab = &mut self.tabs[index];
                tab.doc.set_selection(saved.anchor, saved.cursor);
                tab.pending_cursor = Some(saved.cursor);
                tab.restore_folds(&saved.folded);
            }
        }

        if let Some(active) = &session.active {
            let path = dunce::canonicalize(active).unwrap_or_else(|_| active.clone());
            if let Some(index) = self.tabs.iter().position(|tab| tab.doc.path == path) {
                self.active_tab = Some(index);
            }
        }
        if self.active_tab.is_none() && !self.tabs.is_empty() {
            self.active_tab = Some(0);
        }
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.focus();
            }
        }
    }

    fn open_initial(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.start_project(path);
        } else if path.is_file() {
            if let Some(parent) = path.parent() {
                self.start_project(parent.to_path_buf());
            }
            self.open_file(path);
        } else {
            self.toast(format!("Caminho não encontrado: {}", path.display()));
        }
    }

    fn start_project(&mut self, root: PathBuf) {
        let root = dunce::canonicalize(&root).unwrap_or(root);
        self.root = Some(root.clone());
        self.project = None;
        self.index = None;
        self.classpath_index = None;
        self.classpath_status = ClasspathStatus::Idle;
        self.classpath_rx = None;
        self.project_types = None;
        self.semantic_problems.clear();
        self.classpath_view = None;
        self.tabs.clear();
        self.active_tab = None;
        self.file_search.close();
        self.project_search.close();
        self.watch_ignored.clear();
        self.tree = FileTree::default();
        self.index_status = IndexStatus::Indexing { done: 0, total: 0 };
        self.index_rx = Some(spawn_index(root.clone()));
        self.start_watcher(&root);
    }

    fn start_watcher(&mut self, root: &Path) {
        self.watcher = None;
        self.watch_rx = None;
        let (sender, receiver) = mpsc::channel();
        let repaint = self.egui_ctx.clone();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                for path in event.paths {
                    let _ = sender.send(path);
                }
                repaint.request_repaint();
            }
        });

        match watcher {
            Ok(mut watcher) => match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.watcher = Some(watcher);
                    self.watch_rx = Some(receiver);
                }
                Err(error) => {
                    self.toast(format!("Watcher indisponível: {error}"));
                }
            },
            Err(error) => {
                self.toast(format!("Watcher indisponível: {error}"));
            }
        }
    }

    fn reindex(&mut self) {
        let Some(root) = self.root.clone() else {
            self.toast("Abra um projeto antes de reindexar");
            return;
        };
        self.tree.invalidate();
        self.index_status = IndexStatus::Indexing { done: 0, total: 0 };
        self.index_rx = Some(spawn_index(root));
        self.toast("Reindexando o projeto…");
    }

    fn poll_index(&mut self) {
        let mut messages = Vec::new();
        let mut disconnected = false;
        if let Some(receiver) = &self.index_rx {
            loop {
                match receiver.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        for message in messages {
            match message {
                IndexMessage::Progress { done, total } => {
                    self.index_status = IndexStatus::Indexing { done, total };
                }
                IndexMessage::Done(payload) => {
                    let payload = *payload;
                    let stats = payload.index.stats.clone();
                    self.project = Some(payload.project);
                    self.index = Some(payload.index);
                    self.index_status = IndexStatus::Ready {
                        files: stats.java_files,
                        symbols: stats.symbols,
                        problems: stats.problems,
                        elapsed_ms: stats.elapsed_ms,
                    };
                    self.index_rx = None;

                    // Keep outlines useful for files edited before indexing
                    // finished: merge their in-memory contents into the index.
                    let dirty_java: Vec<(PathBuf, String)> = self
                        .tabs
                        .iter()
                        .filter(|tab| tab.doc.dirty && is_java_path(&tab.doc.path))
                        .map(|tab| (tab.doc.path.clone(), tab.doc.text()))
                        .collect();
                    if let Some(index) = &mut self.index {
                        for (path, text) in dirty_java {
                            index.update_file(&path, &text);
                        }
                    }

                    if let Some(index) = &self.index {
                        self.project_types = Some(ProjectTypes::build(index));
                    }
                    if self.frame_bench.is_none() {
                        if let Some(project) = self.project.clone() {
                            self.start_classpath(project);
                        }
                    }

                    if self.tabs.is_empty() {
                        if let Some(first) = self
                            .project
                            .as_ref()
                            .and_then(|project| project.java_files.first().cloned())
                        {
                            self.open_file(first);
                        }
                    }
                }
                IndexMessage::Failed(error) => {
                    self.index_status = IndexStatus::Failed(error.clone());
                    self.index_rx = None;
                    self.toast(format!("Falha ao indexar: {error}"));
                }
            }
        }

        if disconnected && self.index_rx.is_some() {
            self.index_rx = None;
            if matches!(self.index_status, IndexStatus::Indexing { .. }) {
                self.index_status =
                    IndexStatus::Failed("o indexador terminou inesperadamente".to_owned());
            }
        }
    }

    fn poll_watcher(&mut self) {
        let mut paths = Vec::new();
        if let Some(receiver) = &self.watch_rx {
            while let Ok(path) = receiver.try_recv() {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            self.watch_ignored
                .retain(|_, until| Instant::now() < *until);
            return;
        }
        paths.sort();
        paths.dedup();
        self.tree.invalidate();
        let now = Instant::now();
        self.watch_ignored.retain(|_, until| now < *until);

        for raw_path in paths {
            if path_is_ignored(&raw_path) {
                continue;
            }
            let canonical = dunce::canonicalize(&raw_path).unwrap_or_else(|_| raw_path.clone());
            if self.watch_ignored.contains_key(&raw_path)
                || self.watch_ignored.contains_key(&canonical)
            {
                continue;
            }

            let exists = canonical.exists();
            if exists && canonical.is_dir() {
                continue;
            }
            self.note_project_path_change(&canonical, exists);
            if !exists {
                if let Some(active) = self.tabs.iter().position(|tab| tab.doc.path == canonical) {
                    self.tabs[active].external_change = Some("removido do disco".to_owned());
                }
                if is_java_path(&canonical) {
                    if let Some(index) = &mut self.index {
                        index.remove_file(&canonical);
                    }
                }
                continue;
            }

            let open = self.tabs.iter().position(|tab| tab.doc.path == canonical);
            if let Some(tab_index) = open {
                let disk_text = match fs::read_to_string(&canonical) {
                    Ok(text) => text,
                    Err(error) => {
                        self.tabs[tab_index].external_change =
                            Some(format!("não foi possível ler: {error}"));
                        continue;
                    }
                };
                if disk_text == self.tabs[tab_index].doc.text() {
                    self.tabs[tab_index].external_change = None;
                    continue;
                }
                if self.tabs[tab_index].doc.dirty {
                    self.tabs[tab_index].external_change =
                        Some("alterado no disco; sua versão tem mudanças".to_owned());
                    eprintln!(
                        "[reclipse] external change conflict (dirty buffer): {}",
                        canonical.display()
                    );
                    continue;
                }

                if self.tabs[tab_index].reload_from_disk().is_ok() {
                    if is_java_path(&canonical) {
                        if let Some(index) = &mut self.index {
                            index.update_file(&canonical, &disk_text);
                        }
                        self.refresh_semantics();
                    }
                    let _ = crate::recovery::discard(&canonical);
                    self.backup_state.remove(&canonical);
                    eprintln!("[reclipse] reloaded from disk: {}", canonical.display());
                    self.toast(format!("Recarregado do disco: {}", canonical.display()));
                }
            } else if is_java_path(&canonical) {
                if let Ok(text) = fs::read_to_string(&canonical) {
                    if let Some(index) = &mut self.index {
                        index.update_file(&canonical, &text);
                    }
                }
            }
        }
        self.refresh_semantics();
    }

    fn note_project_path_change(&mut self, path: &Path, exists: bool) {
        let Some(project) = &mut self.project else {
            return;
        };
        if exists {
            if !project.all_files.iter().any(|known| known == path) {
                project.all_files.push(path.to_path_buf());
                project.all_files.sort();
            }
            if is_java_path(path) && !project.java_files.iter().any(|known| known == path) {
                project.java_files.push(path.to_path_buf());
                project.java_files.sort();
            }
        } else {
            project.all_files.retain(|known| known != path);
            project.java_files.retain(|known| known != path);
        }
    }

    fn reload_active_from_disk(&mut self) {
        let Some(active) = self.active_tab else {
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        match self.tabs[active].reload_from_disk() {
            Ok(()) => {
                let text = self.tabs[active].doc.text();
                if is_java_path(&path) {
                    if let Some(index) = &mut self.index {
                        index.update_file(&path, &text);
                    }
                    self.refresh_semantics();
                }
                let _ = crate::recovery::discard(&path);
                self.backup_state.remove(&path);
                self.tabs[active].external_change = None;
                self.toast(format!("Recarregado: {}", path.display()));
            }
            Err(error) => self.toast(format!("{error:#}")),
        }
    }

    fn external_change_bar(&mut self, ctx: &Context) {
        let Some(active) = self.active_tab else {
            return;
        };
        let Some(message) = self.tabs[active].external_change.clone() else {
            return;
        };
        let mut reload = false;
        let mut keep = false;

        egui::TopBottomPanel::top("reclipse_external_change")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(0x3a, 0x32, 0x1c))
                    .inner_margin(egui::Margin::symmetric(10, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ARQUIVO ALTERADO NO DISCO")
                            .small()
                            .strong()
                            .color(theme::YELLOW),
                    );
                    ui.label(RichText::new(message).small().color(theme::TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Manter minha versão").clicked() {
                            keep = true;
                        }
                        if ui.button("Recarregar").clicked() {
                            reload = true;
                        }
                    });
                });
            });

        if reload {
            self.reload_active_from_disk();
        }
        if keep {
            self.tabs[active].external_change = None;
            self.toast("Mantendo a versão em memória; salve para sobrescrever o disco.");
        }
    }

    fn start_classpath(&mut self, project: Project) {
        self.classpath_index = None;
        self.classpath_status = ClasspathStatus::Indexing { done: 0, total: 0 };
        self.semantic_problems.clear();
        self.classpath_rx = Some(spawn_classpath(project));
    }

    fn refresh_semantics(&mut self) {
        let project_types = self.index.as_ref().map(ProjectTypes::build);
        let problems = if let (Some(index), Some(types), Some(classpath)) = (
            self.index.as_ref(),
            project_types.as_ref(),
            self.classpath_index.as_ref(),
        ) {
            compute_semantic_problems(Some(index), Some(types), classpath)
        } else {
            Vec::new()
        };
        self.project_types = project_types;
        self.semantic_problems = problems;
    }

    fn poll_classpath(&mut self) {
        let mut messages = Vec::new();
        let mut disconnected = false;
        if let Some(receiver) = &self.classpath_rx {
            loop {
                match receiver.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                ClasspathMessage::Progress { done, total } => {
                    if matches!(self.classpath_status, ClasspathStatus::Indexing { .. }) {
                        self.classpath_status = ClasspathStatus::Indexing { done, total };
                    }
                }
                ClasspathMessage::Ready(index) => {
                    let stats = index.stats.clone();
                    self.semantic_problems = compute_semantic_problems(
                        self.index.as_ref(),
                        self.project_types.as_ref(),
                        &index,
                    );
                    self.classpath_status = ClasspathStatus::Ready(stats);
                    self.classpath_index = Some(*index);
                    self.classpath_rx = None;
                }
                ClasspathMessage::Failed(error) => {
                    self.classpath_status = ClasspathStatus::Failed(error.clone());
                    self.classpath_rx = None;
                    self.toast(format!("Classpath: {error}"));
                }
            }
        }
        if disconnected && self.classpath_rx.is_some() {
            self.classpath_rx = None;
            if matches!(self.classpath_status, ClasspathStatus::Indexing { .. }) {
                self.classpath_status =
                    ClasspathStatus::Failed("indexador de classpath interrompido".to_owned());
            }
        }
    }

    fn open_folder_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Abrir projeto")
            .pick_folder()
        {
            self.open_folder(path);
        }
    }

    fn open_file_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Abrir arquivo")
            .pick_file()
        {
            if self.root.is_none() {
                if let Some(parent) = path.parent() {
                    self.start_project(parent.to_path_buf());
                }
            }
            self.open_file(path);
        }
    }

    fn open_folder(&mut self, path: PathBuf) {
        if self.tabs.iter().any(|tab| tab.doc.dirty) {
            self.toast("Salve ou feche os arquivos modificados antes de trocar de projeto.");
            return;
        }
        self.start_project(path);
    }

    fn open_file(&mut self, path: PathBuf) {
        let path = dunce::canonicalize(&path).unwrap_or(path);
        if let Some(index) = self.tabs.iter().position(|tab| tab.doc.path == path) {
            self.active_tab = Some(index);
            return;
        }

        match Document::open(path.clone()) {
            Ok(doc) => {
                let is_java = is_java_path(&path);
                let needs_index = is_java
                    && self
                        .index
                        .as_ref()
                        .map(|index| index.symbols_for(&path).is_none())
                        .unwrap_or(false);
                let text_for_index = if needs_index { Some(doc.text()) } else { None };

                self.tabs.push(Tab::new(doc));
                self.active_tab = Some(self.tabs.len() - 1);

                if let (Some(index), Some(text)) = (&mut self.index, text_for_index) {
                    index.update_file(&path, &text);
                    self.refresh_semantics();
                }
            }
            Err(error) => self.toast(format!("{error:#}")),
        }
    }

    fn open_at(&mut self, path: PathBuf, line: usize, column: usize) {
        self.open_file(path);
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                let index = tab.doc.char_index_for_line_col(line, column);
                tab.reveal_cursor(index, true);
            }
        }
    }

    fn save_active(&mut self) -> bool {
        match self.active_tab {
            Some(index) => self.save_tab(index),
            None => false,
        }
    }

    fn save_tab(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        let path = tab.doc.path.clone();
        let is_java = is_java_path(&path);
        let text = tab.doc.text();

        if let Err(error) = self.tabs[index].doc.save() {
            self.toast(format!("{error:#}"));
            return false;
        }
        if is_java {
            if let Some(index) = &mut self.index {
                index.update_file(&path, &text);
            }
            self.refresh_semantics();
        }
        let _ = crate::recovery::discard(&path);
        self.backup_state.remove(&path);
        self.tabs[index].external_change = None;
        self.watch_ignored
            .insert(path.clone(), Instant::now() + Duration::from_secs(2));
        self.toast(format!("Salvo: {}", path.display()));
        true
    }

    fn request_close(&mut self, index: usize) {
        let dirty = self
            .tabs
            .get(index)
            .map(|tab| tab.doc.dirty)
            .unwrap_or(false);
        if dirty {
            self.close_confirm = Some(index);
        } else {
            self.force_close(index);
        }
    }

    fn force_close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let path = self.tabs[index].doc.path.clone();
        let _ = crate::recovery::discard(&path);
        self.backup_state.remove(&path);
        self.tabs.remove(index);
        match self.active_tab {
            None => {}
            Some(active) if active == index => {
                self.active_tab = if self.tabs.is_empty() {
                    None
                } else {
                    Some(index.min(self.tabs.len() - 1))
                };
            }
            Some(active) if active > index => {
                self.active_tab = Some(active - 1);
            }
            _ => {}
        }
    }

    fn goto_definition(&mut self) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let tab = &self.tabs[active];
        let Some((word, _, _)) = tab.doc.identifier_at_cursor() else {
            self.toast("Coloque o cursor sobre um identificador");
            return;
        };
        let from_file = tab.doc.path.clone();
        let location = self
            .index
            .as_ref()
            .and_then(|index| index.find_best(&word, &from_file))
            .cloned();

        if let Some(location) = location {
            self.open_at(location.file, location.symbol.line, location.symbol.column);
            return;
        }

        let resolved = {
            let file = self
                .index
                .as_ref()
                .and_then(|index| index.symbols_for(&from_file));
            let types = self.project_types.as_ref();
            let classpath = self.classpath_index.as_ref();
            match (file, types, classpath) {
                (Some(file), Some(types), Some(classpath)) => {
                    resolve_semantic_type(&word, file, types, classpath)
                }
                _ => None,
            }
        };
        match resolved {
            Some(ResolvedType::Project(location)) => {
                self.open_at(location.path, location.line, location.column);
            }
            Some(ResolvedType::Classpath(name)) => {
                self.classpath_view = Some(name);
            }
            None => self.toast(format!(
                "Definição de '{word}' não encontrada no índice sintático/classpath"
            )),
        }
    }

    fn undo_active(&mut self) {
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.doc.undo();
                tab.invalidate_highlight();
                tab.focus();
            }
        }
    }

    fn redo_active(&mut self) {
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.doc.redo();
                tab.invalidate_highlight();
                tab.focus();
            }
        }
    }

    fn session_snapshot(&self) -> crate::session::Session {
        crate::session::Session {
            root: self.root.clone(),
            tabs: self
                .tabs
                .iter()
                .take(100)
                .map(|tab| crate::session::TabSession {
                    path: tab.doc.path.clone(),
                    cursor: tab.doc.cursor_char,
                    anchor: tab.doc.anchor_char,
                    folded: tab.folded_ranges(),
                })
                .collect(),
            active: self
                .active_tab
                .and_then(|index| self.tabs.get(index))
                .map(|tab| tab.doc.path.clone()),
            show_project: self.show_project,
            show_outline: self.show_outline,
            show_problems: self.show_problems,
            font_size: self.font_size,
            keybindings: self.keybindings.clone(),
        }
    }

    fn open_settings(&mut self) {
        let Some(path) = crate::session::config_path() else {
            self.toast("Diretório de configuração indisponível");
            return;
        };
        if !path.exists() {
            let session = self.session_snapshot();
            if let Err(error) = crate::session::save(&session) {
                self.toast(format!("Não foi possível salvar a sessão: {error:#}"));
                return;
            }
        }
        self.open_file(path);
    }

    fn add_cursor_vertical(&mut self, delta: i32) {
        let Some(active) = self.active_tab else {
            return;
        };
        let tab = &mut self.tabs[active];
        tab.doc.add_cursor_vertical(delta);
        tab.focus();
    }

    fn clear_extra_cursors(&mut self) {
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                let cursor = tab.doc.cursor_char;
                tab.doc.set_cursor(cursor, false);
                tab.focus();
            }
        }
    }

    fn zoom_by(&mut self, delta: f32) {
        self.font_size = (self.font_size + delta).clamp(10.0, 28.0);
        for tab in &mut self.tabs {
            tab.invalidate_highlight();
        }
        self.toast(format!("Fonte: {:.1} pt", self.font_size));
    }

    fn close_search(&mut self) {
        self.file_search.close();
        self.project_search.close();
        for tab in &mut self.tabs {
            tab.clear_search_matches();
        }
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.focus();
            }
        }
    }

    fn begin_file_search(&mut self, replace: bool) {
        self.project_search.close();
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get(active) {
                if let Some(selection) = tab.doc.selection_text() {
                    if !selection.contains('\n') && selection.chars().count() <= 200 {
                        self.file_search.query = selection;
                    }
                } else if let Some((word, _, _)) = tab.doc.identifier_at_cursor() {
                    self.file_search.query = word;
                }
            }
        }
        self.file_search.open(replace);
        self.sync_file_search();
        self.select_file_match(false);
    }

    fn sync_file_search(&mut self) {
        if !self.file_search.open {
            if let Some(active) = self.active_tab {
                if let Some(tab) = self.tabs.get_mut(active) {
                    tab.clear_search_matches();
                }
            }
            return;
        }
        let Some(active) = self.active_tab else {
            return;
        };
        self.file_search.ensure(&self.tabs[active].doc);
        let matches = self.file_search.matches.clone();
        let current = if matches.is_empty() {
            None
        } else {
            Some(self.file_search.current.min(matches.len() - 1))
        };
        if let Some(tab) = self.tabs.get_mut(active) {
            tab.set_search_matches(matches, current);
        }
    }

    fn select_file_match(&mut self, focus_editor: bool) {
        let Some((start, end)) = self.file_search.current_match() else {
            return;
        };
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                tab.reveal_range(start, end, focus_editor);
            }
        }
    }

    fn file_search_next(&mut self) {
        self.file_search.next();
        self.select_file_match(false);
    }

    fn file_search_previous(&mut self) {
        self.file_search.previous();
        self.select_file_match(false);
    }

    fn file_search_matcher(&self) -> Result<SearchMatcher, String> {
        SearchMatcher::new(
            &self.file_search.query,
            SearchOptions {
                case_sensitive: self.file_search.case_sensitive,
                whole_word: self.file_search.whole_word,
                regex: self.file_search.regex,
            },
        )
    }

    fn replace_current_match(&mut self) {
        let Some((start, end)) = self.file_search.current_match() else {
            return;
        };
        let Some(active) = self.active_tab else {
            return;
        };
        let matcher = match self.file_search_matcher() {
            Ok(matcher) => matcher,
            Err(error) => {
                self.toast(format!("Regex inválida: {error}"));
                return;
            }
        };
        let text = self.tabs[active].doc.text();
        let replacement = matcher
            .expand_replacements(&text, &[(start, end)], &self.file_search.replacement)
            .into_iter()
            .next()
            .unwrap_or_else(|| self.file_search.replacement.clone());
        if let Some(tab) = self.tabs.get_mut(active) {
            tab.doc.edit(start, end, &replacement);
        }
        self.sync_file_search();
        self.select_file_match(false);
    }

    fn replace_all_matches(&mut self) {
        let matches = self.file_search.matches.clone();
        if matches.is_empty() {
            return;
        }
        let Some(active) = self.active_tab else {
            return;
        };
        let matcher = match self.file_search_matcher() {
            Ok(matcher) => matcher,
            Err(error) => {
                self.toast(format!("Regex inválida: {error}"));
                return;
            }
        };
        let text = self.tabs[active].doc.text();
        let replacements =
            matcher.expand_replacements(&text, &matches, &self.file_search.replacement);
        let count = matches.len();
        if let Some(tab) = self.tabs.get_mut(active) {
            for ((start, end), replacement) in matches.into_iter().zip(replacements).rev() {
                tab.doc.edit(start, end, &replacement);
            }
        }
        self.sync_file_search();
        self.select_file_match(false);
        self.toast(format!("{count} ocorrência(s) substituída(s)"));
    }

    fn begin_project_search(&mut self) {
        self.file_search.close();
        for tab in &mut self.tabs {
            tab.clear_search_matches();
        }
        if self.project_search.query.is_empty() && !self.file_search.query.is_empty() {
            self.project_search.query = self.file_search.query.clone();
        }
        self.project_search.open();
    }

    fn open_type_palette(&mut self) {
        if self
            .index
            .as_ref()
            .is_none_or(|index| index.by_name.is_empty())
        {
            self.toast("Índice ainda vazio; aguarde a indexação");
            return;
        }
        self.palette.open(PaletteMode::Types);
    }

    fn quick_outline_palette(&mut self) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let path = &self.tabs[active].doc.path;
        if self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(path))
            .is_none_or(|file| file.symbols.is_empty())
        {
            self.toast("Salve o arquivo para atualizar o outline");
            return;
        }
        self.palette.open(PaletteMode::Outline);
    }

    fn run_project_search(&mut self) {
        let files = self.project.as_ref().map(project_files).unwrap_or_default();
        self.project_search.start(files);
    }

    fn show_recovery(&mut self, ctx: &Context) {
        if !self.recovery_open || self.recovery_entries.is_empty() {
            self.recovery_open = false;
            return;
        }
        let mut open = true;
        let mut restore_index = None;
        let mut discard_index = None;
        let mut restore_all = false;
        let mut discard_all = false;

        egui::Window::new("Recuperar buffers não salvos")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(
                    RichText::new(
                        "A IDE encontrou conteúdo não salvo de uma sessão anterior. \
                         Recupere os buffers desejados ou descarte-os.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, entry) in self.recovery_entries.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let size = entry.meta.size;
                                ui.label(
                                    RichText::new(entry.meta.path.display().to_string()).small(),
                                );
                                ui.label(
                                    RichText::new(format!("{size} bytes"))
                                        .small()
                                        .color(theme::MUTED),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("Descartar").clicked() {
                                        discard_index = Some(index);
                                    }
                                    if ui.small_button("Recuperar").clicked() {
                                        restore_index = Some(index);
                                    }
                                });
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Recuperar todos").clicked() {
                        restore_all = true;
                    }
                    if ui.button("Descartar todos").clicked() {
                        discard_all = true;
                    }
                });
            });

        if let Some(index) = restore_index {
            self.restore_recovery(index);
        } else if let Some(index) = discard_index {
            self.discard_recovery(index);
        } else if restore_all {
            while !self.recovery_entries.is_empty() {
                let before = self.recovery_entries.len();
                self.restore_recovery(0);
                if self.recovery_entries.len() == before {
                    break;
                }
            }
        } else if discard_all {
            let entries = std::mem::take(&mut self.recovery_entries);
            for entry in &entries {
                let _ = crate::recovery::discard_entry(entry);
            }
        }

        self.recovery_open = open && !self.recovery_entries.is_empty();
    }

    fn restore_recovery(&mut self, index: usize) {
        let Some(entry) = self.recovery_entries.get(index).cloned() else {
            return;
        };
        let Some(content) = entry.content() else {
            self.toast("Backup de recuperação ilegível");
            self.discard_recovery(index);
            return;
        };
        if !entry.meta.path.exists() {
            if let Some(parent) = entry.meta.path.parent() {
                if !parent.exists() {
                    self.toast(format!(
                        "Diretório original não existe: {}",
                        parent.display()
                    ));
                    return;
                }
            }
            if let Err(error) = fs::write(&entry.meta.path, content.as_bytes()) {
                self.toast(format!(
                    "Falha ao recriar {}: {error}",
                    entry.meta.path.display()
                ));
                return;
            }
        }

        self.open_file(entry.meta.path.clone());
        let canonical =
            dunce::canonicalize(&entry.meta.path).unwrap_or_else(|_| entry.meta.path.clone());
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.doc.path == canonical) else {
            self.toast("Não foi possível abrir o arquivo recuperado");
            return;
        };
        tab.doc
            .restore_text(&content, entry.meta.cursor, entry.meta.anchor);
        tab.invalidate_highlight();
        tab.external_change = None;
        tab.focus();
        if is_java_path(&canonical) {
            if let Some(index) = &mut self.index {
                index.update_file(&canonical, &content);
            }
        }
        let _ = crate::recovery::discard_entry(&entry);
        self.recovery_entries.remove(index);
        self.toast(format!("Buffer recuperado: {}", canonical.display()));
    }

    fn discard_recovery(&mut self, index: usize) {
        if index >= self.recovery_entries.len() {
            return;
        }
        let entry = self.recovery_entries.remove(index);
        let _ = crate::recovery::discard_entry(&entry);
    }

    fn persist_dirty_backups(&mut self) {
        if self.frame_bench.is_some() {
            return;
        }
        let now = Instant::now();
        for index in 0..self.tabs.len() {
            if !self.tabs[index].doc.dirty {
                continue;
            }
            let path = self.tabs[index].doc.path.clone();
            let revision = self.tabs[index].doc.revision;
            let should_write =
                self.backup_state
                    .get(&path)
                    .is_none_or(|(last_revision, last_at)| {
                        *last_revision != revision
                            && now.duration_since(*last_at) >= Duration::from_millis(700)
                    });
            if !should_write {
                continue;
            }
            if let Err(error) = crate::recovery::write_backup(&self.tabs[index].doc) {
                eprintln!("[reclipse] failed to write recovery backup: {error:#}");
            } else {
                self.backup_state.insert(path, (revision, now));
            }
        }
    }

    fn show_project_replace_confirm(&mut self, ctx: &Context) {
        if !self.project_replace_confirm {
            return;
        }
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        let unique_files = self
            .project_search
            .results
            .iter()
            .map(|found| &found.file)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let occurrences = self.project_search.results.len();

        egui::Window::new("Substituir no projeto")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(format!(
                    "Substituir {occurrences} ocorrência(s) em {unique_files} arquivo(s)?"
                ));
                ui.label(
                    RichText::new(
                        "Arquivos abertos com alterações não salvas serão ignorados. \
                         Em modo regex, grupos como $1 e ${name} são expandidos.",
                    )
                    .small()
                    .color(theme::YELLOW),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Substituir").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.project_replace_confirm = false;
            self.apply_project_replace();
        } else if cancel || !open {
            self.project_replace_confirm = false;
        }
    }

    fn apply_project_replace(&mut self) {
        let query = self.project_search.query.clone();
        let options = SearchOptions {
            case_sensitive: self.project_search.case_sensitive,
            whole_word: self.project_search.whole_word,
            regex: self.project_search.regex,
        };
        let replacement = self.project_search.replacement.clone();
        let matcher = match SearchMatcher::new(&query, options) {
            Ok(matcher) => matcher,
            Err(error) => {
                self.toast(format!("Regex inválida: {error}"));
                return;
            }
        };
        let mut files: Vec<PathBuf> = self
            .project_search
            .results
            .iter()
            .map(|found| found.file.clone())
            .collect();
        files.sort();
        files.dedup();
        if files.is_empty() {
            self.toast("Nenhum resultado para substituir");
            return;
        }

        let mut changed_files = 0;
        let mut occurrences = 0;
        let mut skipped = 0;
        for file in files {
            if let Some(tab_index) = self.tabs.iter().position(|tab| tab.doc.path == file) {
                if self.tabs[tab_index].doc.dirty {
                    skipped += 1;
                    continue;
                }
                let text = self.tabs[tab_index].doc.text();
                let matches = matcher.find_all(&text);
                if matches.is_empty() {
                    continue;
                }
                let count = matches.len();
                let replacements = matcher.expand_replacements(&text, &matches, &replacement);
                for ((start, end), replacement) in matches.into_iter().zip(replacements).rev() {
                    self.tabs[tab_index].doc.edit(start, end, &replacement);
                }
                if let Err(error) = self.tabs[tab_index].doc.save() {
                    self.toast(format!("Falha ao salvar {}: {error:#}", file.display()));
                    skipped += 1;
                    continue;
                }
                let updated = self.tabs[tab_index].doc.text();
                if is_java_path(&file) {
                    if let Some(index) = &mut self.index {
                        index.update_file(&file, &updated);
                    }
                }
                changed_files += 1;
                occurrences += count;
            } else {
                let Ok(text) = fs::read_to_string(&file) else {
                    skipped += 1;
                    continue;
                };
                let matches = matcher.find_all(&text);
                if matches.is_empty() {
                    continue;
                }
                let count = matches.len();
                let replacements = matcher.expand_replacements(&text, &matches, &replacement);
                let mut output = text.clone();
                for ((start, end), replacement) in matches.into_iter().zip(replacements).rev() {
                    let byte_start = char_to_byte(&output, start);
                    let byte_end = char_to_byte(&output, end);
                    output.replace_range(byte_start..byte_end, &replacement);
                }
                if let Err(error) = fs::write(&file, &output) {
                    self.toast(format!("Falha ao escrever {}: {error:#}", file.display()));
                    skipped += 1;
                    continue;
                }
                if is_java_path(&file) {
                    if let Some(index) = &mut self.index {
                        index.update_file(&file, &output);
                    }
                }
                changed_files += 1;
                occurrences += count;
            }
        }

        self.run_project_search();
        self.refresh_semantics();
        self.toast(format!(
            "{occurrences} ocorrência(s) em {changed_files} arquivo(s) substituída(s){}",
            if skipped > 0 {
                format!("; {skipped} arquivo(s) ignorado(s)")
            } else {
                String::new()
            }
        ));
    }

    fn open_project_match(&mut self, index: usize) {
        let Some(found) = self.project_search.results.get(index).cloned() else {
            return;
        };
        self.project_search.selected = index;
        self.open_file(found.file);
        if let Some(active) = self.active_tab {
            if let Some(tab) = self.tabs.get_mut(active) {
                let start = tab.doc.char_index_for_line_col(found.line, found.column);
                let length = found.end.saturating_sub(found.start);
                tab.reveal_range(start, start + length, true);
            }
        }
    }

    fn search_panel(&mut self, ctx: &Context) {
        if self.file_search.open {
            self.file_search_panel(ctx);
        } else if self.project_search.open {
            self.project_search_panel(ctx);
        }
    }

    fn file_search_panel(&mut self, ctx: &Context) {
        let mut close = false;
        let mut next = false;
        let mut previous = false;
        let mut replace_one = false;
        let mut replace_all = false;
        let mut query_changed = false;
        let mut search_has_focus = false;
        let matches = self.file_search.matches.len();
        let current = if matches == 0 {
            0
        } else {
            self.file_search.current.min(matches - 1) + 1
        };

        egui::TopBottomPanel::bottom("reclipse_file_search")
            .resizable(true)
            .default_height(if self.file_search.replace { 96.0 } else { 62.0 })
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("BUSCAR").small().strong().color(theme::MUTED));
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.file_search.query)
                            .id(egui::Id::new("reclipse_file_search_query"))
                            .hint_text("texto")
                            .desired_width(300.0),
                    );
                    if self.file_search.focus_pending {
                        response.request_focus();
                        self.file_search.focus_pending = false;
                    }
                    if response.has_focus() {
                        search_has_focus = true;
                    }
                    if response.changed() {
                        query_changed = true;
                    }
                    if ui
                        .checkbox(&mut self.file_search.case_sensitive, "Aa")
                        .changed()
                    {
                        query_changed = true;
                    }
                    if ui
                        .checkbox(&mut self.file_search.whole_word, "W")
                        .on_hover_text("Palavra inteira")
                        .changed()
                    {
                        query_changed = true;
                    }
                    if ui
                        .checkbox(&mut self.file_search.regex, ".*")
                        .on_hover_text("Expressão regular")
                        .changed()
                    {
                        query_changed = true;
                    }
                    ui.label(
                        RichText::new(if matches == 0 {
                            "nenhum resultado".to_owned()
                        } else {
                            format!("{current} de {matches}")
                        })
                        .small()
                        .color(theme::MUTED),
                    );
                    if ui.button("Anterior").clicked() {
                        previous = true;
                    }
                    if ui.button("Próximo").clicked() {
                        next = true;
                    }
                    if ui.button("Fechar").clicked() {
                        close = true;
                    }
                });

                if self.file_search.replace {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Substituir por").color(theme::MUTED));
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.file_search.replacement)
                                .id(egui::Id::new("reclipse_file_replace_query"))
                                .desired_width(300.0),
                        );
                        if response.has_focus() {
                            search_has_focus = true;
                        }
                        if ui.button("Substituir").clicked() {
                            replace_one = true;
                        }
                        if ui.button("Substituir tudo").clicked() {
                            replace_all = true;
                        }
                    });
                }

                if let Some(error) = &self.file_search.error {
                    ui.label(RichText::new(error).small().color(theme::RED));
                }

                if search_has_focus && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    if ui.input(|input| input.modifiers.shift) {
                        previous = true;
                    } else {
                        next = true;
                    }
                }
            });

        if query_changed {
            self.sync_file_search();
            self.select_file_match(false);
        }
        if previous {
            self.file_search_previous();
        }
        if next {
            self.file_search_next();
        }
        if replace_one {
            self.replace_current_match();
        }
        if replace_all {
            self.replace_all_matches();
        }
        if close {
            self.close_search();
        }
    }

    fn project_search_panel(&mut self, ctx: &Context) {
        let mut close = false;
        let mut run = false;
        let mut replace_project = false;
        let mut open_match = None;
        let project_name = self.project.as_ref().map(|project| project.name.clone());
        let results = self.project_search.results.clone();
        let selected = self.project_search.selected;
        let running = self.project_search.running;
        let searched = self.project_search.searched;
        let error = self.project_search.error.clone();
        let total = results.len();
        let mut search_has_focus = false;

        egui::TopBottomPanel::bottom("reclipse_project_search")
            .resizable(true)
            .default_height(220.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("BUSCAR NO PROJETO")
                            .small()
                            .strong()
                            .color(theme::MUTED),
                    );
                    if let Some(name) = &project_name {
                        ui.label(RichText::new(name).small().color(theme::MUTED));
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.project_search.query)
                            .id(egui::Id::new("reclipse_project_search_query"))
                            .hint_text("texto")
                            .desired_width(280.0),
                    );
                    if self.project_search.focus_pending {
                        response.request_focus();
                        self.project_search.focus_pending = false;
                    }
                    if response.has_focus() {
                        search_has_focus = true;
                    }
                    ui.checkbox(&mut self.project_search.case_sensitive, "Aa");
                    if ui
                        .checkbox(&mut self.project_search.whole_word, "W")
                        .on_hover_text("Palavra inteira")
                        .changed()
                    {
                        run = true;
                    }
                    if ui
                        .checkbox(&mut self.project_search.regex, ".*")
                        .on_hover_text("Expressão regular")
                        .changed()
                    {
                        run = true;
                    }
                    if ui.button("Buscar").clicked() {
                        run = true;
                    }
                    if running {
                        ui.spinner();
                    }
                    ui.label(
                        RichText::new(format!("{total} resultado(s)"))
                            .small()
                            .color(theme::MUTED),
                    );
                    if ui.button("Fechar").clicked() {
                        close = true;
                    }
                });

                if search_has_focus && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    run = true;
                }

                if let Some(error) = &error {
                    ui.label(RichText::new(error).color(theme::RED));
                }

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Substituir por").color(theme::MUTED));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.project_search.replacement)
                            .id(egui::Id::new("reclipse_project_replace_query"))
                            .desired_width(280.0),
                    );
                    if ui.button("Substituir no projeto").clicked() {
                        replace_project = true;
                    }
                    ui.label(
                        RichText::new("buffers sujos são ignorados; regex expande grupos $1")
                            .small()
                            .color(theme::MUTED),
                    );
                });

                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if results.is_empty() {
                            ui.label(
                                RichText::new(if searched && !running {
                                    "Nenhum resultado"
                                } else {
                                    "Digite e pressione Enter para buscar"
                                })
                                .small()
                                .color(theme::MUTED),
                            );
                        }
                        for (index, found) in results.iter().enumerate() {
                            let label = format!(
                                "{}:{}:{}  {}",
                                found.file.file_name().unwrap_or_default().to_string_lossy(),
                                found.line,
                                found.column,
                                found.preview
                            );
                            if ui
                                .selectable_label(
                                    index == selected,
                                    RichText::new(label).size(12.0),
                                )
                                .on_hover_text(found.file.display().to_string())
                                .clicked()
                            {
                                open_match = Some(index);
                            }
                        }
                    });
            });

        if run {
            self.run_project_search();
        }
        if replace_project {
            self.project_replace_confirm = true;
        }
        if close {
            self.close_search();
        }
        if let Some(index) = open_match {
            self.open_project_match(index);
        }
    }

    fn insert_into_active(&mut self, insertion: &str, replace_selection: bool) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo para inserir texto");
            return;
        };
        let tab = &mut self.tabs[active];
        let (start, end) = if replace_selection {
            tab.doc
                .selection()
                .unwrap_or((tab.doc.cursor_char, tab.doc.cursor_char))
        } else {
            (tab.doc.cursor_char, tab.doc.cursor_char)
        };

        tab.doc.edit(start, end, insertion);
        tab.pending_cursor = Some(tab.doc.cursor_char);
        tab.focus();
    }

    fn run_plugin(&mut self, plugin_index: usize, command_id: String, title: String) {
        let active = self.active_tab.and_then(|index| self.tabs.get(index));
        let active_file = active.map(|tab| tab.doc.path.to_string_lossy().into_owned());
        let selection = active.and_then(|tab| tab.doc.selection_text());
        let context = InvokeContext {
            active_file,
            project_root: self
                .root
                .as_ref()
                .map(|root| root.to_string_lossy().into_owned()),
            selection,
        };

        match self.plugins.execute(plugin_index, &command_id, &context) {
            Ok(response) => {
                if let (Some(text), replace) = (response.insert_text, response.replace_selection) {
                    self.insert_into_active(&text, replace);
                }
                if let Some(path) = response.open_file {
                    self.open_at(PathBuf::from(path), response.line.unwrap_or(1), 1);
                }
                if let Some(message) = response.message {
                    self.toast(message);
                } else {
                    self.toast(format!("Plugin executou: {title}"));
                }
            }
            Err(error) => self.toast(format!("Plugin: {error:#}")),
        }
    }

    fn reload_plugins(&mut self) {
        self.plugins.reload();
        self.plugin_commands = self.plugins.commands();
        let count = self.plugins.plugin_count();
        let errors = self.plugins.load_errors().len();
        let suffix = if errors > 0 {
            format!(", {errors} falha(s)")
        } else {
            String::new()
        };
        self.toast(format!("{count} plugin(s) carregado(s){suffix}"));
    }

    fn toast(&mut self, message: impl Into<String>) {
        self.status_message = Some(Toast {
            text: message.into(),
            until: Instant::now() + Duration::from_secs(5),
        });
    }

    fn palette_entries(&self) -> Vec<PaletteEntry> {
        match self.palette.mode {
            Some(PaletteMode::Commands) => {
                let mut entries = vec![
                    PaletteEntry::new(
                        "Arquivo: abrir pasta…",
                        self.keymap.label(KeyAction::OpenFolder),
                        Action::OpenFolder,
                    ),
                    PaletteEntry::new(
                        "Arquivo: abrir arquivo…",
                        self.keymap.label(KeyAction::OpenFile),
                        Action::OpenFile,
                    ),
                    PaletteEntry::new(
                        "Arquivo: salvar",
                        self.keymap.label(KeyAction::Save),
                        Action::Save,
                    ),
                    PaletteEntry::new(
                        "Arquivo: fechar aba",
                        self.keymap.label(KeyAction::CloseTab),
                        Action::CloseTab,
                    ),
                    PaletteEntry::new(
                        "Projeto: reindexar",
                        self.keymap.label(KeyAction::Reindex),
                        Action::Reindex,
                    ),
                    PaletteEntry::new(
                        "Ir para definição",
                        self.keymap.label(KeyAction::GoToDefinition),
                        Action::GoToDefinition,
                    ),
                    PaletteEntry::new(
                        "Editar: localizar",
                        self.keymap.label(KeyAction::FindInFile),
                        Action::FindInFile,
                    ),
                    PaletteEntry::new(
                        "Editar: substituir",
                        self.keymap.label(KeyAction::ReplaceInFile),
                        Action::ReplaceInFile,
                    ),
                    PaletteEntry::new(
                        "Editar: buscar no projeto",
                        self.keymap.label(KeyAction::FindInProject),
                        Action::FindInProject,
                    ),
                    PaletteEntry::new(
                        "Editar: próxima ocorrência",
                        self.keymap.label(KeyAction::FindNext),
                        Action::FindNext,
                    ),
                    PaletteEntry::new(
                        "Editar: ocorrência anterior",
                        self.keymap.label(KeyAction::FindPrevious),
                        Action::FindPrevious,
                    ),
                    PaletteEntry::new(
                        "Navegar: abrir recurso",
                        self.keymap.label(KeyAction::QuickOpen),
                        Action::QuickOpen,
                    ),
                    PaletteEntry::new(
                        "Navegar: abrir tipo",
                        self.keymap.label(KeyAction::OpenType),
                        Action::OpenTypes,
                    ),
                    PaletteEntry::new(
                        "Navegar: quick outline",
                        self.keymap.label(KeyAction::QuickOutline),
                        Action::QuickOutline,
                    ),
                    PaletteEntry::new("Editar: desfazer", "Ctrl+Z", Action::Undo),
                    PaletteEntry::new("Editar: refazer", "Ctrl+Y", Action::Redo),
                    PaletteEntry::new(
                        "Editar: adicionar cursor abaixo",
                        "Ctrl+Alt+↓",
                        Action::AddCursorBelow,
                    ),
                    PaletteEntry::new(
                        "Editar: adicionar cursor acima",
                        "Ctrl+Alt+↑",
                        Action::AddCursorAbove,
                    ),
                    PaletteEntry::new("Editar: reduzir cursores", "Esc", Action::ClearExtraCursors),
                    PaletteEntry::new("Exibir: projeto", "", Action::ToggleProject),
                    PaletteEntry::new("Exibir: estrutura", "", Action::ToggleOutline),
                    PaletteEntry::new("Exibir: problemas", "", Action::ToggleProblems),
                    PaletteEntry::new("Exibir: aumentar fonte", "", Action::ZoomIn),
                    PaletteEntry::new("Exibir: diminuir fonte", "", Action::ZoomOut),
                    PaletteEntry::new(
                        "Configurações: abrir session.json",
                        "",
                        Action::OpenSettings,
                    ),
                    PaletteEntry::new(
                        "Ajuda: atalhos do teclado",
                        self.keymap.label(KeyAction::ShortcutHelp),
                        Action::ShowShortcuts,
                    ),
                    PaletteEntry::new("Plugins: recarregar", "", Action::ReloadPlugins),
                    PaletteEntry::new("Ajuda: sobre o Reclipse", "", Action::About),
                ];
                for command in &self.plugin_commands {
                    entries.push(PaletteEntry::new(
                        format!("{}: {}", command.plugin_name, command.title),
                        "plugin",
                        Action::Plugin {
                            plugin_index: command.plugin_index,
                            command_id: command.id.clone(),
                            title: command.title.clone(),
                        },
                    ));
                }
                entries
            }
            Some(PaletteMode::Files) => self
                .project
                .as_ref()
                .map(|project| {
                    project
                        .all_files
                        .iter()
                        .map(|path| {
                            PaletteEntry::new(
                                project.display_path(path),
                                if is_java_path(path) { "Java" } else { "" },
                                Action::OpenPath(path.clone()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Some(PaletteMode::Types) => {
                let mut entries = Vec::new();
                if let Some(index) = &self.index {
                    for (path, file) in &index.files {
                        for symbol in &file.symbols {
                            if !symbol.kind.is_type() {
                                continue;
                            }
                            let mut label = String::new();
                            if let Some(package) = &file.package {
                                label.push_str(package);
                                label.push('.');
                            }
                            if let Some(container) = &symbol.container {
                                label.push_str(container);
                                label.push('.');
                            }
                            label.push_str(&symbol.name);
                            let hint = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            entries.push(PaletteEntry::new(
                                label,
                                hint,
                                Action::OpenAt(path.clone(), symbol.line, symbol.column),
                            ));
                        }
                    }
                }
                entries.sort_by(|left, right| left.label.cmp(&right.label));
                if let Some(classpath) = &self.classpath_index {
                    let query = self.palette.query.trim().to_lowercase();
                    let limit = if query.is_empty() { 500 } else { 2_000 };
                    let mut added = 0;
                    for name in &classpath.type_names {
                        if !query.is_empty() && !name.to_lowercase().contains(&query) {
                            continue;
                        }
                        entries.push(PaletteEntry::new(
                            name.clone(),
                            "classpath",
                            Action::OpenClasspathType(name.clone()),
                        ));
                        added += 1;
                        if added >= limit {
                            break;
                        }
                    }
                }
                entries
            }
            Some(PaletteMode::Outline) => {
                let Some(active) = self.active_tab else {
                    return Vec::new();
                };
                let path = self.tabs[active].doc.path.clone();
                let symbols: Vec<_> = self
                    .index
                    .as_ref()
                    .and_then(|index| index.symbols_for(&path))
                    .map(|file| file.symbols.clone())
                    .unwrap_or_default();
                symbols
                    .into_iter()
                    .map(|symbol| {
                        PaletteEntry::new(
                            format!(
                                "{}{}  [{}]",
                                "  ".repeat(symbol.depth),
                                symbol.name,
                                symbol.kind.label()
                            ),
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            Action::OpenAt(path.clone(), symbol.line, symbol.column),
                        )
                    })
                    .collect()
            }
            None => Vec::new(),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        if self.palette.open || self.close_confirm.is_some() {
            return;
        }

        if (self.file_search.open || self.project_search.open)
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.close_search();
            return;
        }

        let shift = ctx.input(|input| input.modifiers.shift);

        if self.keymap.consume(ctx, KeyAction::ShortcutHelp) {
            self.shortcuts_open = !self.shortcuts_open;
            return;
        }
        if self.keymap.consume(ctx, KeyAction::OpenType) {
            self.open_type_palette();
            return;
        }
        if self.keymap.consume(ctx, KeyAction::QuickOpen) {
            self.palette.open(PaletteMode::Files);
            return;
        }
        if self.keymap.consume(ctx, KeyAction::FindInProject) {
            self.begin_project_search();
            return;
        }
        if self.keymap.consume(ctx, KeyAction::OpenFolder) {
            self.open_folder_dialog();
            return;
        }
        if self.keymap.consume(ctx, KeyAction::OpenFile) {
            self.open_file_dialog();
            return;
        }
        if self.keymap.consume(ctx, KeyAction::QuickOutline) {
            self.quick_outline_palette();
            return;
        }
        if self.keymap.consume(ctx, KeyAction::CommandPalette) {
            self.palette.open(PaletteMode::Commands);
            return;
        }
        if self.keymap.consume(ctx, KeyAction::ReplaceInFile) {
            self.begin_file_search(true);
            return;
        }
        if self.keymap.consume(ctx, KeyAction::FindInFile) {
            self.begin_file_search(false);
            return;
        }
        if self.keymap.consume(ctx, KeyAction::FindPrevious) {
            if self.file_search.open {
                self.file_search_previous();
            } else if self.project_search.open {
                self.project_search.previous();
            }
            return;
        }
        if self.keymap.consume(ctx, KeyAction::FindNext) {
            if self.file_search.open {
                if shift {
                    self.file_search_previous();
                } else {
                    self.file_search_next();
                }
            } else if self.project_search.open {
                if shift {
                    self.project_search.previous();
                } else {
                    self.project_search.next();
                }
            }
            return;
        }
        if self.keymap.consume(ctx, KeyAction::Save) {
            self.save_active();
        }
        if self.keymap.consume(ctx, KeyAction::CloseTab) {
            if let Some(active) = self.active_tab {
                self.request_close(active);
            }
        }
        if self.keymap.consume(ctx, KeyAction::Reindex) {
            self.reindex();
        }
        if self.keymap.consume(ctx, KeyAction::GoToDefinition) {
            self.goto_definition();
        }
    }

    fn top_bar(&mut self, ctx: &Context) {
        let k_open_folder = self.keymap.label(KeyAction::OpenFolder);
        let k_open_file = self.keymap.label(KeyAction::OpenFile);
        let k_save = self.keymap.label(KeyAction::Save);
        let k_close = self.keymap.label(KeyAction::CloseTab);
        let k_find = self.keymap.label(KeyAction::FindInFile);
        let k_replace = self.keymap.label(KeyAction::ReplaceInFile);
        let k_project_search = self.keymap.label(KeyAction::FindInProject);
        let k_next = self.keymap.label(KeyAction::FindNext);
        let k_reindex = self.keymap.label(KeyAction::Reindex);
        let k_quick_open = self.keymap.label(KeyAction::QuickOpen);
        let k_open_type = self.keymap.label(KeyAction::OpenType);
        let k_outline = self.keymap.label(KeyAction::QuickOutline);
        let k_goto = self.keymap.label(KeyAction::GoToDefinition);

        egui::TopBottomPanel::top("reclipse_menu")
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    ui.menu_button("Arquivo", |ui| {
                        if ui
                            .add(
                                egui::Button::new("Abrir pasta…")
                                    .shortcut_text(k_open_folder.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.open_folder_dialog();
                        }
                        if ui
                            .add(
                                egui::Button::new("Abrir arquivo…")
                                    .shortcut_text(k_open_file.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.open_file_dialog();
                        }
                        ui.separator();
                        if ui
                            .add(egui::Button::new("Salvar").shortcut_text(k_save.clone()))
                            .clicked()
                        {
                            ui.close_menu();
                            self.save_active();
                        }
                        if ui
                            .add(egui::Button::new("Fechar aba").shortcut_text(k_close.clone()))
                            .clicked()
                        {
                            ui.close_menu();
                            if let Some(active) = self.active_tab {
                                self.request_close(active);
                            }
                        }
                        ui.separator();
                        if ui.button("Sair").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });

                    ui.menu_button("Editar", |ui| {
                        if ui
                            .add(egui::Button::new("Desfazer").shortcut_text("Ctrl+Z"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.undo_active();
                        }
                        if ui
                            .add(egui::Button::new("Refazer").shortcut_text("Ctrl+Y"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.redo_active();
                        }
                        ui.separator();
                        if ui
                            .add(
                                egui::Button::new("Adicionar cursor abaixo")
                                    .shortcut_text("Ctrl+Alt+↓"),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.add_cursor_vertical(1);
                        }
                        if ui
                            .add(
                                egui::Button::new("Adicionar cursor acima")
                                    .shortcut_text("Ctrl+Alt+↑"),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.add_cursor_vertical(-1);
                        }
                        if ui
                            .add(egui::Button::new("Reduzir cursores").shortcut_text("Esc"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.clear_extra_cursors();
                        }
                        ui.separator();
                        if ui
                            .add(egui::Button::new("Localizar…").shortcut_text(k_find.clone()))
                            .clicked()
                        {
                            ui.close_menu();
                            self.begin_file_search(false);
                        }
                        if ui
                            .add(egui::Button::new("Substituir…").shortcut_text(k_replace.clone()))
                            .clicked()
                        {
                            ui.close_menu();
                            self.begin_file_search(true);
                        }
                        if ui
                            .add(
                                egui::Button::new("Buscar no projeto…")
                                    .shortcut_text(k_project_search.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.begin_project_search();
                        }
                        if ui
                            .add(
                                egui::Button::new("Próxima ocorrência")
                                    .shortcut_text(k_next.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            if self.file_search.open {
                                self.file_search_next();
                            } else if self.project_search.open {
                                self.project_search.next();
                            }
                        }
                    });

                    ui.menu_button("Navegar", |ui| {
                        if ui
                            .add(
                                egui::Button::new("Abrir recurso…")
                                    .shortcut_text(k_quick_open.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.palette.open(PaletteMode::Files);
                        }
                        if ui
                            .add(
                                egui::Button::new("Abrir tipo…").shortcut_text(k_open_type.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.open_type_palette();
                        }
                        if ui
                            .add(
                                egui::Button::new("Quick outline").shortcut_text(k_outline.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.quick_outline_palette();
                        }
                        ui.separator();
                        if ui
                            .add(
                                egui::Button::new("Ir para definição")
                                    .shortcut_text(k_goto.clone()),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.goto_definition();
                        }
                    });

                    ui.menu_button("Projeto", |ui| {
                        if ui
                            .add(egui::Button::new("Reindexar").shortcut_text(k_reindex.clone()))
                            .clicked()
                        {
                            ui.close_menu();
                            self.reindex();
                        }
                        if let Some(root) = &self.root {
                            ui.separator();
                            ui.label(
                                RichText::new(root.display().to_string())
                                    .small()
                                    .color(theme::MUTED),
                            );
                        }
                    });

                    ui.menu_button("Exibir", |ui| {
                        ui.checkbox(&mut self.show_project, "Projeto");
                        ui.checkbox(&mut self.show_outline, "Estrutura");
                        ui.checkbox(&mut self.show_problems, "Problemas");
                        ui.separator();
                        if ui.button("Aumentar fonte").clicked() {
                            ui.close_menu();
                            self.zoom_by(1.0);
                        }
                        if ui.button("Diminuir fonte").clicked() {
                            ui.close_menu();
                            self.zoom_by(-1.0);
                        }
                    });

                    ui.menu_button("Plugins", |ui| {
                        let plugins = self.plugins.plugins();
                        if plugins.is_empty() {
                            ui.label(
                                RichText::new("Nenhum plugin carregado")
                                    .small()
                                    .color(theme::MUTED),
                            );
                            ui.label(
                                RichText::new(
                                    "Coloque reclipse_plugin_*.dll ao lado do executável",
                                )
                                .small()
                                .color(theme::MUTED),
                            );
                        } else {
                            for plugin in &plugins {
                                ui.label(format!("{}  v{}", plugin.name, plugin.version));
                            }
                        }
                        ui.separator();
                        if ui.button("Recarregar plugins").clicked() {
                            ui.close_menu();
                            self.reload_plugins();
                        }
                    });

                    ui.menu_button("Ajuda", |ui| {
                        if ui.button("Atalhos do teclado").clicked() {
                            ui.close_menu();
                            self.shortcuts_open = true;
                        }
                        if !self.recovery_entries.is_empty()
                            && ui
                                .button(format!(
                                    "Recuperar buffers não salvos ({})…",
                                    self.recovery_entries.len()
                                ))
                                .clicked()
                        {
                            ui.close_menu();
                            self.recovery_open = true;
                        }
                        if ui.button("Abrir configuração (session.json)").clicked() {
                            ui.close_menu();
                            self.open_settings();
                        }
                        ui.separator();
                        if ui.button("Sobre o Reclipse").clicked() {
                            ui.close_menu();
                            self.about_open = true;
                        }
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("RECLIPSE")
                                .strong()
                                .color(theme::ACCENT)
                                .size(13.0),
                        );
                        ui.label(
                            RichText::new("Rust-native Java IDE")
                                .small()
                                .color(theme::MUTED),
                        );
                    });
                });
            });
    }

    fn status_bar(&mut self, ctx: &Context) {
        egui::TopBottomPanel::bottom("reclipse_status")
            .exact_height(27.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(toast) = &self.status_message {
                        ui.label(RichText::new(&toast.text).color(theme::ACCENT));
                    } else {
                        if let Some(project) = &self.project {
                            ui.label(RichText::new(&project.name).strong());
                            ui.separator();
                        }
                        self.index_status_ui(ui);
                        ui.separator();
                        self.classpath_status_ui(ui);
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(active) = self.active_tab {
                            if let Some(tab) = self.tabs.get(active) {
                                ui.label(format!("Ln {}, Col {}", tab.cursor_line, tab.cursor_col));
                                ui.separator();
                                ui.label(if is_java_path(&tab.doc.path) {
                                    "Java"
                                } else {
                                    "Texto"
                                });
                                ui.separator();
                                if tab.doc.dirty {
                                    ui.label(RichText::new("não salvo").color(theme::YELLOW));
                                    ui.separator();
                                }
                            }
                        }
                        ui.label(format!("{} plugin(s)", self.plugins.plugin_count()));
                        ui.separator();
                        ui.label("UTF-8");
                    });
                });
            });
    }

    fn classpath_status_ui(&self, ui: &mut Ui) {
        match &self.classpath_status {
            ClasspathStatus::Idle => {
                ui.label(RichText::new("cp: —").small().color(theme::MUTED));
            }
            ClasspathStatus::Indexing { done, total } => {
                ui.spinner();
                let label = if *total == 0 {
                    "classpath: resolvendo…".to_owned()
                } else {
                    format!("classpath: {done}/{total} archives")
                };
                ui.label(RichText::new(label).small().color(theme::MUTED));
            }
            ClasspathStatus::Ready(stats) => {
                ui.label(
                    RichText::new(format!(
                        "cp: {} archives · {} classes · {} ms",
                        stats.archives, stats.classes, stats.elapsed_ms
                    ))
                    .small()
                    .color(theme::MUTED),
                )
                .on_hover_text(
                    self.classpath_index
                        .as_ref()
                        .map(|index| format!("{} warning(s) de classpath", index.warnings.len()))
                        .unwrap_or_default(),
                );
            }
            ClasspathStatus::Failed(error) => {
                ui.label(RichText::new("classpath falhou").small().color(theme::RED))
                    .on_hover_text(error);
            }
        }
    }

    fn index_status_ui(&self, ui: &mut Ui) {
        match &self.index_status {
            IndexStatus::Idle => {
                ui.label(RichText::new("Pronto").color(theme::MUTED));
            }
            IndexStatus::Indexing { done, total } => {
                ui.spinner();
                if *total == 0 {
                    ui.label(RichText::new("Indexando…").color(theme::MUTED));
                } else {
                    ui.label(
                        RichText::new(format!("Indexando {done}/{total} arquivos Java"))
                            .color(theme::MUTED),
                    );
                }
            }
            IndexStatus::Ready {
                files,
                symbols,
                problems,
                elapsed_ms,
            } => {
                ui.label(
                    RichText::new(format!(
                        "{files} arquivos Java · {symbols} símbolos · {problems} problemas · {elapsed_ms} ms"
                    ))
                    .color(theme::MUTED),
                );
            }
            IndexStatus::Failed(error) => {
                ui.label(RichText::new("Falha na indexação").color(theme::RED))
                    .on_hover_text(error);
            }
        }
    }

    fn left_panel(&mut self, ctx: &Context) {
        if !(self.show_project || self.show_outline || self.show_problems) {
            return;
        }

        let mut actions = Vec::new();
        egui::SidePanel::left("reclipse_left")
            .resizable(true)
            .min_width(220.0)
            .max_width(520.0)
            .default_width(300.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.show_project {
                            self.project_section(ui, &mut actions);
                        }
                        if self.show_outline {
                            self.outline_section(ui, &mut actions);
                        }
                        if self.show_problems {
                            self.problems_section(ui, &mut actions);
                        }
                    });
            });

        for action in actions {
            self.execute_action(action);
        }
    }

    fn project_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "PROJETO");
        let Some(root) = self.root.clone() else {
            ui.label(RichText::new("Nenhum projeto aberto").color(theme::MUTED));
            if ui.button("Abrir pasta…").clicked() {
                actions.push(Action::OpenFolder);
            }
            return;
        };

        ui.horizontal(|ui| {
            let name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string());
            ui.label(RichText::new(name).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Abrir").clicked() {
                    actions.push(Action::OpenFolder);
                }
                if ui.small_button("Reindexar").clicked() {
                    actions.push(Action::Reindex);
                }
            });
        });

        if let Some(project) = &self.project {
            ui.label(
                RichText::new(format!(
                    "{} · {} arquivos Java",
                    project.build_system.label(),
                    project.java_files.len()
                ))
                .small()
                .color(theme::MUTED),
            );
        } else if matches!(self.index_status, IndexStatus::Indexing { .. }) {
            ui.label(
                RichText::new("descobrindo fontes…")
                    .small()
                    .color(theme::MUTED),
            );
        }

        if let Some(path) = self.tree.show(ui, &root) {
            actions.push(Action::OpenPath(path));
        }
    }

    fn outline_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "ESTRUTURA");
        let Some(active) = self.active_tab else {
            ui.label(RichText::new("Nenhum arquivo ativo").color(theme::MUTED));
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let Some(index) = &self.index else {
            ui.label(RichText::new("Índice em construção…").color(theme::MUTED));
            return;
        };
        let Some(file) = index.symbols_for(&path) else {
            ui.label(
                RichText::new("Este arquivo ainda não está no índice (salve-o)")
                    .small()
                    .color(theme::MUTED),
            );
            return;
        };
        let package = file.package.clone();
        let total_symbols = file.symbols.len();
        let symbols: Vec<_> = file.symbols.iter().take(1_000).cloned().collect();

        if let Some(package) = package {
            ui.label(RichText::new(package).small().color(theme::MUTED));
        }
        if symbols.is_empty() {
            ui.label(RichText::new("Nenhum símbolo encontrado").color(theme::MUTED));
            return;
        }

        for symbol in symbols {
            let color = match symbol.kind {
                SymbolKind::Class
                | SymbolKind::Interface
                | SymbolKind::Enum
                | SymbolKind::Record
                | SymbolKind::Annotation => theme::CLR_TYPE,
                SymbolKind::Method | SymbolKind::Constructor => theme::YELLOW,
                _ => theme::MUTED,
            };
            ui.horizontal(|ui| {
                ui.add_space((symbol.depth as f32) * 12.0);
                ui.label(
                    RichText::new(symbol.kind.glyph())
                        .monospace()
                        .color(color)
                        .size(11.0),
                );
                let response =
                    ui.selectable_label(false, RichText::new(&symbol.name).color(color).size(13.0));
                if response.clicked() {
                    actions.push(Action::OpenAt(path.clone(), symbol.line, symbol.column));
                }
                if let Some(detail) = &symbol.detail {
                    response.on_hover_text(detail);
                }
            });
        }
        if total_symbols > 1_000 {
            ui.label(
                RichText::new(format!("mostrando 1.000 de {total_symbols} símbolos"))
                    .small()
                    .color(theme::MUTED),
            );
        }
    }

    fn problems_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "PROBLEMAS");
        let Some(index) = &self.index else {
            ui.label(RichText::new("Sem dados de indexação").color(theme::MUTED));
            return;
        };
        let total = index.problems.len();
        let problems: Vec<IndexedProblem> = index.problems.iter().take(300).cloned().collect();
        let semantic_total = self.semantic_problems.len();
        let semantic: Vec<SemanticProblem> =
            self.semantic_problems.iter().take(300).cloned().collect();
        let root = self.root.clone();

        ui.label(
            RichText::new(format!("{total} problema(s) de sintaxe"))
                .small()
                .color(if total == 0 { theme::GREEN } else { theme::RED }),
        );
        if total == 0 && semantic_total == 0 {
            return;
        }

        for problem in problems {
            let relative = root
                .as_ref()
                .map(|root| {
                    problem
                        .file
                        .strip_prefix(root)
                        .unwrap_or(&problem.file)
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| problem.file.display().to_string());
            let label = format!(
                "{}:{}:{}  {}",
                short_file(&relative),
                problem.line,
                problem.column,
                problem.message
            );
            if ui
                .selectable_label(false, RichText::new(label).size(12.0).color(theme::RED))
                .clicked()
            {
                actions.push(Action::OpenAt(problem.file, problem.line, problem.column));
            }
        }

        if semantic_total > 0 {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{semantic_total} import(s) não resolvido(s)"))
                    .small()
                    .color(theme::YELLOW),
            );
            for problem in semantic {
                let relative = root
                    .as_ref()
                    .map(|root| {
                        problem
                            .file
                            .strip_prefix(root)
                            .unwrap_or(&problem.file)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|| problem.file.display().to_string());
                let label = format!(
                    "{}:{}  {}",
                    short_file(&relative),
                    problem.line,
                    problem.message
                );
                if ui
                    .selectable_label(false, RichText::new(label).size(12.0).color(theme::YELLOW))
                    .clicked()
                {
                    actions.push(Action::OpenAt(problem.file, problem.line, 1));
                }
            }
        }
    }

    fn central_panel(&mut self, ctx: &Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::EDITOR_BG))
            .show(ctx, |ui| {
                self.tab_bar(ui);
                ui.separator();
                if let Some(active) = self.active_tab {
                    if let Some(tab) = self.tabs.get_mut(active) {
                        let font_size = self.font_size;
                        tab.show(ui, font_size);
                    }
                } else {
                    self.welcome(ui);
                }
            });
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut activate = None;
        let mut close = None;
        let mut open_folder = false;

        egui::Frame::NONE
            .fill(theme::PANEL)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("reclipse_tabs")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if self.tabs.is_empty() {
                                ui.label(
                                    RichText::new("RECLIPSE")
                                        .small()
                                        .strong()
                                        .color(theme::MUTED),
                                );
                                if ui.small_button("Abrir projeto…").clicked() {
                                    open_folder = true;
                                }
                                return;
                            }
                            for (index, tab) in self.tabs.iter().enumerate() {
                                let title = format!(
                                    "{}{}",
                                    if tab.doc.dirty { "* " } else { "" },
                                    tab.title()
                                );
                                let is_active = self.active_tab == Some(index);
                                let response =
                                    ui.selectable_label(is_active, RichText::new(title).size(13.0));
                                if response.clicked() {
                                    activate = Some(index);
                                }
                                response.on_hover_text(tab.doc.path.display().to_string());
                                if ui.small_button("x").clicked() {
                                    close = Some(index);
                                }
                                ui.add_space(2.0);
                            }
                        });
                    });
            });

        if open_folder {
            self.open_folder_dialog();
        }
        if let Some(index) = activate {
            self.active_tab = Some(index);
        }
        if let Some(index) = close {
            self.request_close(index);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            ui.label(
                RichText::new("RECLIPSE")
                    .size(42.0)
                    .strong()
                    .color(theme::ACCENT),
            );
            ui.label(
                RichText::new("A IDE Java nativa em Rust")
                    .size(15.0)
                    .color(theme::MUTED),
            );
            ui.add_space(26.0);

            ui.horizontal(|ui| {
                if ui.button("Abrir projeto…").clicked() {
                    self.open_folder_dialog();
                }
                if ui.button("Abrir arquivo…").clicked() {
                    self.open_file_dialog();
                }
                if self.project.is_some() && ui.button("Quick open (Ctrl+P)").clicked() {
                    self.palette.open(PaletteMode::Files);
                }
            });

            ui.add_space(30.0);
            if let Some(project) = &self.project {
                ui.label(
                    RichText::new(format!("{} · {}", project.name, project.build_system.label()))
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new(format!(
                        "{} fontes Java · {} arquivos no projeto",
                        project.java_files.len(),
                        project.all_files.len()
                    ))
                    .small()
                    .color(theme::MUTED),
                );
            } else if matches!(self.index_status, IndexStatus::Indexing { .. }) {
                ui.spinner();
                ui.label(RichText::new("Indexando o projeto…").color(theme::MUTED));
            } else {
                ui.label(
                    RichText::new("Abra uma pasta com um projeto Java para começar")
                        .color(theme::MUTED),
                );
            }

            ui.add_space(36.0);
            ui.separator();
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "Ctrl+Shift+R  abrir recurso     Ctrl+3  quick access     F3  definição     F5  reindexar",
                )
                .small()
                .color(theme::MUTED),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("MVP nativo: tree-sitter-java · egui · plugins Rust ABI v1")
                    .small()
                    .color(theme::MUTED),
            );
        });
    }

    fn show_palette(&mut self, ctx: &Context) {
        if !self.palette.open {
            return;
        }
        let mode = self.palette.mode.unwrap_or(PaletteMode::Commands);
        let title = match mode {
            PaletteMode::Commands => "Quick Access",
            PaletteMode::Files => "Open Resource",
            PaletteMode::Types => "Open Type",
            PaletteMode::Outline => "Quick Outline",
        };
        let mut chosen: Option<Action> = None;

        egui::Window::new(title)
            .id(egui::Id::new("reclipse_palette"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 72.0))
            .show(ctx, |ui| {
                ui.set_width(680.0);
                let escape = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
                if escape {
                    chosen = None;
                    self.palette.close();
                    return;
                }
                let down = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown));
                let up = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp));

                let query_id = egui::Id::new("reclipse_palette_query");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.palette.query)
                        .id(query_id)
                        .hint_text("Digite para filtrar…")
                        .desired_width(f32::INFINITY),
                );
                if self.palette.focus_pending {
                    response.request_focus();
                    if response.has_focus() {
                        self.palette.focus_pending = false;
                    }
                }

                let entries = self.palette_entries();
                let filtered = rank(entries, &self.palette.query, 200);
                if filtered.is_empty() {
                    self.palette.selected = 0;
                } else {
                    if down {
                        self.palette.selected = (self.palette.selected + 1).min(filtered.len() - 1);
                    }
                    if up {
                        self.palette.selected = self.palette.selected.saturating_sub(1);
                    }
                    self.palette.selected = self.palette.selected.min(filtered.len() - 1);
                }

                let enter = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
                if enter {
                    if let Some(entry) = filtered.get(self.palette.selected) {
                        chosen = Some(entry.action.clone());
                    }
                }

                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if filtered.is_empty() {
                            ui.label(RichText::new("Nenhum resultado").color(theme::MUTED));
                        }
                        for (index, entry) in filtered.iter().enumerate() {
                            let selected = index == self.palette.selected;
                            let label = if entry.hint.is_empty() {
                                entry.label.clone()
                            } else {
                                format!("{}    {}", entry.label, entry.hint)
                            };
                            let response =
                                ui.selectable_label(selected, RichText::new(label).size(13.5));
                            if response.clicked() {
                                chosen = Some(entry.action.clone());
                            }
                        }
                    });
                ui.separator();
                ui.label(
                    RichText::new("Setas: navegar   Enter: abrir   Esc: fechar")
                        .small()
                        .color(theme::MUTED),
                );
            });

        if let Some(action) = chosen {
            self.palette.close();
            self.execute_action(action);
        }
    }

    fn show_classpath_view(&mut self, ctx: &Context) {
        let Some(name) = self.classpath_view.clone() else {
            return;
        };
        let Some(class) = self
            .classpath_index
            .as_ref()
            .and_then(|index| index.get(&name))
            .cloned()
        else {
            self.classpath_view = None;
            return;
        };
        let mut open = true;
        egui::Window::new(format!("Tipo externo — {}", class.display_name))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(680.0);
                ui.label(
                    RichText::new(&class.name)
                        .monospace()
                        .strong()
                        .color(theme::ACCENT),
                );
                ui.label(
                    RichText::new(format!(
                        "{} · {} · origem: {}",
                        class.kind.label(),
                        class.package,
                        class.origin
                    ))
                    .small()
                    .color(theme::MUTED),
                );
                if let Some(super_name) = &class.super_name {
                    ui.label(
                        RichText::new(format!("extends {super_name}"))
                            .small()
                            .color(theme::MUTED),
                    );
                }
                if !class.interfaces.is_empty() {
                    ui.label(
                        RichText::new(format!("implements {}", class.interfaces.join(", ")))
                            .small()
                            .color(theme::MUTED),
                    );
                }
                ui.separator();
                ui.label(
                    RichText::new("Metadados do classpath; código-fonte não disponível.")
                        .small()
                        .color(theme::YELLOW),
                );
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.collapsing(format!("Campos ({})", class.fields.len()), |ui| {
                            for field in class.fields.iter().take(500) {
                                ui.label(
                                    RichText::new(format!(
                                        "{}{}",
                                        if field.is_static { "static " } else { "" },
                                        field.signature
                                    ))
                                    .monospace()
                                    .size(12.0),
                                );
                            }
                        });
                        ui.collapsing(format!("Métodos ({})", class.methods.len()), |ui| {
                            for method in class.methods.iter().take(500) {
                                ui.label(
                                    RichText::new(format!(
                                        "{}{}",
                                        if method.is_static { "static " } else { "" },
                                        method.signature
                                    ))
                                    .monospace()
                                    .size(12.0),
                                );
                            }
                        });
                    });
            });
        if !open {
            self.classpath_view = None;
        }
    }

    fn show_shortcuts(&mut self, ctx: &Context) {
        if !self.shortcuts_open {
            return;
        }
        let mut open = true;
        let items = [
            ("Abrir recurso", KeyAction::QuickOpen),
            ("Abrir tipo", KeyAction::OpenType),
            ("Quick outline", KeyAction::QuickOutline),
            ("Quick access (comandos)", KeyAction::CommandPalette),
            ("Abrir arquivo", KeyAction::OpenFile),
            ("Abrir pasta", KeyAction::OpenFolder),
            ("Salvar", KeyAction::Save),
            ("Fechar aba", KeyAction::CloseTab),
            ("Localizar no arquivo", KeyAction::FindInFile),
            ("Substituir no arquivo", KeyAction::ReplaceInFile),
            ("Buscar no projeto", KeyAction::FindInProject),
            ("Ir para definição", KeyAction::GoToDefinition),
            ("Reindexar", KeyAction::Reindex),
            ("Próxima ocorrência", KeyAction::FindNext),
            ("Ocorrência anterior", KeyAction::FindPrevious),
        ];

        egui::Window::new("Atalhos do teclado — padrão Eclipse")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(
                    RichText::new(
                        "Atalhos de navegação e busca seguem o padrão do Eclipse. \
                         Edição de texto usa as teclas convencionais.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.separator();
                egui::Grid::new("reclipse_shortcut_grid")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (label, action) in items {
                            ui.label(label);
                            ui.label(
                                RichText::new(self.keymap.label(action))
                                    .monospace()
                                    .color(theme::ACCENT),
                            );
                            ui.end_row();
                        }
                    });
                ui.separator();
                ui.label(
                    RichText::new(
                        "Editor: Ctrl+Z desfazer · Ctrl+Y refazer · Ctrl+A selecionar tudo · \
                         Ctrl+Alt+↓ adicionar cursor · Alt+arrastar seleção por coluna · Esc reduzir cursores",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.label(
                    RichText::new(
                        "Folding: Ctrl+Shift+[ dobrar · Ctrl+Shift+] desdobrar · Ctrl+Shift+0/9 recolher/expandir tudo",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.label(
                    RichText::new(
                        "Busca: Enter próximo · Shift+Enter anterior · Ctrl+F localizar · Ctrl+H substituir",
                    )
                    .small()
                    .color(theme::MUTED),
                );
            });
        self.shortcuts_open = open;
    }

    fn show_about(&mut self, ctx: &Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Sobre o Reclipse")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                ui.label(
                    RichText::new("RECLIPSE 0.1.0")
                        .size(24.0)
                        .strong()
                        .color(theme::ACCENT),
                );
                ui.label(
                    RichText::new("Uma IDE Java nativa, rápida e extensível em Rust.")
                        .color(theme::TEXT),
                );
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label("UI: egui / eframe (GPU)");
                ui.label("Parser e índice: tree-sitter-java + Rayon");
                ui.label("Plugins: ABI C v1 com comandos JSON");
                ui.label("Objetivo: cold start < 150 ms e memória < 300 MB");
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Compatibilidade Eclipse (equinox/JDT headless) está no roadmap. \
                         Este MVP não carrega bundles OSGi nem plugins SWT.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Projeto independente; não afiliado à Eclipse Foundation.")
                        .small()
                        .color(theme::MUTED),
                );
            });
        self.about_open = open;
    }

    fn show_close_confirm(&mut self, ctx: &Context) {
        let Some(index) = self.close_confirm else {
            return;
        };
        if index >= self.tabs.len() {
            self.close_confirm = None;
            return;
        }
        let title = self.tabs[index].title();
        let mut open = true;
        let mut save = false;
        let mut discard = false;
        let mut cancel = false;

        egui::Window::new("Fechar arquivo")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(format!(
                    "“{title}” tem alterações não salvas. O que deseja fazer?"
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Salvar e fechar").clicked() {
                        save = true;
                    }
                    if ui.button("Descartar").clicked() {
                        discard = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });

        if !open || cancel {
            self.close_confirm = None;
        } else if save {
            if self.save_tab(index) {
                self.force_close(index);
            }
            self.close_confirm = None;
        } else if discard {
            self.force_close(index);
            self.close_confirm = None;
        }
    }

    fn execute_action(&mut self, action: Action) {
        match action {
            Action::OpenFolder => self.open_folder_dialog(),
            Action::QuickOpen => self.palette.open(PaletteMode::Files),
            Action::OpenTypes => self.open_type_palette(),
            Action::QuickOutline => self.quick_outline_palette(),
            Action::ShowShortcuts => self.shortcuts_open = true,
            Action::OpenFile => self.open_file_dialog(),
            Action::Save => {
                if !self.save_active() {
                    self.toast("Nenhum arquivo ativo para salvar");
                }
            }
            Action::CloseTab => {
                if let Some(active) = self.active_tab {
                    self.request_close(active);
                }
            }
            Action::Reindex => self.reindex(),
            Action::ToggleProject => self.show_project = !self.show_project,
            Action::ToggleOutline => self.show_outline = !self.show_outline,
            Action::ToggleProblems => self.show_problems = !self.show_problems,
            Action::GoToDefinition => self.goto_definition(),
            Action::FindInFile => self.begin_file_search(false),
            Action::ReplaceInFile => self.begin_file_search(true),
            Action::FindInProject => self.begin_project_search(),
            Action::FindNext => {
                if self.file_search.open {
                    self.file_search_next();
                } else if self.project_search.open {
                    self.project_search.next();
                    let index = self.project_search.selected;
                    self.open_project_match(index);
                }
            }
            Action::FindPrevious => {
                if self.file_search.open {
                    self.file_search_previous();
                } else if self.project_search.open {
                    self.project_search.previous();
                    let index = self.project_search.selected;
                    self.open_project_match(index);
                }
            }
            Action::Undo => self.undo_active(),
            Action::Redo => self.redo_active(),
            Action::AddCursorBelow => self.add_cursor_vertical(1),
            Action::AddCursorAbove => self.add_cursor_vertical(-1),
            Action::ClearExtraCursors => self.clear_extra_cursors(),
            Action::OpenSettings => self.open_settings(),
            Action::ZoomIn => self.zoom_by(1.0),
            Action::ZoomOut => self.zoom_by(-1.0),
            Action::ReloadPlugins => self.reload_plugins(),
            Action::About => self.about_open = true,
            Action::OpenPath(path) => self.open_file(path),
            Action::OpenAt(path, line, column) => self.open_at(path, line, column),
            Action::OpenClasspathType(name) => self.classpath_view = Some(name),
            Action::Plugin {
                plugin_index,
                command_id,
                title,
            } => self.run_plugin(plugin_index, command_id, title),
        }
    }

    /// Optional CI/development screenshot: set `RECLIPSE_SCREENSHOT_TO` to a
    /// `.png` path. The build must enable the `screenshot` feature.
    fn run_frame_bench(&mut self, ctx: &Context, frame_start: Instant) {
        let Some(bench) = &self.frame_bench else {
            return;
        };
        if !matches!(self.index_status, IndexStatus::Ready { .. }) || self.active_tab.is_none() {
            ctx.request_repaint_after(Duration::from_millis(10));
            return;
        }
        let Some(active) = self.active_tab else {
            return;
        };
        let target = bench.target;
        let warmup = bench.warmup;
        let started = bench.started;
        let frames = bench.frames;

        let elapsed_ms = frame_start.elapsed().as_secs_f64() * 1_000.0;
        let now = Instant::now();
        if !started {
            let cursor = self.tabs[active].doc.len_chars() / 2;
            self.tabs[active].doc.set_cursor(cursor, false);
            if let Some(bench) = &mut self.frame_bench {
                bench.cursor = cursor;
                bench.started = true;
                bench.last_end = Some(now);
            }
            ctx.request_repaint_after(Duration::from_millis(1));
            return;
        }

        if frames >= warmup {
            let wall_ms = self
                .frame_bench
                .as_ref()
                .and_then(|bench| bench.last_end)
                .map(|last| now.duration_since(last).as_secs_f64() * 1_000.0)
                .unwrap_or(elapsed_ms);
            if let Some(bench) = &mut self.frame_bench {
                bench.samples.push(elapsed_ms);
                bench.wall_samples.push(wall_ms);
            }
        }
        let samples_len = self
            .frame_bench
            .as_ref()
            .map(|bench| bench.samples.len())
            .unwrap_or(0);
        if samples_len >= target {
            let Some(mut bench) = self.frame_bench.take() else {
                return;
            };
            bench.samples.sort_by(|left, right| left.total_cmp(right));
            bench
                .wall_samples
                .sort_by(|left, right| left.total_cmp(right));
            if bench.samples.is_empty() {
                return;
            }
            let percentile = |samples: &[f64], fraction: f64| {
                if samples.is_empty() {
                    return 0.0;
                }
                let index = ((samples.len() - 1) as f64 * fraction).round() as usize;
                samples[index]
            };
            let cpu_p50 = percentile(&bench.samples, 0.50);
            let cpu_p95 = percentile(&bench.samples, 0.95);
            let cpu_p99 = percentile(&bench.samples, 0.99);
            let wall_p50 = percentile(&bench.wall_samples, 0.50);
            let wall_p95 = percentile(&bench.wall_samples, 0.95);
            let wall_p99 = percentile(&bench.wall_samples, 0.99);
            eprintln!(
                "[reclipse] frame_bench samples={} cpu_p50={cpu_p50:.2}ms cpu_p95={cpu_p95:.2}ms cpu_p99={cpu_p99:.2}ms wall_p50={wall_p50:.2}ms wall_p95={wall_p95:.2}ms wall_p99={wall_p99:.2}ms",
                bench.samples.len()
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let cursor = self
            .frame_bench
            .as_ref()
            .map(|bench| bench.cursor)
            .unwrap_or(0)
            .min(self.tabs[active].doc.len_chars());
        self.tabs[active].doc.set_cursor(cursor, false);
        self.tabs[active].doc.insert("x");
        if let Some(bench) = &mut self.frame_bench {
            bench.cursor = cursor + 1;
            bench.frames += 1;
            bench.last_end = Some(Instant::now());
        }
        ctx.request_repaint_after(Duration::from_millis(1));
    }

    fn maybe_capture_screenshot(&mut self, ctx: &Context) {
        let Some(path) = self.screenshot.path.clone() else {
            return;
        };

        #[cfg(feature = "screenshot")]
        {
            let image = ctx.input(|input| {
                input.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });

            if let Some(image) = image {
                let [width, height] = image.size;
                let mut rgba = Vec::with_capacity(width * height * 4);
                for pixel in &image.pixels {
                    rgba.extend_from_slice(&pixel.to_array());
                }
                match ::image::save_buffer(
                    &path,
                    &rgba,
                    width as u32,
                    height as u32,
                    ::image::ColorType::Rgba8,
                ) {
                    Ok(()) => eprintln!("[reclipse] screenshot saved to {}", path.display()),
                    Err(error) => eprintln!("[reclipse] screenshot failed: {error}"),
                }
                self.screenshot.path = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            if !self.screenshot.requested {
                if matches!(self.index_status, IndexStatus::Ready { .. }) {
                    self.screenshot.ready_frames += 1;
                    if let Some(name) = self.screenshot.classpath_view.clone() {
                        if self.classpath_view.is_none()
                            && self
                                .classpath_index
                                .as_ref()
                                .is_some_and(|index| index.contains(&name))
                        {
                            self.classpath_view = Some(name);
                        }
                    }
                    if self.screenshot.ready_frames == 5 && self.screenshot.open_type {
                        self.palette.open(PaletteMode::Types);
                        if let Some(query) = self.screenshot.palette_query.clone() {
                            self.palette.query = query;
                        }
                    }
                    if self.screenshot.ready_frames == 5 && self.screenshot.open_palette {
                        self.palette.open(PaletteMode::Commands);
                        if let Some(query) = self.screenshot.palette_query.clone() {
                            self.palette.query = query;
                        }
                    }
                    if self.screenshot.ready_frames == 5 {
                        if let Some(query) = self.screenshot.search_query.clone() {
                            self.file_search.query = query;
                            self.file_search.open(false);
                            self.sync_file_search();
                            self.select_file_match(false);
                        }
                        if let Some(query) = self.screenshot.project_search_query.clone() {
                            self.project_search.query = query;
                            self.project_search.open();
                            self.run_project_search();
                        }
                    }
                    // Give the palette (and any focus/fade animation) time to
                    // finish before the framebuffer capture.
                    if self.screenshot.ready_frames >= self.screenshot.capture_after_frames {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                            egui::UserData::default(),
                        ));
                        self.screenshot.requested = true;
                    }
                }
                ctx.request_repaint_after(Duration::from_millis(60));
            }
        }

        #[cfg(not(feature = "screenshot"))]
        {
            let _ = (ctx, path);
        }
    }
}

impl eframe::App for ReclipseApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        if self
            .status_message
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.until)
        {
            self.status_message = None;
        }

        self.poll_index();
        self.poll_classpath();
        self.poll_watcher();
        if self.project_search.poll() {
            let count = self.project_search.results.len();
            if count > 0 {
                self.toast(format!("{count} resultado(s) no projeto"));
            } else {
                self.toast("Nenhum resultado no projeto");
            }
        }
        self.sync_file_search();
        self.handle_shortcuts(ctx);
        self.top_bar(ctx);
        self.status_bar(ctx);
        self.external_change_bar(ctx);
        self.search_panel(ctx);
        self.left_panel(ctx);
        self.central_panel(ctx);
        self.show_palette(ctx);
        self.show_shortcuts(ctx);
        self.show_classpath_view(ctx);
        self.show_about(ctx);
        self.show_project_replace_confirm(ctx);
        self.show_recovery(ctx);
        self.show_close_confirm(ctx);
        self.persist_dirty_backups();
        self.run_frame_bench(ctx, frame_start);
        self.maybe_capture_screenshot(ctx);

        if matches!(self.index_status, IndexStatus::Indexing { .. })
            || matches!(self.classpath_status, ClasspathStatus::Indexing { .. })
            || self.project_search.running
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        } else if self.status_message.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [red, green, blue, alpha] = theme::BG.to_array();
        [
            red as f32 / 255.0,
            green as f32 / 255.0,
            blue as f32 / 255.0,
            alpha as f32 / 255.0,
        ]
    }
}

impl Drop for ReclipseApp {
    fn drop(&mut self) {
        let mut session = self.session_snapshot();
        // Preserve keybindings edited externally while the app was running.
        if let Some(keybindings) = crate::session::load_keybindings() {
            session.keybindings = keybindings;
        }
        if let Err(error) = crate::session::save(&session) {
            eprintln!("[reclipse] failed to save session: {error:#}");
        }
    }
}

fn section_header(ui: &mut Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).small().strong().color(theme::MUTED));
    ui.separator();
}

fn short_file(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn is_java_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("java"))
        .unwrap_or(false)
}

fn path_is_ignored(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|name| {
                IGNORED_DIRS
                    .iter()
                    .any(|ignored| name.eq_ignore_ascii_case(ignored))
            })
            .unwrap_or(false)
    })
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn compute_semantic_problems(
    index: Option<&ProjectIndex>,
    project_types: Option<&ProjectTypes>,
    classpath: &ClasspathIndex,
) -> Vec<SemanticProblem> {
    let (Some(index), Some(project_types)) = (index, project_types) else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for (path, file) in &index.files {
        for (import, line) in unresolved_imports(file, project_types, classpath) {
            problems.push(SemanticProblem {
                file: path.clone(),
                line,
                message: format!("import não resolvido: {import}"),
            });
            if problems.len() >= 2_000 {
                return problems;
            }
        }
    }
    problems
}

fn spawn_classpath(project: Project) -> Receiver<ClasspathMessage> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<ClasspathIndex> {
            let resolution = reclipse_classpath::resolve(&project)?;
            let progress_sender = sender.clone();
            let progress = move |done: usize, total: usize| {
                let _ = progress_sender.send(ClasspathMessage::Progress { done, total });
            };
            Ok(reclipse_classpath::build_index(
                &resolution,
                Some(&progress),
            ))
        })();
        match result {
            Ok(index) => {
                let _ = sender.send(ClasspathMessage::Ready(Box::new(index)));
            }
            Err(error) => {
                let _ = sender.send(ClasspathMessage::Failed(format!("{error:#}")));
            }
        }
    });
    receiver
}

fn spawn_index(root: PathBuf) -> Receiver<IndexMessage> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<IndexPayload> {
            let project = discover_project(&root)?;
            let progress_sender = sender.clone();
            let progress = move |done: usize, total: usize| {
                if done == total || done.is_multiple_of(16) {
                    let _ = progress_sender.send(IndexMessage::Progress { done, total });
                }
            };
            let index = build_index(project.clone(), Some(&progress));
            Ok(IndexPayload { project, index })
        })();

        match result {
            Ok(payload) => {
                let _ = sender.send(IndexMessage::Done(Box::new(payload)));
            }
            Err(error) => {
                let _ = sender.send(IndexMessage::Failed(format!("{error:#}")));
            }
        }
    });
    receiver
}
