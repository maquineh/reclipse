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
use reclipse_java::refactor::{
    apply_text_edits, local_binding_at, plan_local_rename, plan_member_rename, plan_type_rename,
    LocalRenamePlan, RenameKind, RenamePlan,
};
use reclipse_java::semantic::{
    completion_context, find_member_references, find_type_references, infer_variable_type,
    member_target_at, resolve_type as resolve_semantic_type, unresolved_imports, CompletionItem,
    CompletionKind, MemberTarget, ProjectTypes, ResolvedType,
};
use reclipse_java::{build_index, IndexedProblem, ProjectIndex, SymbolKind};
use reclipse_plugin_api::InvokeContext;
use reclipse_plugin_host::{PluginCommand, PluginHost};

use crate::editor_view::Tab;
use crate::keymap::{KeyAction, Keymap};
use crate::palette::{rank, Action, Palette, PaletteEntry, PaletteMode};
use crate::search::{project_files, FileSearch, ProjectMatch, ProjectSearch};
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

struct CompletionState {
    path: PathBuf,
    start: usize,
    items: Vec<CompletionItem>,
    selected: usize,
    anchor: egui::Pos2,
    receiver: Option<String>,
}

enum RenameTarget {
    Type(String),
    Member(MemberTarget),
    Local { path: PathBuf, cursor: usize },
}

impl Clone for RenameTarget {
    fn clone(&self) -> Self {
        match self {
            Self::Type(fqn) => Self::Type(fqn.clone()),
            Self::Member(target) => Self::Member(target.clone()),
            Self::Local { path, cursor } => Self::Local {
                path: path.clone(),
                cursor: *cursor,
            },
        }
    }
}

struct RenameState {
    target: RenameTarget,
    old_name: String,
    new_name: String,
    kind: RenameKind,
    preview: Option<usize>,
    focus_pending: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleTab {
    Output,
    Rust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

struct LogLine {
    at: Duration,
    level: LogLevel,
    text: String,
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
    completion_trigger: Option<String>,
    completion_done: bool,
    hover_word: Option<String>,
    source_class: Option<String>,
    references_word: Option<String>,
    references_done: bool,
    rename_word: Option<String>,
    rename_new_name: Option<String>,
    rename_done: bool,
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
    classpath_source: Option<(String, String)>,
    classpath_reload_at: Option<Instant>,
    completion: Option<CompletionState>,
    last_completion_revision: Option<(PathBuf, u64)>,
    rename: Option<RenameState>,
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
    show_console: bool,
    console_tab: ConsoleTab,
    console: Vec<LogLine>,
    app_started_at: Instant,
    plugins: PluginHost,
    plugin_commands: Vec<PluginCommand>,
    status_message: Option<Toast>,
    about_open: bool,
    shortcuts_open: bool,
    project_replace_confirm: bool,
    close_confirm: Option<usize>,
    font_size: f32,
    theme: crate::theme::ThemeKind,
    theme_label: String,
    recovery_entries: Vec<crate::recovery::RecoveryEntry>,
    recovery_open: bool,
    backup_state: HashMap<PathBuf, (u64, Instant)>,
    frame_bench: Option<FrameBench>,
    screenshot: ScreenshotState,
}

impl ReclipseApp {
    pub fn new(cc: &CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let session = crate::session::load();
        let theme = std::env::var("RECLIPSE_THEME")
            .ok()
            .map(|name| crate::theme::ThemeKind::from_name(&name))
            .unwrap_or_else(|| crate::theme::ThemeKind::from_name(session.theme.as_str()));
        crate::theme::configure(&cc.egui_ctx, theme);

        let mut plugins = PluginHost::new();
        plugins.load_default_paths();
        let plugin_commands = plugins.commands();
        let recovery_entries = crate::recovery::list();
        let recovery_open = !recovery_entries.is_empty();
        let screenshot_mode = std::env::var_os("RECLIPSE_SCREENSHOT_TO").is_some();

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
            classpath_source: None,
            classpath_reload_at: None,
            completion: None,
            last_completion_revision: None,
            rename: None,
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
            show_project: session.show_project || screenshot_mode,
            show_outline: session.show_outline || screenshot_mode,
            show_problems: session.show_problems || screenshot_mode,
            show_console: session.show_console || screenshot_mode,
            console_tab: ConsoleTab::Output,
            console: Vec::new(),
            app_started_at: Instant::now(),
            plugins,
            plugin_commands,
            status_message: None,
            about_open: false,
            shortcuts_open: false,
            project_replace_confirm: false,
            close_confirm: None,
            font_size: session.font_size.clamp(10.0, 28.0),
            theme,
            theme_label: theme.short_label().to_owned(),
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
                completion_trigger: std::env::var("RECLIPSE_SCREENSHOT_COMPLETION").ok(),
                completion_done: false,
                hover_word: std::env::var("RECLIPSE_SCREENSHOT_HOVER").ok(),
                source_class: std::env::var("RECLIPSE_SCREENSHOT_SOURCE").ok(),
                references_word: std::env::var("RECLIPSE_SCREENSHOT_REFERENCES").ok(),
                references_done: false,
                rename_word: std::env::var("RECLIPSE_SCREENSHOT_RENAME").ok(),
                rename_new_name: std::env::var("RECLIPSE_SCREENSHOT_RENAME_NEW").ok(),
                rename_done: false,
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
        self.classpath_source = None;
        self.classpath_reload_at = None;
        self.completion = None;
        self.last_completion_revision = None;
        self.tabs.clear();
        self.active_tab = None;
        self.file_search.close();
        self.project_search.close();
        self.watch_ignored.clear();
        self.tree = FileTree::default();
        self.index_status = IndexStatus::Indexing { done: 0, total: 0 };
        self.index_rx = Some(spawn_index(root.clone()));
        self.start_watcher(&root);
        self.log(
            LogLevel::Info,
            format!("Abrindo projeto: {}", root.display()),
        );
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
                    self.log(
                        LogLevel::Success,
                        format!(
                            "Rust core: {} arquivos Java, {} símbolos em {} ms ({} problema(s))",
                            stats.java_files, stats.symbols, stats.elapsed_ms, stats.problems
                        ),
                    );

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

            if is_classpath_config(&canonical) {
                self.classpath_reload_at = Some(now + Duration::from_millis(500));
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
                    .fill(tint(theme::yellow(), 28))
                    .inner_margin(egui::Margin::symmetric(10, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("ARQUIVO ALTERADO NO DISCO")
                            .small()
                            .strong()
                            .color(theme::yellow()),
                    );
                    ui.label(RichText::new(message).small().color(theme::text()));
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
        self.log(LogLevel::Info, "Resolvendo classpath e indexando JARs/JDK…");
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
                    self.classpath_status = ClasspathStatus::Ready(stats.clone());
                    self.classpath_index = Some(*index);
                    self.classpath_rx = None;
                    self.log(
                        LogLevel::Success,
                        format!(
                            "Classpath: {} archives, {} classes em {} ms{}",
                            stats.archives,
                            stats.classes,
                            stats.elapsed_ms,
                            stats
                                .jdk_target
                                .map(|target| format!(" · Java {target}"))
                                .unwrap_or_default()
                        ),
                    );
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

    fn maybe_reload_classpath(&mut self) {
        let Some(at) = self.classpath_reload_at else {
            return;
        };
        if Instant::now() < at || matches!(self.classpath_status, ClasspathStatus::Indexing { .. })
        {
            return;
        }
        self.classpath_reload_at = None;
        if let Some(project) = self.project.clone() {
            self.start_classpath(project);
            self.toast("Classpath recarregado");
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
        let (word, start, cursor, path, text, file) = {
            let tab = &self.tabs[active];
            let Some((word, start, _)) = tab.doc.identifier_at_cursor() else {
                self.toast("Coloque o cursor sobre um identificador");
                return;
            };
            let path = tab.doc.path.clone();
            let text = tab.doc.text();
            let file = reclipse_java::parse_source(&path, &text);
            (word, start, tab.doc.cursor_char, path, text, file)
        };

        if let Some((local, declaration)) = local_binding_at(&text, cursor) {
            if local == word {
                let (line, column) = reclipse_editor::line_col_at_char(&text, declaration);
                self.open_at(path, line, column);
                return;
            }
        }

        let types = self.project_types.as_ref();
        let classpath = self.classpath_index.as_ref();
        let empty = ClasspathIndex::default();
        let classpath = classpath.unwrap_or(&empty);
        let Some(types) = types else {
            self.toast("Índice semântico ainda não está pronto");
            return;
        };

        if cursor_is_type_reference(&file, &text, start) {
            if let Some(resolved) = resolve_semantic_type(&word, &file, types, classpath) {
                self.open_resolved_type(resolved);
                return;
            }
        }

        if let Some(index) = self.index.as_ref() {
            if let Some(target) = member_target_at(&text, &file, cursor, types, index, classpath) {
                match target {
                    MemberTarget::Project {
                        path: target_path,
                        container,
                        name,
                        is_call,
                        ..
                    } => {
                        if let Some(symbol) = index.files.get(&target_path).and_then(|file| {
                            file.symbols.iter().find(|symbol| {
                                symbol.name == name
                                    && symbol.container.as_deref() == Some(container.as_str())
                                    && (if is_call {
                                        symbol.kind == SymbolKind::Method
                                    } else {
                                        matches!(
                                            symbol.kind,
                                            SymbolKind::Field | SymbolKind::EnumConstant
                                        )
                                    })
                            })
                        }) {
                            let text = fs::read_to_string(&target_path).unwrap_or_default();
                            let (line, column) = line_col_of_byte(&text, symbol.name_start_byte);
                            self.open_at(target_path, line, column);
                            return;
                        }
                    }
                    MemberTarget::Classpath { class, .. } => {
                        self.classpath_view = Some(class);
                        return;
                    }
                }
            }
        }

        if let Some(resolved) = resolve_semantic_type(&word, &file, types, classpath) {
            self.open_resolved_type(resolved);
            return;
        }

        let fallback = self.index.as_ref().and_then(|index| {
            let candidates = index.find_symbols(&word)?;
            let same_file: Vec<_> = candidates
                .iter()
                .filter(|location| location.file == path)
                .collect();
            let location = if same_file.len() == 1 {
                same_file[0]
            } else if candidates.len() == 1 {
                &candidates[0]
            } else {
                return None;
            };
            Some(location.clone())
        });
        match fallback {
            Some(location) => {
                self.open_at(location.file, location.symbol.line, location.symbol.column);
            }
            None => self.toast(format!(
                "Definição de '{word}' não encontrada (ambígua ou sem índice)"
            )),
        }
    }

    fn open_resolved_type(&mut self, resolved: ResolvedType) {
        match resolved {
            ResolvedType::Project(location) => {
                self.open_at(location.path, location.line, location.column);
            }
            ResolvedType::Classpath(name) => {
                self.classpath_view = Some(name);
            }
        }
    }

    fn find_references(&mut self) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let (word, start, cursor, path, text, file) = {
            let tab = &self.tabs[active];
            let Some((word, start, _)) = tab.doc.identifier_at_cursor() else {
                self.toast("Coloque o cursor sobre um identificador");
                return;
            };
            let path = tab.doc.path.clone();
            let text = tab.doc.text();
            let file = reclipse_java::parse_source(&path, &text);
            (word, start, tab.doc.cursor_char, path, text, file)
        };

        // Local variables: occurrences inside the current buffer.
        if let Some((local, _)) = local_binding_at(&text, cursor) {
            if local == word {
                if let Ok(plan) = plan_local_rename(&text, cursor, &format!("{word}_refs")) {
                    let matches = local_plan_matches(&path, &text, &plan, &word);
                    if !matches.is_empty() {
                        let count = matches.len();
                        self.project_search.show_results(word.clone(), matches);
                        self.toast(format!("{count} referência(s) da variável local '{word}'"));
                        return;
                    }
                }
                // Shadowed/ambiguous local: do not fall through to a field
                // with the same name; use textual search instead.
                self.textual_references(word);
                return;
            }
        }

        let types = self.project_types.as_ref();
        let classpath = self.classpath_index.as_ref();
        let empty = ClasspathIndex::default();
        let classpath = classpath.unwrap_or(&empty);
        let Some(types) = types else {
            self.toast("Índice semântico ainda não está pronto");
            return;
        };
        let Some(index) = self.index.as_ref() else {
            self.toast("Índice semântico ainda não está pronto");
            return;
        };

        if cursor_is_type_reference(&file, &text, start) {
            if let Some(resolved) = resolve_semantic_type(&word, &file, types, classpath) {
                let fqn = resolved.name().to_owned();
                if let Some(matches) = self.type_reference_matches(&fqn, types, index, classpath) {
                    let count = matches.len();
                    self.project_search.show_results(word.clone(), matches);
                    self.toast(format!("{count} referência(s) semânticas de {fqn}"));
                    return;
                }
            }
        }

        if let Some(target) = member_target_at(&text, &file, cursor, types, index, classpath) {
            if let Some((matches, label)) =
                self.member_reference_matches(&target, types, index, classpath)
            {
                let count = matches.len();
                self.project_search.show_results(word.clone(), matches);
                self.toast(format!("{count} referência(s) semânticas de {label}"));
                return;
            }
        }

        if let Some(resolved) = resolve_semantic_type(&word, &file, types, classpath) {
            let fqn = resolved.name().to_owned();
            if let Some(matches) = self.type_reference_matches(&fqn, types, index, classpath) {
                let count = matches.len();
                self.project_search.show_results(word.clone(), matches);
                self.toast(format!("{count} referência(s) semânticas de {fqn}"));
                return;
            }
        }

        self.textual_references(word);
    }

    fn textual_references(&mut self, word: String) {
        self.project_search.query = word.clone();
        self.project_search.case_sensitive = true;
        self.project_search.whole_word = true;
        self.project_search.regex = false;
        self.completion = None;
        self.project_search.open();
        self.run_project_search();
        self.toast(format!("Referências textuais de '{word}'"));
    }

    fn type_reference_matches(
        &self,
        fqn: &str,
        types: &ProjectTypes,
        index: &ProjectIndex,
        classpath: &ClasspathIndex,
    ) -> Option<Vec<ProjectMatch>> {
        let references = find_type_references(fqn, types, index, classpath);
        if references.is_empty() {
            return None;
        }
        let mut cache: HashMap<PathBuf, String> = HashMap::new();
        let matches: Vec<ProjectMatch> = references
            .into_iter()
            .map(|reference| {
                let text = cache
                    .entry(reference.file.clone())
                    .or_insert_with(|| fs::read_to_string(&reference.file).unwrap_or_default());
                ProjectMatch {
                    preview: preview_line(text, reference.line),
                    file: reference.file,
                    line: reference.line,
                    column: reference.column,
                    start: 0,
                    end: reference.name.chars().count(),
                }
            })
            .collect();
        Some(matches)
    }

    fn member_reference_matches(
        &self,
        target: &MemberTarget,
        types: &ProjectTypes,
        index: &ProjectIndex,
        classpath: &ClasspathIndex,
    ) -> Option<(Vec<ProjectMatch>, String)> {
        let references = find_member_references(target, types, index, classpath);
        let label = format!("{}.{}", target.owner_name(), target.name());
        let mut matches: Vec<ProjectMatch> = Vec::new();
        if let MemberTarget::Project {
            path,
            container,
            name,
            is_call,
            ..
        } = target
        {
            if let Some(file) = index.files.get(path) {
                let text = fs::read_to_string(path).unwrap_or_default();
                for symbol in &file.symbols {
                    if symbol.name == *name
                        && symbol.container.as_deref() == Some(container.as_str())
                        && (if *is_call {
                            symbol.kind == SymbolKind::Method
                        } else {
                            matches!(symbol.kind, SymbolKind::Field | SymbolKind::EnumConstant)
                        })
                    {
                        let (line, column) = line_col_of_byte(&text, symbol.name_start_byte);
                        matches.push(ProjectMatch {
                            file: path.clone(),
                            line,
                            column,
                            start: 0,
                            end: symbol.name.chars().count(),
                            preview: preview_line(&text, line),
                        });
                    }
                }
            }
        }
        let mut cache: HashMap<PathBuf, String> = HashMap::new();
        for reference in references {
            let text = cache
                .entry(reference.file.clone())
                .or_insert_with(|| fs::read_to_string(&reference.file).unwrap_or_default());
            matches.push(ProjectMatch {
                preview: preview_line(text, reference.line),
                file: reference.file,
                line: reference.line,
                column: reference.column,
                start: 0,
                end: reference.name.chars().count(),
            });
        }
        if matches.is_empty() {
            return None;
        }
        matches.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
        });
        matches.dedup_by(|left, right| {
            left.file == right.file && left.line == right.line && left.column == right.column
        });
        Some((matches, label))
    }

    fn begin_rename(&mut self) {
        if self.rename.is_some() {
            return;
        }
        self.completion = None;
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let (word, start, cursor, path, text, file) = {
            let tab = &self.tabs[active];
            let Some((word, start, _)) = tab.doc.identifier_at_cursor() else {
                self.toast("Coloque o cursor sobre um identificador");
                return;
            };
            let path = tab.doc.path.clone();
            let text = tab.doc.text();
            let file = reclipse_java::parse_source(&path, &text);
            (word, start, tab.doc.cursor_char, path, text, file)
        };

        if let Some((local, _)) = local_binding_at(&text, cursor) {
            if local == word {
                let preview = plan_local_rename(&text, cursor, &format!("{word}_"))
                    .ok()
                    .map(|plan| plan.edits.len());
                self.rename = Some(RenameState {
                    target: RenameTarget::Local { path, cursor },
                    old_name: word.clone(),
                    new_name: word,
                    kind: RenameKind::Local,
                    preview,
                    focus_pending: true,
                    error: None,
                });
                return;
            }
        }

        let types = self.project_types.as_ref();
        let classpath = self.classpath_index.as_ref();
        let empty = ClasspathIndex::default();
        let classpath = classpath.unwrap_or(&empty);
        let Some(types) = types else {
            self.toast("Índice semântico ainda não está pronto");
            return;
        };
        let Some(index) = self.index.as_ref() else {
            self.toast("Índice semântico ainda não está pronto");
            return;
        };

        if cursor_is_type_reference(&file, &text, start) {
            if let Some(ResolvedType::Project(location)) =
                resolve_semantic_type(&word, &file, types, classpath)
            {
                let preview =
                    plan_type_rename(&location.fqn, &format!("{word}_"), types, index, classpath)
                        .ok()
                        .map(|plan| plan.total_edits());
                self.rename = Some(RenameState {
                    target: RenameTarget::Type(location.fqn.clone()),
                    old_name: word.clone(),
                    new_name: word,
                    kind: RenameKind::Type,
                    preview,
                    focus_pending: true,
                    error: None,
                });
                return;
            }
        }

        if let Some(target) = member_target_at(&text, &file, cursor, types, index, classpath) {
            let (member_name, is_call) = match &target {
                MemberTarget::Project { name, is_call, .. } => (name.clone(), *is_call),
                MemberTarget::Classpath { .. } => {
                    self.toast("Membros do classpath/JDK são somente leitura");
                    return;
                }
            };
            let preview =
                plan_member_rename(&target, &format!("{member_name}_"), types, index, classpath)
                    .ok()
                    .map(|plan| plan.total_edits());
            self.rename = Some(RenameState {
                target: RenameTarget::Member(target),
                old_name: word.clone(),
                new_name: word,
                kind: if is_call {
                    RenameKind::Method
                } else {
                    RenameKind::Field
                },
                preview,
                focus_pending: true,
                error: None,
            });
            return;
        }

        if let Some(ResolvedType::Project(location)) =
            resolve_semantic_type(&word, &file, types, classpath)
        {
            let preview =
                plan_type_rename(&location.fqn, &format!("{word}_"), types, index, classpath)
                    .ok()
                    .map(|plan| plan.total_edits());
            self.rename = Some(RenameState {
                target: RenameTarget::Type(location.fqn.clone()),
                old_name: word.clone(),
                new_name: word,
                kind: RenameKind::Type,
                preview,
                focus_pending: true,
                error: None,
            });
            return;
        }
        self.toast("Nada renomeável sob o cursor");
    }

    fn apply_rename(&mut self) {
        let Some(state) = self.rename.as_ref() else {
            return;
        };
        let target = state.target.clone();
        let old_name = state.old_name.clone();
        let new_name = state.new_name.trim().to_owned();
        if new_name.is_empty() {
            if let Some(state) = &mut self.rename {
                state.error = Some("informe um nome".to_owned());
            }
            return;
        }

        if let RenameTarget::Local { path, cursor } = &target {
            let Some(index) = self.tabs.iter().position(|tab| tab.doc.path == *path) else {
                self.rename = None;
                self.toast("A aba do arquivo foi fechada");
                return;
            };
            let source = self.tabs[index].doc.text();
            match plan_local_rename(&source, *cursor, &new_name) {
                Ok(plan) if plan.old_name != old_name => {
                    if let Some(state) = &mut self.rename {
                        state.error = Some(
                            "o buffer mudou desde a abertura do diálogo; tente novamente"
                                .to_owned(),
                        );
                    }
                }
                Ok(plan) => {
                    let ranges: Vec<(usize, usize)> = plan
                        .edits
                        .iter()
                        .map(|edit| {
                            (
                                source[..edit.start_byte].chars().count(),
                                source[..edit.end_byte].chars().count(),
                            )
                        })
                        .collect();
                    self.tabs[index].doc.replace_ranges(ranges, &plan.new_name);
                    self.tabs[index].invalidate_highlight();
                    self.tabs[index].focus();
                    self.rename = None;
                    self.toast(format!(
                        "{} renomeado para {} ({} ocorrência(s))",
                        plan.old_name,
                        plan.new_name,
                        plan.edits.len()
                    ));
                }
                Err(error) => {
                    if let Some(state) = &mut self.rename {
                        state.error = Some(error);
                    }
                }
            }
            return;
        }

        let types = self.project_types.as_ref();
        let classpath = self.classpath_index.as_ref();
        let empty = ClasspathIndex::default();
        let classpath = classpath.unwrap_or(&empty);
        let Some(types) = types else {
            return;
        };
        let Some(index) = self.index.as_ref() else {
            return;
        };
        let plan = match &target {
            RenameTarget::Type(fqn) => plan_type_rename(fqn, &new_name, types, index, classpath),
            RenameTarget::Member(target) => {
                plan_member_rename(target, &new_name, types, index, classpath)
            }
            RenameTarget::Local { .. } => unreachable!(),
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                if let Some(state) = &mut self.rename {
                    state.error = Some(error);
                }
                return;
            }
        };

        let dirty = self
            .tabs
            .iter()
            .find(|tab| tab.doc.dirty && is_java_path(&tab.doc.path))
            .map(|tab| tab.doc.path.clone());
        if let Some(path) = dirty {
            if let Some(state) = &mut self.rename {
                state.error = Some(format!(
                    "salve {} antes de renomear (workspace rename exige buffers limpos)",
                    path.display()
                ));
            }
            return;
        }

        match self.apply_workspace_rename(&plan) {
            Ok(()) => {
                let count = plan.total_edits();
                let renamed_file = plan.rename_file.as_ref().map(|(_, to)| {
                    to.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                self.rename = None;
                match renamed_file {
                    Some(name) => self.toast(format!(
                        "{} renomeado para {new_name} ({count} edições; arquivo {name})",
                        plan.old_name
                    )),
                    None => self.toast(format!(
                        "{} renomeado para {new_name} ({count} ocorrência(s))",
                        plan.old_name
                    )),
                }
            }
            Err(error) => {
                if let Some(state) = &mut self.rename {
                    state.error = Some(error);
                }
            }
        }
    }

    fn apply_workspace_rename(&mut self, plan: &RenamePlan) -> Result<(), String> {
        let mut new_texts: Vec<(PathBuf, String)> = Vec::new();
        for file in &plan.files {
            // Disk is the source of truth for workspace edits; dirty buffers are
            // rejected before this point.
            let text = fs::read_to_string(&file.path)
                .map_err(|error| format!("falha ao ler {}: {error}", file.path.display()))?;
            let updated = apply_text_edits(&text, &file.edits)?;
            fs::write(&file.path, &updated)
                .map_err(|error| format!("falha ao salvar {}: {error}", file.path.display()))?;
            new_texts.push((file.path.clone(), updated));
        }
        if let Some((from, to)) = &plan.rename_file {
            fs::rename(from, to).map_err(|error| {
                format!(
                    "falha ao renomear {} para {}: {error}",
                    from.display(),
                    to.display()
                )
            })?;
        }
        for (old_path, _) in &new_texts {
            let new_path = plan
                .rename_file
                .as_ref()
                .filter(|(from, _)| from == old_path)
                .map(|(_, to)| to.clone())
                .unwrap_or_else(|| old_path.clone());
            if let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.doc.path == old_path) {
                tab.doc.path = new_path.clone();
                let _ = tab.reload_from_disk();
                tab.external_change = None;
                tab.invalidate_highlight();
            }
            self.watch_ignored
                .insert(old_path.clone(), Instant::now() + Duration::from_secs(2));
            self.watch_ignored
                .insert(new_path, Instant::now() + Duration::from_secs(2));
        }
        self.tree.invalidate();
        if let Some(index) = &mut self.index {
            let mut removed = Vec::new();
            let mut updates = Vec::new();
            for (old_path, text) in &new_texts {
                let new_path = plan
                    .rename_file
                    .as_ref()
                    .filter(|(from, _)| from == old_path)
                    .map(|(_, to)| to.clone())
                    .unwrap_or_else(|| old_path.clone());
                if new_path != *old_path {
                    removed.push(old_path.clone());
                }
                updates.push((new_path, text.clone()));
            }
            index.update_files(&removed, updates);
            if let Some(project) = &mut self.project {
                replace_project_paths(project, plan.rename_file.as_ref());
            }
            replace_project_paths(&mut index.project, plan.rename_file.as_ref());
            let stats = index.stats.clone();
            self.index_status = IndexStatus::Ready {
                files: stats.java_files,
                symbols: stats.symbols,
                problems: stats.problems,
                elapsed_ms: stats.elapsed_ms,
            };
            self.refresh_semantics();
        } else if self.root.is_some() {
            self.reindex();
        }
        Ok(())
    }

    fn show_rename(&mut self, ctx: &Context) {
        if self.rename.is_none() {
            return;
        }
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Renomear")
            .id(egui::Id::new("reclipse_rename"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                let state = self.rename.as_mut().expect("rename state");
                ui.label(
                    RichText::new(format!("{} · {}", state.old_name, state.kind.label()))
                        .strong()
                        .color(theme::accent()),
                );
                ui.add_space(4.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut state.new_name)
                        .hint_text("novo nome")
                        .desired_width(f32::INFINITY),
                );
                if state.focus_pending {
                    response.request_focus();
                    if response.has_focus() {
                        state.focus_pending = false;
                    }
                }
                if response.changed() {
                    state.error = None;
                }
                if let Some(preview) = state.preview {
                    ui.label(
                        RichText::new(format!("prévia: ~{preview} ocorrência(s)"))
                            .small()
                            .color(theme::muted()),
                    );
                }
                if let Some(error) = &state.error {
                    ui.label(RichText::new(error).small().color(theme::red()));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Renomear").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
                let enter = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
                let escape = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
                if enter {
                    apply = true;
                }
                if escape {
                    cancel = true;
                }
            });
        if cancel {
            self.rename = None;
        } else if apply {
            self.apply_rename();
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
            show_console: self.show_console,
            font_size: self.font_size,
            theme: self.theme.config_name().to_owned(),
            keybindings: self.keybindings.clone(),
        }
    }

    fn set_theme(&mut self, kind: crate::theme::ThemeKind) {
        self.theme = kind;
        self.theme_label = kind.short_label().to_owned();
        crate::theme::configure(&self.egui_ctx, kind);
        self.tabs.iter_mut().for_each(Tab::invalidate_theme);
        self.toast(format!("Tema: {}", kind.label()));
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
                    .color(theme::muted()),
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
                                        .color(theme::muted()),
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
                    .color(theme::yellow()),
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
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("BUSCAR")
                            .small()
                            .strong()
                            .color(theme::muted()),
                    );
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
                        .color(theme::muted()),
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
                        ui.label(RichText::new("Substituir por").color(theme::muted()));
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
                    ui.label(RichText::new(error).small().color(theme::red()));
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
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("BUSCAR NO PROJETO")
                            .small()
                            .strong()
                            .color(theme::muted()),
                    );
                    if let Some(name) = &project_name {
                        ui.label(RichText::new(name).small().color(theme::muted()));
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
                            .color(theme::muted()),
                    );
                    if ui.button("Fechar").clicked() {
                        close = true;
                    }
                });

                if search_has_focus && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    run = true;
                }

                if let Some(error) = &error {
                    ui.label(RichText::new(error).color(theme::red()));
                }

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Substituir por").color(theme::muted()));
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
                            .color(theme::muted()),
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
                                .color(theme::muted()),
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
        let text = message.into();
        let level = if text.contains("Falha")
            || text.contains("falhou")
            || text.contains("não foi possível")
            || text.contains("Erro")
        {
            LogLevel::Error
        } else if text.contains("Salvo")
            || text.contains("renomeado")
            || text.contains("Recarregado")
        {
            LogLevel::Success
        } else if text.contains("aviso") || text.contains("warning") {
            LogLevel::Warning
        } else {
            LogLevel::Info
        };
        self.log(level, text.clone());
        self.status_message = Some(Toast {
            text,
            until: Instant::now() + Duration::from_secs(5),
        });
    }

    fn log(&mut self, level: LogLevel, text: impl Into<String>) {
        self.console.push(LogLine {
            at: self.app_started_at.elapsed(),
            level,
            text: text.into(),
        });
        if self.console.len() > 300 {
            self.console.remove(0);
        }
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
                    PaletteEntry::new("Editar: renomear", "Alt+Shift+R", Action::Rename),
                    PaletteEntry::new(
                        "Navegar: referências",
                        "Ctrl+Shift+G",
                        Action::FindReferences,
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
                            name.replace('$', "."),
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
        if self.palette.open || self.close_confirm.is_some() || self.rename.is_some() {
            return;
        }

        if ctx.input_mut(|input| input.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::R)) {
            self.completion = None;
            self.begin_rename();
            return;
        }

        if self.completion.is_some() {
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                self.last_completion_revision = self.active_tab.map(|active| {
                    (
                        self.tabs[active].doc.path.clone(),
                        self.tabs[active].doc.revision,
                    )
                });
                self.completion = None;
                return;
            }
            let visible = self.completion_visible();
            if visible.is_empty() {
                if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter)) {
                    self.last_completion_revision = self.active_tab.map(|active| {
                        (
                            self.tabs[active].doc.path.clone(),
                            self.tabs[active].doc.revision,
                        )
                    });
                    self.completion = None;
                    return;
                }
            } else {
                if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                    if let Some(state) = &mut self.completion {
                        state.selected = (state.selected + 1).min(visible.len() - 1);
                    }
                    return;
                }
                if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                    if let Some(state) = &mut self.completion {
                        state.selected = state.selected.saturating_sub(1);
                    }
                    return;
                }
                let accept = ctx.input_mut(|input| {
                    input.consume_key(Modifiers::NONE, Key::Enter)
                        || input.consume_key(Modifiers::NONE, Key::Tab)
                });
                if accept {
                    let index = visible[self.completion.as_ref().map_or(0, |state| state.selected)];
                    self.accept_completion(index);
                    return;
                }
            }
        }

        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::Space)) {
            self.open_completion(true);
            return;
        }

        let modifiers = ctx.input(|input| input.modifiers);
        if modifiers.ctrl
            && modifiers.shift
            && ctx.input_mut(|input| input.consume_key(modifiers, Key::G))
        {
            self.find_references();
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

    fn completion_visible(&self) -> Vec<usize> {
        let Some(state) = &self.completion else {
            return Vec::new();
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.doc.path == state.path) else {
            return Vec::new();
        };
        let cursor = tab.doc.cursor_char;
        if cursor < state.start {
            return Vec::new();
        }
        let prefix = tab.doc.slice(state.start, cursor);
        if !prefix.chars().all(is_completion_identifier) {
            return Vec::new();
        }
        let query = prefix.to_lowercase();
        let mut matches: Vec<(u8, usize)> = state
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let label = item.label.to_lowercase();
                if label.starts_with(&query) {
                    Some((0, index))
                } else if query.len() > 1 && label.contains(&query) {
                    Some((1, index))
                } else {
                    None
                }
            })
            .collect();
        matches.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        matches
            .into_iter()
            .map(|(_, index)| index)
            .take(80)
            .collect()
    }

    fn open_completion(&mut self, manual: bool) {
        let Some(active) = self.active_tab else {
            if manual {
                self.toast("Nenhum arquivo ativo");
            }
            return;
        };
        let Some(project_types) = self.project_types.as_ref() else {
            if manual {
                self.toast("Índice ainda construindo");
            }
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let cursor = self.tabs[active].doc.cursor_char;
        let window_start = cursor.saturating_sub(100_000);
        let source = self.tabs[active].doc.slice(window_start, cursor);
        let window_cursor = source.chars().count();
        let anchor = self.tabs[active]
            .cursor_screen_rect()
            .map(|rect| rect.right_bottom())
            .unwrap_or_else(|| egui::Pos2::new(320.0, 120.0));
        let file = self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(&path))
            .cloned()
            .unwrap_or_else(|| {
                let full = self.tabs[active].doc.text();
                reclipse_java::parse_source(&path, &full)
            });
        let empty = ClasspathIndex::default();
        let classpath = self.classpath_index.as_ref().unwrap_or(&empty);
        let context = completion_context(
            &source,
            window_cursor,
            &file,
            project_types,
            self.index.as_ref(),
            classpath,
        );
        if context.items.is_empty() {
            if manual {
                self.toast(if context.receiver.is_some() {
                    "Sem sugestões para este receptor"
                } else {
                    "Sem sugestões"
                });
            }
            self.completion = None;
            return;
        }
        self.completion = Some(CompletionState {
            path,
            start: window_start + context.start,
            items: context.items,
            selected: 0,
            anchor,
            receiver: context.receiver,
        });
    }

    fn maybe_auto_complete(&mut self) {
        let Some(active) = self.active_tab else {
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let revision = self.tabs[active].doc.revision;
        let changed =
            self.last_completion_revision
                .as_ref()
                .is_none_or(|(known_path, known_revision)| {
                    known_path != &path || *known_revision != revision
                });
        if !changed {
            return;
        }
        self.last_completion_revision = Some((path, revision));
        let cursor = self.tabs[active].doc.cursor_char;
        if cursor == 0 || self.tabs[active].doc.slice(cursor - 1, cursor) != "." {
            return;
        }
        self.open_completion(false);
    }

    fn accept_completion(&mut self, index: usize) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(active) = self.active_tab else {
            return;
        };
        let Some(item) = state.items.get(index) else {
            return;
        };
        let insert = item.insert_text.clone();
        let inserted_len = insert.chars().count();
        let tab = &mut self.tabs[active];
        if tab.doc.path != state.path {
            return;
        }
        tab.doc.edit(state.start, tab.doc.cursor_char, &insert);
        let cursor = state.start + inserted_len;
        tab.doc.set_cursor(cursor, false);
        tab.pending_cursor = Some(cursor);
        self.last_completion_revision = Some((state.path, tab.doc.revision));
    }

    fn show_completion(&mut self, ctx: &Context) {
        let Some(state) = &self.completion else {
            return;
        };
        let active_path = self
            .active_tab
            .and_then(|active| self.tabs.get(active))
            .map(|tab| tab.doc.path.as_path());
        if active_path != Some(state.path.as_path()) {
            self.completion = None;
            return;
        }
        let Some(tab) = self.tabs.iter().find(|tab| tab.doc.path == state.path) else {
            self.completion = None;
            return;
        };
        let cursor = tab.doc.cursor_char;
        if cursor < state.start {
            self.completion = None;
            return;
        }
        let prefix = tab.doc.slice(state.start, cursor);
        if !prefix.chars().all(is_completion_identifier) {
            self.completion = None;
            return;
        }
        let visible = self.completion_visible();
        if visible.is_empty() {
            self.completion = None;
            return;
        }
        let (anchor, receiver, selected) = {
            let state = self.completion.as_mut().unwrap();
            state.selected = state.selected.min(visible.len() - 1);
            let live_anchor = self
                .tabs
                .iter()
                .find(|tab| tab.doc.path == state.path)
                .and_then(|tab| tab.cursor_screen_rect())
                .map(|rect| rect.right_bottom())
                .unwrap_or(state.anchor);
            state.anchor = live_anchor;
            (live_anchor, state.receiver.clone(), state.selected)
        };
        let screen = ctx.screen_rect();
        let mut position = anchor + egui::vec2(4.0, 2.0);
        if position.y + 280.0 > screen.bottom() {
            position.y = (anchor.y - 280.0).max(screen.top() + 8.0);
        }
        position.x = position.x.clamp(
            screen.left() + 8.0,
            (screen.right() - 360.0).max(screen.left() + 8.0),
        );

        let mut clicked = None;
        {
            let items = &self.completion.as_ref().unwrap().items;
            egui::Area::new(egui::Id::new("reclipse_completion"))
                .order(egui::Order::Foreground)
                .fixed_pos(position)
                .show(ctx, |ui| {
                    egui::Frame::NONE
                        .fill(theme::panel())
                        .stroke(egui::Stroke::new(1.0_f32, theme::accent()))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_min_width(320.0);
                            ui.set_max_width(620.0);
                            let title = match &receiver {
                                Some(receiver) => {
                                    format!("Completion · {receiver} · {} sugestões", visible.len())
                                }
                                None => format!("Completion · {} sugestões", visible.len()),
                            };
                            ui.label(RichText::new(title).small().color(theme::muted()));
                            egui::ScrollArea::vertical()
                                .max_height(230.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (rank, index) in visible.iter().enumerate() {
                                        let item = &items[*index];
                                        let glyph = match item.kind {
                                            CompletionKind::Type => "T",
                                            CompletionKind::Field => "f",
                                            CompletionKind::Method => "m",
                                        };
                                        let color = match item.kind {
                                            CompletionKind::Type => theme::clr_type(),
                                            CompletionKind::Field => theme::muted(),
                                            CompletionKind::Method => theme::yellow(),
                                        };
                                        let detail = truncate_detail(&item.detail, 72);
                                        let response = ui.selectable_label(
                                            rank == selected,
                                            RichText::new(format!(
                                                "{glyph}  {}    {detail}",
                                                item.label
                                            ))
                                            .color(color)
                                            .size(12.5),
                                        );
                                        if response.clicked() {
                                            clicked = Some(*index);
                                        }
                                        response.on_hover_text(&item.detail);
                                    }
                                });
                            ui.label(
                                RichText::new("Ctrl+Space · Enter/Tab aceitar · Esc fechar")
                                    .small()
                                    .color(theme::muted()),
                            );
                        });
                });
        }

        if let Some(index) = clicked {
            self.accept_completion(index);
        }
    }

    fn show_hover(&mut self, ctx: &Context) {
        if self.completion.is_some()
            || self.palette.open
            || self.rename.is_some()
            || self.classpath_view.is_some()
            || self.classpath_source.is_some()
            || self.about_open
            || self.shortcuts_open
            || self.close_confirm.is_some()
        {
            return;
        }
        let Some(active) = self.active_tab else {
            return;
        };
        let (word, position, start, path) = if let Some(word) = self.screenshot.hover_word.clone() {
            let tab = &self.tabs[active];
            let path = tab.doc.path.clone();
            let source = tab.doc.text();
            let Some(byte_position) = source.find(&word) else {
                return;
            };
            let start = source[..byte_position].chars().count();
            (word, egui::Pos2::new(620.0, 300.0), start, path)
        } else {
            let tab = &self.tabs[active];
            let Some((word, position, start)) = tab.hovered_identifier() else {
                return;
            };
            (word.to_owned(), position, start, tab.doc.path.clone())
        };
        if word.chars().count() < 2 {
            return;
        }
        let Some(project_types) = self.project_types.as_ref() else {
            return;
        };
        let Some(file) = self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(&path))
            .cloned()
        else {
            return;
        };
        let empty = ClasspathIndex::default();
        let classpath = self.classpath_index.as_ref().unwrap_or(&empty);

        if let Some(resolved) = resolve_semantic_type(&word, &file, project_types, classpath) {
            let (title, lines) = hover_resolved(&word, &resolved, classpath);
            show_hover_popup(ctx, position, &title, &lines);
            return;
        }

        let prefix_start = start.saturating_sub(20_000);
        let mut prefix = self.tabs[active].doc.slice(prefix_start, start);
        prefix.push_str(&word);
        let cursor = prefix.chars().count();
        if let Some(type_name) = infer_variable_type(&prefix, cursor, &word) {
            let mut lines = Vec::new();
            let mut title = format!("{word} : {type_name}");
            if let Some(resolved) =
                resolve_semantic_type(&type_name, &file, project_types, classpath)
            {
                let (resolved_title, mut resolved_lines) =
                    hover_resolved(&type_name, &resolved, classpath);
                title = format!("{word} : {resolved_title}");
                lines.append(&mut resolved_lines);
            }
            show_hover_popup(ctx, position, &title, &lines);
            return;
        }

        if let Some(symbol) = file.symbols.iter().find(|symbol| symbol.name == word) {
            let title = format!("{word} · {}", symbol.kind.label());
            let lines = symbol.detail.clone().into_iter().collect::<Vec<_>>();
            show_hover_popup(ctx, position, &title, &lines);
            return;
        }
        if let Some(location) = self
            .index
            .as_ref()
            .and_then(|index| index.find_best(&word, &path))
        {
            let title = format!("{word} · {}", location.symbol.kind.label());
            let relative = self
                .root
                .as_ref()
                .map(|root| {
                    location
                        .file
                        .strip_prefix(root)
                        .unwrap_or(&location.file)
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| location.file.display().to_string());
            let mut lines = location
                .symbol
                .detail
                .clone()
                .into_iter()
                .collect::<Vec<_>>();
            lines.push(relative);
            show_hover_popup(ctx, position, &title, &lines);
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
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    draw_emblem(ui, self.theme, 18.0).on_hover_text("RECLIPSE Studio v1.0");
                    ui.add_space(4.0);
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
                            .add(egui::Button::new("Renomear…").shortcut_text("Alt+Shift+R"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.begin_rename();
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
                        if ui
                            .add(egui::Button::new("Referências").shortcut_text("Ctrl+Shift+G"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.find_references();
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
                                    .color(theme::muted()),
                            );
                        }
                    });

                    ui.menu_button("Exibir", |ui| {
                        ui.checkbox(&mut self.show_project, "Projeto");
                        ui.checkbox(&mut self.show_outline, "Estrutura");
                        ui.checkbox(&mut self.show_problems, "Problemas");
                        ui.checkbox(&mut self.show_console, "Console");
                        ui.separator();
                        ui.label(
                            RichText::new("Tema (RECLIPSE Studio)")
                                .small()
                                .strong()
                                .color(theme::muted()),
                        );
                        for kind in [
                            crate::theme::ThemeKind::RustOxide,
                            crate::theme::ThemeKind::EclipseCosmic,
                            crate::theme::ThemeKind::CyberSolar,
                        ] {
                            let selected = self.theme == kind;
                            if ui.selectable_label(selected, kind.label()).clicked() {
                                ui.close_menu();
                                self.set_theme(kind);
                            }
                        }
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
                                    .color(theme::muted()),
                            );
                            ui.label(
                                RichText::new(
                                    "Coloque reclipse_plugin_*.dll ao lado do executável",
                                )
                                .small()
                                .color(theme::muted()),
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
                                .font(theme::display_font(13.0))
                                .color(theme::accent()),
                        );
                        ui.label(
                            RichText::new("Rust-native Java IDE")
                                .small()
                                .color(theme::muted()),
                        );
                        ui.separator();
                        for kind in [
                            crate::theme::ThemeKind::CyberSolar,
                            crate::theme::ThemeKind::EclipseCosmic,
                            crate::theme::ThemeKind::RustOxide,
                        ] {
                            let selected = self.theme == kind;
                            if ui
                                .selectable_label(
                                    selected,
                                    RichText::new(kind.short_label()).size(10.5),
                                )
                                .on_hover_text(kind.label())
                                .clicked()
                            {
                                self.set_theme(kind);
                            }
                        }
                    });
                });
            });
    }

    fn quick_toolbar(&mut self, ctx: &Context) {
        let mut actions = Vec::new();
        let syntax_problems = self
            .index
            .as_ref()
            .map(|index| index.problems.len())
            .unwrap_or(0);
        let semantic_problems = self.semantic_problems.len();
        let index_text = match &self.index_status {
            IndexStatus::Ready { elapsed_ms, .. } => {
                (format!("Rust core · {elapsed_ms} ms"), theme::green())
            }
            IndexStatus::Indexing { .. } => ("Indexando…".to_owned(), theme::yellow()),
            IndexStatus::Failed(_) => ("Índice falhou".to_owned(), theme::red()),
            IndexStatus::Idle => ("Rust core · idle".to_owned(), theme::muted()),
        };
        let cp_text = match &self.classpath_status {
            ClasspathStatus::Ready(stats) => stats
                .jdk_target
                .map(|target| format!("Java {target}"))
                .unwrap_or_else(|| "classpath pronto".to_owned()),
            ClasspathStatus::Indexing { .. } => "classpath…".to_owned(),
            ClasspathStatus::Failed(_) => "classpath falhou".to_owned(),
            ClasspathStatus::Idle => "classpath —".to_owned(),
        };
        let cp_color = match &self.classpath_status {
            ClasspathStatus::Ready(_) => theme::green(),
            ClasspathStatus::Indexing { .. } => theme::yellow(),
            ClasspathStatus::Failed(_) => theme::red(),
            ClasspathStatus::Idle => theme::muted(),
        };

        egui::TopBottomPanel::top("reclipse_toolbar")
            .exact_height(34.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel_alt())
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if toolbar_button(ui, "Salvar", "Salvar arquivo ativo (Ctrl+S)") {
                        actions.push(Action::Save);
                    }
                    if toolbar_button(ui, "Reindexar", "Reindexar projeto (F5)") {
                        actions.push(Action::Reindex);
                    }
                    if toolbar_button(ui, "Buscar", "Buscar no projeto (Ctrl+Shift+F)") {
                        actions.push(Action::FindInProject);
                    }
                    if toolbar_button(ui, "Referências", "Referências (Ctrl+Shift+G)") {
                        actions.push(Action::FindReferences);
                    }
                    if toolbar_button(ui, "Renomear", "Renomear (Alt+Shift+R)") {
                        actions.push(Action::Rename);
                    }
                    ui.separator();
                    ui.label(
                        RichText::new("JAVA")
                            .monospace()
                            .size(10.0)
                            .color(theme::clr_type()),
                    );
                    if let Some(project) = &self.project {
                        ui.label(RichText::new(&project.name).small().color(theme::muted()))
                            .on_hover_text(project.root.display().to_string());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(cp_text).small().color(cp_color));
                        ui.separator();
                        ui.label(RichText::new(index_text.0).small().color(index_text.1));
                        ui.separator();
                        problem_badge(ui, "erros", syntax_problems, theme::red());
                        problem_badge(ui, "avisos", semantic_problems, theme::yellow());
                    });
                });
            });

        for action in actions {
            self.execute_action(action);
        }
    }

    fn activity_bar(&mut self, ctx: &Context) {
        let problem_count = self
            .index
            .as_ref()
            .map(|index| index.problems.len())
            .unwrap_or(0)
            + self.semantic_problems.len();
        let mut open_settings = false;
        let mut toggle_console = false;
        egui::SidePanel::left("reclipse_activity")
            .exact_width(42.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel_alt())
                    .inner_margin(egui::Margin::symmetric(3, 6)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    if activity_button(ui, ActivityIcon::Project, self.show_project, "Projeto")
                        .clicked()
                    {
                        self.show_project = !self.show_project;
                    }
                    if activity_button(ui, ActivityIcon::Outline, self.show_outline, "Estrutura")
                        .clicked()
                    {
                        self.show_outline = !self.show_outline;
                    }
                    let response = activity_button(
                        ui,
                        ActivityIcon::Problems,
                        self.show_problems,
                        "Problemas",
                    );
                    if problem_count > 0 {
                        let badge_pos = response.rect.right_top() + egui::vec2(-8.0, 3.0);
                        ui.painter().circle_filled(badge_pos, 4.0, theme::red());
                    }
                    if response.clicked() {
                        self.show_problems = !self.show_problems;
                    }
                    if activity_button(ui, ActivityIcon::Console, self.show_console, "Console")
                        .clicked()
                    {
                        toggle_console = true;
                    }
                    ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                        if activity_button(ui, ActivityIcon::Settings, false, "Configurações")
                            .clicked()
                        {
                            open_settings = true;
                        }
                    });
                });
            });
        if toggle_console {
            self.show_console = !self.show_console;
        }
        if open_settings {
            self.open_settings();
        }
    }

    fn console_panel(&mut self, ctx: &Context) {
        if !self.show_console {
            return;
        }
        let mut clear = false;
        let mut close = false;
        let index_line = match &self.index_status {
            IndexStatus::Ready {
                files,
                symbols,
                problems,
                elapsed_ms,
            } => format!(
                "Índice Rust: {files} arquivos · {symbols} símbolos · {problems} problemas · {elapsed_ms} ms"
            ),
            IndexStatus::Indexing { done, total } => {
                format!("Indexação em andamento: {done}/{total}")
            }
            IndexStatus::Failed(error) => format!("Falha na indexação: {error}"),
            IndexStatus::Idle => "Índice ocioso".to_owned(),
        };
        let classpath_line = match &self.classpath_status {
            ClasspathStatus::Ready(stats) => format!(
                "Classpath: {} archives · {} classes · {} ms · Java {}",
                stats.archives,
                stats.classes,
                stats.elapsed_ms,
                stats
                    .jdk_target
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".to_owned())
            ),
            ClasspathStatus::Indexing { done, total } => {
                format!("Classpath: {done}/{total} archives")
            }
            ClasspathStatus::Failed(error) => format!("Classpath falhou: {error}"),
            ClasspathStatus::Idle => "Classpath ocioso".to_owned(),
        };
        egui::TopBottomPanel::bottom("reclipse_console")
            .resizable(true)
            .default_height(132.0)
            .min_height(72.0)
            .max_height(360.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            self.console_tab == ConsoleTab::Output,
                            RichText::new("Console").size(12.0).strong(),
                        )
                        .clicked()
                    {
                        self.console_tab = ConsoleTab::Output;
                    }
                    if ui
                        .selectable_label(
                            self.console_tab == ConsoleTab::Rust,
                            RichText::new("Rust Engine").size(12.0).strong(),
                        )
                        .clicked()
                    {
                        self.console_tab = ConsoleTab::Rust;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text("Fechar console")
                            .clicked()
                        {
                            close = true;
                        }
                        if ui.small_button("Limpar").clicked() {
                            clear = true;
                        }
                        ui.label(
                            RichText::new("Rust-native diagnostics")
                                .small()
                                .color(theme::muted()),
                        );
                    });
                });
                ui.separator();
                match self.console_tab {
                    ConsoleTab::Output => {
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.console.is_empty() {
                                    ui.label(
                                        RichText::new("Sem saída ainda.")
                                            .small()
                                            .color(theme::muted()),
                                    );
                                }
                                for line in &self.console {
                                    let color = match line.level {
                                        LogLevel::Info => theme::muted(),
                                        LogLevel::Success => theme::green(),
                                        LogLevel::Warning => theme::yellow(),
                                        LogLevel::Error => theme::red(),
                                    };
                                    let seconds = line.at.as_secs();
                                    ui.label(
                                        RichText::new(format!(
                                            "[{:02}:{:02}] {}",
                                            seconds / 60,
                                            seconds % 60,
                                            line.text
                                        ))
                                        .monospace()
                                        .size(11.0)
                                        .color(color),
                                    );
                                }
                            });
                    }
                    ConsoleTab::Rust => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label(RichText::new(index_line).monospace().size(11.5));
                                ui.label(RichText::new(classpath_line).monospace().size(11.5));
                                ui.label(
                                    RichText::new(format!(
                                        "Semântica: {} import(s) não resolvido(s) · tema {}",
                                        self.semantic_problems.len(),
                                        self.theme.label()
                                    ))
                                    .monospace()
                                    .size(11.5)
                                    .color(theme::muted()),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "Plugins: {} · fonte {:.1} pt · abas {}",
                                        self.plugins.plugin_count(),
                                        self.font_size,
                                        self.tabs.len()
                                    ))
                                    .monospace()
                                    .size(11.5)
                                    .color(theme::muted()),
                                );
                            });
                    }
                }
            });
        if clear {
            self.console.clear();
        }
        if close {
            self.show_console = false;
        }
    }

    fn status_bar(&mut self, ctx: &Context) {
        egui::TopBottomPanel::bottom("reclipse_status")
            .exact_height(27.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(toast) = &self.status_message {
                        ui.label(RichText::new(&toast.text).color(theme::accent()));
                    } else {
                        if let Some(project) = &self.project {
                            ui.label(RichText::new(&project.name).strong());
                            ui.separator();
                        }
                        ui.label(
                            RichText::new("RUST")
                                .monospace()
                                .size(10.0)
                                .strong()
                                .color(theme::accent()),
                        );
                        ui.separator();
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
                                    ui.label(RichText::new("não salvo").color(theme::yellow()));
                                    ui.separator();
                                }
                            }
                        }
                        ui.label(format!("{} plugin(s)", self.plugins.plugin_count()));
                        ui.separator();
                        ui.label("UTF-8");
                        ui.separator();
                        ui.label(RichText::new(&self.theme_label).color(theme::muted()))
                            .on_hover_text(self.theme.label());
                    });
                });
            });
    }

    fn classpath_status_ui(&self, ui: &mut Ui) {
        match &self.classpath_status {
            ClasspathStatus::Idle => {
                ui.label(RichText::new("cp: —").small().color(theme::muted()));
            }
            ClasspathStatus::Indexing { done, total } => {
                ui.spinner();
                let label = if *total == 0 {
                    "classpath: resolvendo…".to_owned()
                } else {
                    format!("classpath: {done}/{total} archives")
                };
                ui.label(RichText::new(label).small().color(theme::muted()));
            }
            ClasspathStatus::Ready(stats) => {
                let target = stats
                    .jdk_target
                    .map(|target| format!(" · Java {target}"))
                    .unwrap_or_default();
                ui.label(
                    RichText::new(format!(
                        "cp: {} archives · {} classes · {} ms{}",
                        stats.archives, stats.classes, stats.elapsed_ms, target
                    ))
                    .small()
                    .color(theme::muted()),
                )
                .on_hover_text(format!(
                    "JDK: {} · {} warning(s) de classpath",
                    stats.jdk_version.as_deref().unwrap_or("não encontrado"),
                    self.classpath_index
                        .as_ref()
                        .map(|index| index.warnings.len())
                        .unwrap_or(0)
                ));
            }
            ClasspathStatus::Failed(error) => {
                ui.label(
                    RichText::new("classpath falhou")
                        .small()
                        .color(theme::red()),
                )
                .on_hover_text(error);
            }
        }
    }

    fn index_status_ui(&self, ui: &mut Ui) {
        match &self.index_status {
            IndexStatus::Idle => {
                ui.label(RichText::new("Pronto").color(theme::muted()));
            }
            IndexStatus::Indexing { done, total } => {
                ui.spinner();
                if *total == 0 {
                    ui.label(RichText::new("Indexando…").color(theme::muted()));
                } else {
                    ui.label(
                        RichText::new(format!("Indexando {done}/{total} arquivos Java"))
                            .color(theme::muted()),
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
                    .color(theme::muted()),
                );
            }
            IndexStatus::Failed(error) => {
                ui.label(RichText::new("Falha na indexação").color(theme::red()))
                    .on_hover_text(error);
            }
        }
    }

    fn left_panel(&mut self, ctx: &Context) {
        if !(self.show_project || self.show_problems) {
            return;
        }

        let mut actions = Vec::new();
        egui::SidePanel::left("reclipse_left")
            .resizable(true)
            .min_width(220.0)
            .max_width(520.0)
            .default_width(280.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.show_project {
                            self.project_section(ui, &mut actions);
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

    fn right_panel(&mut self, ctx: &Context) {
        if !self.show_outline {
            return;
        }
        let mut actions = Vec::new();
        egui::SidePanel::right("reclipse_right")
            .resizable(true)
            .min_width(220.0)
            .max_width(440.0)
            .default_width(260.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::panel())
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.outline_section(ui, &mut actions);
                        ui.add_space(10.0);
                        self.rust_telemetry_card(ui);
                        self.classpath_warnings_card(ui);
                    });
            });
        for action in actions {
            self.execute_action(action);
        }
    }

    fn rust_telemetry_card(&self, ui: &mut Ui) {
        egui::Frame::NONE
            .fill(theme::panel_alt())
            .stroke(egui::Stroke::new(1.0, theme::border()))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .corner_radius(6.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("RUST ENGINE")
                            .font(theme::display_font(10.0))
                            .color(theme::accent()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("ativo").small().color(theme::green()));
                    });
                });
                ui.separator();
                let (files, symbols, elapsed) = match &self.index_status {
                    IndexStatus::Ready {
                        files,
                        symbols,
                        elapsed_ms,
                        ..
                    } => (
                        files.to_string(),
                        symbols.to_string(),
                        format!("{elapsed_ms} ms"),
                    ),
                    IndexStatus::Indexing { .. } => {
                        ("…".to_owned(), "…".to_owned(), "…".to_owned())
                    }
                    IndexStatus::Failed(_) => ("erro".to_owned(), "—".to_owned(), "—".to_owned()),
                    IndexStatus::Idle => ("—".to_owned(), "—".to_owned(), "—".to_owned()),
                };
                telemetry_row(ui, "arquivos", &files);
                telemetry_row(ui, "símbolos", &symbols);
                telemetry_row(ui, "índice", &elapsed);
                let cp = match &self.classpath_status {
                    ClasspathStatus::Ready(stats) => {
                        format!("{} classes", stats.classes)
                    }
                    ClasspathStatus::Indexing { .. } => "indexando…".to_owned(),
                    ClasspathStatus::Failed(_) => "falhou".to_owned(),
                    ClasspathStatus::Idle => "—".to_owned(),
                };
                telemetry_row(ui, "classpath", &cp);
                telemetry_row(
                    ui,
                    "semântica",
                    &format!("{} import(s)", self.semantic_problems.len()),
                );
                telemetry_row(ui, "tema", self.theme.compact_label());
                telemetry_row(ui, "plugins", &self.plugins.plugin_count().to_string());
            });
    }

    fn classpath_warnings_card(&self, ui: &mut Ui) {
        let Some(classpath) = &self.classpath_index else {
            return;
        };
        if classpath.warnings.is_empty() {
            return;
        }
        ui.add_space(8.0);
        egui::Frame::NONE
            .fill(theme::panel_alt())
            .stroke(egui::Stroke::new(1.0, theme::border()))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .corner_radius(6.0)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "CLASSPATH · {} warning(s)",
                        classpath.warnings.len()
                    ))
                    .font(theme::display_font(10.0))
                    .color(theme::yellow()),
                );
                ui.separator();
                for warning in classpath.warnings.iter().take(6) {
                    ui.label(RichText::new(warning).small().color(theme::muted()));
                }
            });
    }

    fn project_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "PROJETO");
        let Some(root) = self.root.clone() else {
            ui.label(RichText::new("Nenhum projeto aberto").color(theme::muted()));
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
                .color(theme::muted()),
            );
        } else if matches!(self.index_status, IndexStatus::Indexing { .. }) {
            ui.label(
                RichText::new("descobrindo fontes…")
                    .small()
                    .color(theme::muted()),
            );
        }

        if let Some(path) = self.tree.show(ui, &root) {
            actions.push(Action::OpenPath(path));
        }
    }

    fn outline_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "ESTRUTURA");
        let Some(active) = self.active_tab else {
            ui.label(RichText::new("Nenhum arquivo ativo").color(theme::muted()));
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let Some(index) = &self.index else {
            ui.label(RichText::new("Índice em construção…").color(theme::muted()));
            return;
        };
        let Some(file) = index.symbols_for(&path) else {
            ui.label(
                RichText::new("Este arquivo ainda não está no índice (salve-o)")
                    .small()
                    .color(theme::muted()),
            );
            return;
        };
        let package = file.package.clone();
        let total_symbols = file.symbols.len();
        let symbols: Vec<_> = file.symbols.iter().take(1_000).cloned().collect();

        if let Some(package) = package {
            ui.label(RichText::new(package).small().color(theme::muted()));
        }
        if symbols.is_empty() {
            ui.label(RichText::new("Nenhum símbolo encontrado").color(theme::muted()));
            return;
        }

        for symbol in symbols {
            let color = match symbol.kind {
                SymbolKind::Class
                | SymbolKind::Interface
                | SymbolKind::Enum
                | SymbolKind::Record
                | SymbolKind::Annotation => theme::clr_type(),
                SymbolKind::Method | SymbolKind::Constructor => theme::yellow(),
                _ => theme::muted(),
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
                    .color(theme::muted()),
            );
        }
    }

    fn problems_section(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        section_header(ui, "PROBLEMAS");
        let Some(index) = &self.index else {
            ui.label(RichText::new("Sem dados de indexação").color(theme::muted()));
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
                .color(if total == 0 {
                    theme::green()
                } else {
                    theme::red()
                }),
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
                .selectable_label(false, RichText::new(label).size(12.0).color(theme::red()))
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
                    .color(theme::yellow()),
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
                    .selectable_label(
                        false,
                        RichText::new(label).size(12.0).color(theme::yellow()),
                    )
                    .clicked()
                {
                    actions.push(Action::OpenAt(problem.file, problem.line, 1));
                }
            }
        }
    }

    fn central_panel(&mut self, ctx: &Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::editor_bg()))
            .show(ctx, |ui| {
                self.tab_bar(ui);
                self.breadcrumb(ui);
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

    fn breadcrumb(&self, ui: &mut Ui) {
        let Some(active) = self.active_tab else {
            return;
        };
        let Some(tab) = self.tabs.get(active) else {
            return;
        };
        let path = &tab.doc.path;
        let project_name = self
            .project
            .as_ref()
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "workspace".to_owned());
        let package = self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(path))
            .and_then(|file| file.package.clone());
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let cursor_line = tab.cursor_line;
        let symbol = self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(path))
            .and_then(|file| {
                file.symbols
                    .iter()
                    .filter(|symbol| symbol.line <= cursor_line)
                    .max_by_key(|symbol| (symbol.line, symbol.depth))
            })
            .map(|symbol| symbol.name.clone());

        egui::Frame::NONE
            .fill(theme::panel())
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(project_name).small().color(theme::muted()));
                    ui.label(RichText::new("›").small().color(theme::border()));
                    if let Some(package) = package {
                        ui.label(RichText::new(package).small().color(theme::muted()));
                        ui.label(RichText::new("›").small().color(theme::border()));
                    }
                    ui.label(RichText::new(file_name).small().color(theme::text()));
                    if let Some(symbol) = symbol {
                        ui.label(RichText::new("›").small().color(theme::border()));
                        ui.label(RichText::new(symbol).small().color(theme::accent()));
                    }
                });
            });
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut activate = None;
        let mut close = None;
        let mut open_folder = false;

        egui::Frame::NONE
            .fill(theme::panel())
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
                                        .color(theme::muted()),
                                );
                                if ui.small_button("Abrir projeto…").clicked() {
                                    open_folder = true;
                                }
                                return;
                            }
                            for (index, tab) in self.tabs.iter().enumerate() {
                                let is_active = self.active_tab == Some(index);
                                ui.label(
                                    RichText::new("J")
                                        .monospace()
                                        .size(10.0)
                                        .strong()
                                        .color(theme::clr_type()),
                                );
                                let title = tab.title();
                                let text = RichText::new(title).size(12.5).color(if is_active {
                                    theme::text()
                                } else {
                                    theme::muted()
                                });
                                let response = ui.selectable_label(is_active, text);
                                if is_active {
                                    let rect = response.rect;
                                    ui.painter().line_segment(
                                        [
                                            egui::Pos2::new(rect.left(), rect.bottom()),
                                            egui::Pos2::new(rect.right(), rect.bottom()),
                                        ],
                                        egui::Stroke::new(2.0, theme::accent()),
                                    );
                                }
                                if response.clicked() {
                                    activate = Some(index);
                                }
                                response.on_hover_text(tab.doc.path.display().to_string());
                                if tab.doc.dirty {
                                    ui.label(RichText::new("●").size(8.0).color(theme::yellow()))
                                        .on_hover_text("alterações não salvas");
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("×").size(12.0).color(theme::muted()),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_text("Fechar aba")
                                    .clicked()
                                {
                                    close = Some(index);
                                }
                                ui.add_space(4.0);
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
        let theme_kind = self.theme;
        ui.vertical_centered(|ui| {
            ui.add_space(42.0);
            draw_emblem(ui, theme_kind, 76.0);
            ui.add_space(6.0);
            ui.label(
                RichText::new("RECLIPSE Studio")
                    .font(theme::display_font(36.0))
                    .strong()
                    .color(theme::accent()),
            );
            ui.label(
                RichText::new("Rust-native Java IDE · design system v1.0")
                    .size(14.0)
                    .color(theme::muted()),
            );
            ui.add_space(22.0);

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
                        .color(theme::text()),
                );
                ui.label(
                    RichText::new(format!(
                        "{} fontes Java · {} arquivos no projeto",
                        project.java_files.len(),
                        project.all_files.len()
                    ))
                    .small()
                    .color(theme::muted()),
                );
            } else if matches!(self.index_status, IndexStatus::Indexing { .. }) {
                ui.spinner();
                ui.label(RichText::new("Indexando o projeto…").color(theme::muted()));
            } else {
                ui.label(
                    RichText::new("Abra uma pasta com um projeto Java para começar")
                        .color(theme::muted()),
                );
            }

            ui.add_space(36.0);
            ui.separator();
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "Ctrl+Shift+R  recurso   Ctrl+Shift+T  tipo   Ctrl+Shift+G  referências   Alt+Shift+R  rename   F3  definição   Ctrl+Space  completion",
                )
                .small()
                .color(theme::muted()),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("MVP nativo: tree-sitter-java · egui · plugins Rust ABI v1")
                    .small()
                    .color(theme::muted()),
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
                            ui.label(RichText::new("Nenhum resultado").color(theme::muted()));
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
                        .color(theme::muted()),
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
        let source = self
            .classpath_index
            .as_ref()
            .and_then(|index| index.source_location(&name))
            .and_then(|location| reclipse_classpath::read_source(&location).ok());
        let has_source = source.is_some();
        let mut open_source = false;
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
                        .color(theme::accent()),
                );
                ui.label(
                    RichText::new(format!(
                        "{} · {} · origem: {}",
                        class.kind.label(),
                        class.package,
                        class.origin
                    ))
                    .small()
                    .color(theme::muted()),
                );
                if let Some(super_name) = &class.super_name {
                    ui.label(
                        RichText::new(format!("extends {super_name}"))
                            .small()
                            .color(theme::muted()),
                    );
                }
                if !class.interfaces.is_empty() {
                    ui.label(
                        RichText::new(format!("implements {}", class.interfaces.join(", ")))
                            .small()
                            .color(theme::muted()),
                    );
                }
                ui.separator();
                if has_source && ui.button("Ver código-fonte").clicked() {
                    open_source = true;
                }
                ui.label(
                    RichText::new(if has_source {
                        "Fontes anexados disponíveis."
                    } else {
                        "Metadados do classpath; código-fonte não disponível."
                    })
                    .small()
                    .color(if has_source {
                        theme::green()
                    } else {
                        theme::yellow()
                    }),
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
        if open_source {
            if let Some(source) = source {
                self.classpath_source = Some((class.name.clone(), source));
            }
        }
    }

    fn show_classpath_source(&mut self, ctx: &Context) {
        let Some((name, _)) = self.classpath_source.as_ref() else {
            return;
        };
        let name = name.clone();
        let Some((_, source)) = self.classpath_source.as_mut() else {
            return;
        };
        let mut open = true;
        egui::Window::new(format!("Código-fonte — {name}"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(900.0, 620.0))
            .show(ctx, |ui| {
                egui::ScrollArea::both()
                    .id_salt("reclipse_classpath_source")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(source)
                                .code_editor()
                                .desired_width(1600.0)
                                .desired_rows(36),
                        );
                    });
            });
        if !open {
            self.classpath_source = None;
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
                    .color(theme::muted()),
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
                                    .color(theme::accent()),
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
                    .color(theme::muted()),
                );
                ui.label(
                    RichText::new(
                        "Folding: Ctrl+Shift+[ dobrar · Ctrl+Shift+] desdobrar · Ctrl+Shift+0/9 recolher/expandir tudo",
                    )
                    .small()
                    .color(theme::muted()),
                );
                ui.label(
                    RichText::new(
                        "Completion: Ctrl+Space abre sugestões · digitar após `.` abre membros · Enter/Tab aceita",
                    )
                    .small()
                    .color(theme::muted()),
                );
                ui.label(
                    RichText::new("Referências semânticas: Ctrl+Shift+G · rename: Alt+Shift+R")
                        .small()
                        .color(theme::muted()),
                );
                ui.label(
                    RichText::new(
                        "Busca: Enter próximo · Shift+Enter anterior · Ctrl+F localizar · Ctrl+H substituir",
                    )
                    .small()
                    .color(theme::muted()),
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
                        .color(theme::accent()),
                );
                ui.label(
                    RichText::new("Uma IDE Java nativa, rápida e extensível em Rust.")
                        .color(theme::text()),
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
                    .color(theme::muted()),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Projeto independente; não afiliado à Eclipse Foundation.")
                        .small()
                        .color(theme::muted()),
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
            Action::Rename => self.begin_rename(),
            Action::FindReferences => self.find_references(),
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
                if self.project.is_some()
                    && matches!(self.classpath_status, ClasspathStatus::Indexing { .. })
                {
                    ctx.request_repaint_after(Duration::from_millis(60));
                    return;
                }
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
                    if !self.screenshot.completion_done {
                        if let Some(marker) = self.screenshot.completion_trigger.clone() {
                            if self.classpath_index.is_some() {
                                let trigger = self.active_tab.and_then(|active| {
                                    let text = self.tabs[active].doc.text();
                                    text.find(&marker)
                                        .map(|position| (active, position + marker.len()))
                                });
                                if let Some((active, byte_position)) = trigger {
                                    let text = self.tabs[active].doc.text();
                                    let character = text[..byte_position].chars().count();
                                    self.tabs[active].doc.set_cursor(character, false);
                                    self.tabs[active].pending_cursor = Some(character);
                                    self.open_completion(false);
                                    self.screenshot.completion_done = true;
                                }
                            }
                        }
                    }
                    if !self.screenshot.references_done {
                        if let Some(word) = self.screenshot.references_word.clone() {
                            if let Some(active) = self.active_tab {
                                let text = self.tabs[active].doc.text();
                                if let Some(position) = text.find(&word) {
                                    let inside = position
                                        + word
                                            .chars()
                                            .next()
                                            .map(|character| character.len_utf8())
                                            .unwrap_or(0);
                                    let character = text[..inside].chars().count();
                                    self.tabs[active].doc.set_cursor(character, false);
                                    self.tabs[active].pending_cursor = Some(character);
                                    self.find_references();
                                    self.screenshot.references_done = true;
                                }
                            }
                        }
                    }
                    if !self.screenshot.rename_done {
                        if let Some(word) = self.screenshot.rename_word.clone() {
                            if let Some(active) = self.active_tab {
                                let text = self.tabs[active].doc.text();
                                if let Some(position) = text.rfind(&word) {
                                    let inside = position
                                        + word
                                            .chars()
                                            .next()
                                            .map(|character| character.len_utf8())
                                            .unwrap_or(0);
                                    let character = text[..inside].chars().count();
                                    self.tabs[active].doc.set_cursor(character, false);
                                    self.tabs[active].pending_cursor = Some(character);
                                    self.begin_rename();
                                    if let (Some(state), Some(name)) = (
                                        self.rename.as_mut(),
                                        self.screenshot.rename_new_name.clone(),
                                    ) {
                                        state.new_name = name;
                                    }
                                    self.screenshot.rename_done = true;
                                }
                            }
                        }
                    }
                    if self.classpath_source.is_none() {
                        if let Some(name) = self.screenshot.source_class.clone() {
                            if let Some(location) = self
                                .classpath_index
                                .as_ref()
                                .and_then(|index| index.source_location(&name))
                            {
                                if let Ok(source) = reclipse_classpath::read_source(&location) {
                                    self.classpath_source = Some((name, source));
                                }
                            }
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
        self.maybe_reload_classpath();
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
        self.quick_toolbar(ctx);
        self.status_bar(ctx);
        self.console_panel(ctx);
        self.external_change_bar(ctx);
        self.search_panel(ctx);
        self.activity_bar(ctx);
        self.left_panel(ctx);
        self.right_panel(ctx);
        self.central_panel(ctx);
        self.maybe_auto_complete();
        self.show_completion(ctx);
        self.show_hover(ctx);
        self.show_rename(ctx);
        self.show_palette(ctx);
        self.show_shortcuts(ctx);
        self.show_classpath_view(ctx);
        self.show_classpath_source(ctx);
        self.show_about(ctx);
        self.show_project_replace_confirm(ctx);
        self.show_recovery(ctx);
        self.show_close_confirm(ctx);
        self.persist_dirty_backups();
        self.run_frame_bench(ctx, frame_start);
        self.maybe_capture_screenshot(ctx);

        if matches!(self.index_status, IndexStatus::Indexing { .. })
            || matches!(self.classpath_status, ClasspathStatus::Indexing { .. })
            || self.classpath_reload_at.is_some()
            || self.project_search.running
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        } else if self.status_message.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [red, green, blue, alpha] = theme::bg().to_array();
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
    ui.label(
        RichText::new(title)
            .font(theme::display_font(11.0))
            .color(theme::muted()),
    );
    ui.separator();
}

fn telemetry_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).small().color(theme::muted()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(value)
                    .small()
                    .monospace()
                    .color(theme::text()),
            );
        });
    });
}

fn toolbar_button(ui: &mut Ui, label: &str, hint: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).size(12.0)).min_size(egui::vec2(0.0, 22.0)))
        .on_hover_text(hint)
        .clicked()
}

fn problem_badge(ui: &mut Ui, label: &str, count: usize, color: Color32) {
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.14))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.45)))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{count} {label}"))
                    .small()
                    .color(color),
            );
        });
}

#[derive(Clone, Copy)]
enum ActivityIcon {
    Project,
    Outline,
    Problems,
    Console,
    Settings,
}

fn activity_button(ui: &mut Ui, icon: ActivityIcon, active: bool, tooltip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(34.0, 32.0), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::hover());
    }
    if active {
        ui.painter().rect_filled(rect, 4.0, theme::accent_dim());
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                rect.left_top() + egui::vec2(0.0, 5.0),
                egui::vec2(2.0, (rect.height() - 10.0).max(2.0)),
            ),
            1.0,
            theme::accent(),
        );
    }
    let color = if active {
        theme::accent()
    } else if response.hovered() {
        theme::text()
    } else {
        theme::muted()
    };
    let painter = ui.painter();
    let center = rect.center();
    let stroke = egui::Stroke::new(1.4, color);
    match icon {
        ActivityIcon::Project => {
            let left = center.x - 7.0;
            let right = center.x + 7.0;
            let top = center.y - 5.0;
            let bottom = center.y + 6.0;
            painter.line_segment(
                [
                    egui::Pos2::new(left, top + 3.0),
                    egui::Pos2::new(left + 4.0, top),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(left + 4.0, top),
                    egui::Pos2::new(right - 2.0, top),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(right - 2.0, top),
                    egui::Pos2::new(right, top + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(right, top + 3.0),
                    egui::Pos2::new(right, bottom),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(right, bottom),
                    egui::Pos2::new(left, bottom),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(left, bottom),
                    egui::Pos2::new(left, top + 3.0),
                ],
                stroke,
            );
        }
        ActivityIcon::Outline => {
            for (index, width) in [0.0_f32, 2.0, -1.0].into_iter().enumerate() {
                let y = center.y - 5.0 + index as f32 * 5.0;
                painter.circle_filled(egui::Pos2::new(center.x - 7.0, y), 1.2, color);
                painter.line_segment(
                    [
                        egui::Pos2::new(center.x - 4.0, y),
                        egui::Pos2::new(center.x + 7.0 + width, y),
                    ],
                    stroke,
                );
            }
        }
        ActivityIcon::Problems => {
            let apex = egui::Pos2::new(center.x, center.y - 7.0);
            painter.line_segment(
                [apex, egui::Pos2::new(center.x - 7.0, center.y + 6.0)],
                stroke,
            );
            painter.line_segment(
                [apex, egui::Pos2::new(center.x + 7.0, center.y + 6.0)],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(center.x - 7.0, center.y + 6.0),
                    egui::Pos2::new(center.x + 7.0, center.y + 6.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(center.x, center.y - 1.0),
                    egui::Pos2::new(center.x, center.y + 2.0),
                ],
                stroke,
            );
            painter.circle_filled(egui::Pos2::new(center.x, center.y + 4.5), 1.0, color);
        }
        ActivityIcon::Console => {
            let rect = egui::Rect::from_center_size(center, egui::vec2(14.0, 11.0));
            painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::Pos2::new(center.x - 4.0, center.y - 2.5),
                    egui::Pos2::new(center.x - 1.0, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(center.x - 1.0, center.y),
                    egui::Pos2::new(center.x - 4.0, center.y + 2.5),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::Pos2::new(center.x + 1.0, center.y + 2.5),
                    egui::Pos2::new(center.x + 5.0, center.y + 2.5),
                ],
                stroke,
            );
        }
        ActivityIcon::Settings => {
            painter.circle_stroke(center, 4.0, stroke);
            for i in 0..4 {
                let angle = std::f32::consts::FRAC_PI_4 + i as f32 * std::f32::consts::FRAC_PI_2;
                let direction = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment([center + direction * 5.0, center + direction * 7.5], stroke);
            }
        }
    }
    response.on_hover_text(tooltip)
}

/// Draws the RECLIPSE emblem (corona ring + crab disc + `R`) with painter
/// primitives, tinted by the active theme.
fn draw_emblem(ui: &mut Ui, kind: crate::theme::ThemeKind, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    if !response.rect.is_positive() {
        return response;
    }
    let painter = ui.painter();
    let center = rect.center();
    let radius = size * 0.5;
    let primary = crate::theme::emblem_color(kind, false);
    let corona = crate::theme::emblem_color(kind, true);
    // Crab shell disc.
    painter.circle_filled(center, radius * 0.64, theme::editor_bg());
    painter.circle_stroke(center, radius * 0.64, egui::Stroke::new(1.5, primary));
    // Corona ring (dashed arc).
    let segments = 12_u32;
    for i in 0..segments {
        if i % 2 == 1 {
            continue;
        }
        let a0 = std::f32::consts::TAU * i as f32 / segments as f32;
        let a1 = std::f32::consts::TAU * (i as f32 + 0.6) / segments as f32;
        let p0 = center + egui::Vec2::new(a0.cos(), a0.sin()) * radius * 0.88;
        let p1 = center + egui::Vec2::new(a1.cos(), a1.sin()) * radius * 0.88;
        painter.line_segment([p0, p1], egui::Stroke::new(1.5, corona));
    }
    // Crescent hint.
    painter.circle_filled(
        center + egui::Vec2::new(radius * 0.28, -radius * 0.10),
        radius * 0.42,
        corona.gamma_multiply(0.35),
    );
    // `R` monogram with painter primitives: vertical stem, bowl, leg.
    let stem_x = center.x - radius * 0.30;
    let top = center.y - radius * 0.36;
    let mid = center.y;
    let bottom = center.y + radius * 0.36;
    let bowl_right = center.x + radius * 0.28;
    let stroke = egui::Stroke::new(2.0, primary);
    painter.line_segment(
        [
            egui::Pos2::new(stem_x, top),
            egui::Pos2::new(stem_x, bottom),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::Pos2::new(stem_x, top),
            egui::Pos2::new(bowl_right, top),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::Pos2::new(stem_x, mid),
            egui::Pos2::new(bowl_right, mid),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::Pos2::new(bowl_right, top),
            egui::Pos2::new(bowl_right, mid),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::Pos2::new(bowl_right, mid),
            egui::Pos2::new(center.x + radius * 0.34, bottom),
        ],
        stroke,
    );
    response
}

fn short_file(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn replace_project_paths(project: &mut Project, rename: Option<&(PathBuf, PathBuf)>) {
    let Some((from, to)) = rename else {
        return;
    };
    for path in project.java_files.iter_mut() {
        if path == from {
            *path = to.clone();
        }
    }
    for path in project.all_files.iter_mut() {
        if path == from {
            *path = to.clone();
        }
    }
}

fn byte_index_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn preview_line(text: &str, line: usize) -> String {
    text.split('\n')
        .nth(line.saturating_sub(1))
        .unwrap_or_default()
        .trim()
        .chars()
        .take(160)
        .collect()
}

fn line_col_of_byte(text: &str, byte: usize) -> (usize, usize) {
    let char_index = text[..byte.min(text.len())].chars().count();
    reclipse_editor::line_col_at_char(text, char_index)
}

fn cursor_is_type_reference(
    file: &reclipse_java::FileSymbols,
    text: &str,
    start_char: usize,
) -> bool {
    let start_byte = byte_index_of_char(text, start_char);
    file.type_refs
        .iter()
        .any(|reference| reference.start_byte == start_byte)
}

fn local_plan_matches(
    path: &Path,
    text: &str,
    plan: &LocalRenamePlan,
    word: &str,
) -> Vec<ProjectMatch> {
    plan.edits
        .iter()
        .map(|edit| {
            let char_index = text[..edit.start_byte.min(text.len())].chars().count();
            let (line, column) = reclipse_editor::line_col_at_char(text, char_index);
            ProjectMatch {
                file: path.to_path_buf(),
                line,
                column,
                start: 0,
                end: word.chars().count(),
                preview: preview_line(text, line),
            }
        })
        .collect()
}

fn is_completion_identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn is_classpath_config(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        "pom.xml"
            | ".classpath"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "gradle.properties"
            | "libs.versions.toml"
            | "org.eclipse.jdt.core.prefs"
    )
}

fn truncate_detail(detail: &str, limit: usize) -> String {
    let count = detail.chars().count();
    if count <= limit {
        return detail.to_owned();
    }
    let mut result: String = detail.chars().take(limit.saturating_sub(3)).collect();
    result.push_str("...");
    result
}

fn hover_resolved(
    word: &str,
    resolved: &ResolvedType,
    classpath: &ClasspathIndex,
) -> (String, Vec<String>) {
    match resolved {
        ResolvedType::Project(location) => {
            let title = format!("{word} · {}", location.kind.label());
            let lines = vec![
                location.fqn.clone(),
                format!("{}:{}", location.path.display(), location.line),
            ];
            (title, lines)
        }
        ResolvedType::Classpath(name) => {
            let Some(class) = classpath.get_loose(name) else {
                return (word.to_owned(), vec![name.clone()]);
            };
            let title = format!("{word} · {}", class.kind.label());
            let mut lines = vec![
                class.name.clone(),
                format!("pacote {} · {}", class.package, class.origin),
            ];
            if let Some(super_name) = &class.super_name {
                lines.push(format!("extends {super_name}"));
            }
            if !class.interfaces.is_empty() {
                lines.push(format!("implements {}", class.interfaces.join(", ")));
            }
            (title, lines)
        }
    }
}

fn show_hover_popup(ctx: &Context, position: egui::Pos2, title: &str, lines: &[String]) {
    let screen = ctx.screen_rect();
    let mut position = position + egui::vec2(14.0, 18.0);
    position.x = position.x.clamp(
        screen.left() + 8.0,
        (screen.right() - 440.0).max(screen.left() + 8.0),
    );
    position.y = position.y.clamp(
        screen.top() + 8.0,
        (screen.bottom() - 160.0).max(screen.top() + 8.0),
    );
    egui::Area::new(egui::Id::new(("reclipse_hover", title)))
        .order(egui::Order::Foreground)
        .interactable(false)
        .fixed_pos(position)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(theme::panel())
                .stroke(egui::Stroke::new(1.0_f32, theme::border()))
                .inner_margin(7.0)
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        RichText::new(title)
                            .monospace()
                            .strong()
                            .color(theme::accent()),
                    );
                    for line in lines {
                        ui.label(RichText::new(line).small().color(theme::muted()));
                    }
                });
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_classpath_configuration_files() {
        assert!(is_classpath_config(Path::new("project/pom.xml")));
        assert!(is_classpath_config(Path::new("project/.classpath")));
        assert!(is_classpath_config(Path::new("project/build.gradle.kts")));
        assert!(is_classpath_config(Path::new(
            "project/gradle/libs.versions.toml"
        )));
        assert!(!is_classpath_config(Path::new("project/App.java")));
    }
}
