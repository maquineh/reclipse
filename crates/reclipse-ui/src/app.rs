//! Application state and egui composition.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use eframe::CreationContext;
use egui::{Align, Color32, Context, Key, Layout, Modifiers, RichText, Ui};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use reclipse_classpath::{ClasspathIndex, ClasspathStats};
use reclipse_core::{discover_project, Project, IGNORED_DIRS};
use reclipse_debug::{
    BreakpointSpec, DebugCommand, DebugEvent, DebugSession, FrameInfo, SourceLocation, StepKind,
    ThreadInfo, VariableInfo,
};
use reclipse_editor::{Document, SearchMatcher, SearchOptions};
use reclipse_java::refactor::{
    apply_text_edits, local_binding_at, organize_imports, plan_local_rename, plan_member_rename,
    plan_package_rename, plan_type_rename, LocalRenamePlan, RenameKind, RenamePlan,
};
use reclipse_java::semantic::{
    completion_context, find_member_references, find_type_references, infer_variable_type,
    member_target_at, resolve_type as resolve_semantic_type, unresolved_imports, CompletionItem,
    CompletionKind, MemberTarget, ProjectTypes, ResolvedType,
};
use reclipse_java::{build_index, IndexedProblem, ProjectIndex, SymbolKind};
use reclipse_plugin_api::InvokeContext;
use reclipse_plugin_host::{PluginCommand, PluginHost};

use crate::build::{self, BuildEnv};
use crate::editor_view::Tab;
use crate::keymap::{KeyAction, Keymap};
use crate::palette::{rank, Action, Palette, PaletteEntry, PaletteMode};
use crate::search::{project_files, FileSearch, ProjectMatch, ProjectSearch};
use crate::session::Keybindings;
use crate::theme;
use crate::tree::FileTree;

const CONSOLE_MAX_LINES: usize = 600;
const CONSOLE_MAX_LINE_CHARS: usize = 4096;

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

enum BuildMessage {
    Output(String),
    Compiled(Box<build::CompileResult>),
    DebugStarted {
        session: Box<DebugSession>,
        classes: Box<build::CompileResult>,
        main: String,
    },
    RunFinished {
        compile: Box<build::CompileResult>,
        run: build::RunResult,
        main: String,
    },
    TestsFinished(Box<build::TestResult>),
    External {
        label: String,
        success: bool,
        output: String,
        elapsed_ms: u128,
        reindex_classpath: bool,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JdtlsStatus {
    Disabled,
    Starting,
    Ready,
    Failed,
}

#[derive(Debug, Clone)]
struct JdtProblem {
    file: PathBuf,
    line: usize,
    column: usize,
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
    Package { old: String },
    Local { path: PathBuf, cursor: usize },
}

impl Clone for RenameTarget {
    fn clone(&self) -> Self {
        match self {
            Self::Type(fqn) => Self::Type(fqn.clone()),
            Self::Member(target) => Self::Member(target.clone()),
            Self::Package { old } => Self::Package { old: old.clone() },
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

#[derive(Debug, Clone, Copy)]
struct TabDrag {
    index: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugUiStatus {
    Starting,
    Running,
    Paused,
    Exited,
    Failed,
}

impl DebugUiStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "iniciando",
            Self::Running => "executando",
            Self::Paused => "pausado",
            Self::Exited => "encerrado",
            Self::Failed => "falhou",
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Paused)
    }
}

struct DebugUi {
    session: DebugSession,
    status: DebugUiStatus,
    detail: String,
    threads: Vec<ThreadInfo>,
    frames: Vec<FrameInfo>,
    variables: Vec<VariableInfo>,
    selected_frame: usize,
    location: Option<SourceLocation>,
    paused_thread: Option<u64>,
    eval_input: String,
    eval_result: Option<String>,
    exceptions: bool,
    breakpoint_errors: Vec<String>,
}

struct BreakpointConditionEdit {
    path: PathBuf,
    line: usize,
    text: String,
}

type HotSwapResult = Result<Vec<(String, Vec<u8>)>, String>;

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
    run_project: bool,
    run_done: bool,
    split: bool,
    split_done: bool,
    search_query: Option<String>,
    project_search_query: Option<String>,
    debug: bool,
    debug_done: bool,
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
    javac_problems: Vec<build::JavacDiagnostic>,
    main_classes: Vec<String>,
    build_running: bool,
    build_rx: Option<Receiver<BuildMessage>>,
    build_output_buffer: String,
    build_cancel: Option<Arc<AtomicBool>>,
    build_label: Option<String>,
    javac_diag_pending_at: Option<Instant>,
    javac_diag_rx: Option<Receiver<build::CompileResult>>,
    javac_diag_cancel: Option<Arc<AtomicBool>>,
    javac_missing_warned: bool,
    jdtls: Option<crate::jdtls::JdtlsHandle>,
    jdtls_status: JdtlsStatus,
    jdtls_status_message: Option<String>,
    jdtls_problems: Vec<JdtProblem>,
    jdtls_versions: HashMap<PathBuf, i32>,
    jdtls_failed_notified: bool,
    last_build: Option<(bool, u128)>,
    last_run_main: Option<String>,
    launch: build::LaunchConfig,
    launch_open: bool,
    test_failures: Vec<build::TestFailure>,
    test_targets: Vec<reclipse_java::JavaTestMethod>,
    debug: Option<DebugUi>,
    debug_classes: Option<build::CompileResult>,
    debug_pause_exceptions: bool,
    breakpoints: BTreeMap<PathBuf, BTreeSet<usize>>,
    breakpoint_conditions: HashMap<(PathBuf, usize), String>,
    breakpoint_condition_edit: Option<BreakpointConditionEdit>,
    watches: Vec<String>,
    watch_input: String,
    watch_results: Vec<reclipse_debug::WatchValue>,
    hot_swap_running: bool,
    hot_swap_rx: Option<Receiver<HotSwapResult>>,
    source_signatures: HashMap<String, PathBuf>,
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
    secondary_tab: Option<usize>,
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
    console_dropped: usize,
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
            javac_problems: Vec::new(),
            main_classes: Vec::new(),
            build_running: false,
            build_rx: None,
            build_output_buffer: String::new(),
            build_cancel: None,
            build_label: None,
            javac_diag_pending_at: None,
            javac_diag_rx: None,
            javac_diag_cancel: None,
            javac_missing_warned: false,
            jdtls: None,
            jdtls_status: JdtlsStatus::Disabled,
            jdtls_status_message: None,
            jdtls_problems: Vec::new(),
            jdtls_versions: HashMap::new(),
            jdtls_failed_notified: false,
            last_build: None,
            last_run_main: None,
            launch: session.launch.clone(),
            launch_open: false,
            test_failures: Vec::new(),
            test_targets: Vec::new(),
            debug: None,
            debug_classes: None,
            debug_pause_exceptions: false,
            breakpoints: {
                let mut map: BTreeMap<PathBuf, BTreeSet<usize>> = BTreeMap::new();
                for breakpoint in &session.breakpoints {
                    if breakpoint.line > 0 {
                        map.entry(breakpoint.path.clone())
                            .or_default()
                            .insert(breakpoint.line);
                    }
                }
                map
            },
            breakpoint_conditions: session
                .breakpoints
                .iter()
                .filter(|breakpoint| !breakpoint.condition.trim().is_empty())
                .map(|breakpoint| {
                    (
                        (breakpoint.path.clone(), breakpoint.line),
                        breakpoint.condition.clone(),
                    )
                })
                .collect(),
            breakpoint_condition_edit: None,
            watches: session.watches.clone(),
            watch_input: String::new(),
            watch_results: Vec::new(),
            hot_swap_running: false,
            hot_swap_rx: None,
            source_signatures: HashMap::new(),
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
            secondary_tab: None,
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
            console: session
                .console_history
                .iter()
                .map(|text| LogLine {
                    at: Duration::ZERO,
                    level: LogLevel::Info,
                    text: text.clone(),
                })
                .collect(),
            console_dropped: 0,
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
                run_project: std::env::var_os("RECLIPSE_SCREENSHOT_RUN").is_some(),
                run_done: false,
                split: std::env::var_os("RECLIPSE_SCREENSHOT_SPLIT").is_some(),
                split_done: false,
                search_query: std::env::var("RECLIPSE_SCREENSHOT_SEARCH").ok(),
                project_search_query: std::env::var("RECLIPSE_SCREENSHOT_PROJECT_SEARCH").ok(),
                debug: std::env::var_os("RECLIPSE_SCREENSHOT_DEBUG").is_some(),
                debug_done: false,
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
        self.javac_problems.clear();
        self.test_failures.clear();
        self.main_classes.clear();
        self.last_build = None;
        self.last_run_main = None;
        self.build_cancel = None;
        self.build_label = None;
        self.javac_diag_pending_at = None;
        self.javac_diag_rx = None;
        self.javac_diag_cancel = None;
        self.launch_open = false;
        self.jdtls = None;
        self.jdtls_status = JdtlsStatus::Disabled;
        self.jdtls_status_message = None;
        self.jdtls_problems.clear();
        self.jdtls_versions.clear();
        self.jdtls_failed_notified = false;
        self.classpath_view = None;
        self.classpath_source = None;
        self.classpath_reload_at = None;
        self.completion = None;
        self.last_completion_revision = None;
        self.tabs.clear();
        self.active_tab = None;
        self.secondary_tab = None;
        self.file_search.close();
        self.project_search.close();
        self.watch_ignored.clear();
        self.tree = FileTree::default();
        self.index_status = IndexStatus::Indexing { done: 0, total: 0 };
        self.index_rx = Some(spawn_index(root.clone()));
        self.start_watcher(&root);
        self.maybe_start_jdtls();
        self.log(
            LogLevel::Info,
            format!("Abrindo projeto: {}", root.display()),
        );
    }

    /// Start the opt-in JDT language server (`RECLIPSE_JDTLS`), if configured.
    /// Failures degrade to native diagnostics with a single toast.
    fn maybe_start_jdtls(&mut self) {
        if self.jdtls.is_some() {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let Some(config) = crate::jdtls::JdtlsConfig::from_env(&root) else {
            self.jdtls_status = JdtlsStatus::Disabled;
            return;
        };
        let root_uri = crate::jdtls::file_uri(&root);
        match crate::jdtls::spawn(&config, &root_uri) {
            Ok(handle) => {
                self.jdtls = Some(handle);
                self.jdtls_status = JdtlsStatus::Starting;
                self.jdtls_status_message = Some(format!("JDT: iniciando ({})", config.command));
                self.log(LogLevel::Info, "JDT: modo precisão ativado (opt-in)");
            }
            Err(error) => {
                self.jdtls_status = JdtlsStatus::Failed;
                self.jdtls_status_message = Some(error.clone());
                self.log(LogLevel::Warning, format!("JDT indisponível: {error}"));
            }
        }
    }

    /// Ensure the active Java buffer is known to JDT LS (didOpen on first
    /// sight; saves bump the version via [`Self::jdtls_notify_open`]).
    fn jdtls_announce_active(&mut self) {
        let Some(active) = self.active_tab else {
            return;
        };
        let Some(tab) = self.tabs.get(active) else {
            return;
        };
        if !is_java_path(&tab.doc.path) || self.jdtls_versions.contains_key(&tab.doc.path) {
            return;
        }
        let (path, text) = (tab.doc.path.clone(), tab.doc.text());
        self.jdtls_notify_open(&path, text);
    }

    /// Send the open buffer to JDT LS (didOpen on first sight, didChange on save).
    fn jdtls_notify_open(&mut self, path: &Path, text: String) {
        let Some(handle) = &self.jdtls else {
            return;
        };
        let version = self.jdtls_versions.get(path).copied().unwrap_or(0) + 1;
        let first = !self.jdtls_versions.contains_key(path);
        self.jdtls_versions.insert(path.to_path_buf(), version);
        if first {
            handle.send(crate::jdtls::JdtlsCommand::DidOpen {
                path: path.to_path_buf(),
                text,
                version,
            });
        } else {
            handle.send(crate::jdtls::JdtlsCommand::DidChange {
                path: path.to_path_buf(),
                text,
                version,
            });
        }
    }

    fn poll_jdtls(&mut self) {
        if self.jdtls.is_some() {
            self.jdtls_announce_active();
        }
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(handle) = &self.jdtls {
            loop {
                match handle.events.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        } else {
            return;
        }
        for event in events {
            match event {
                crate::jdtls::JdtlsEvent::Ready => {
                    self.jdtls_status = JdtlsStatus::Ready;
                    self.jdtls_status_message = None;
                    self.log(LogLevel::Success, "JDT: servidor pronto".to_owned());
                    self.toast("JDT: modo precisão ativo");
                }
                crate::jdtls::JdtlsEvent::Log(line) => {
                    self.log(LogLevel::Info, line);
                }
                crate::jdtls::JdtlsEvent::Diagnostics { file, items } => {
                    self.jdtls_problems.retain(|problem| problem.file != file);
                    for item in items.into_iter().take(100) {
                        self.jdtls_problems.push(JdtProblem {
                            file: item.file,
                            line: item.line,
                            column: item.column,
                            message: format!("{}: {}", item.severity, item.message),
                        });
                    }
                    if self.jdtls_problems.len() > 2_000 {
                        self.jdtls_problems.truncate(2_000);
                    }
                    self.show_problems = true;
                }
                crate::jdtls::JdtlsEvent::Exited(reason) => {
                    self.jdtls = None;
                    self.jdtls_status = JdtlsStatus::Failed;
                    self.jdtls_status_message = Some(reason.clone());
                    if !self.jdtls_failed_notified {
                        self.jdtls_failed_notified = true;
                        self.toast(format!("JDT encerrado: {reason}"));
                    }
                    self.log(LogLevel::Warning, format!("JDT encerrado: {reason}"));
                }
            }
        }
        if disconnected && self.jdtls.is_some() {
            self.jdtls = None;
            self.jdtls_status = JdtlsStatus::Failed;
            self.jdtls_status_message = Some("canal JDT interrompido".to_owned());
        }
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
                    self.refresh_source_signatures();
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

                    self.refresh_semantics();
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

    fn open_java_buffers(&self) -> HashMap<PathBuf, String> {
        let mut buffers = HashMap::new();
        for tab in &self.tabs {
            if is_java_path(&tab.doc.path) {
                buffers.insert(tab.doc.path.clone(), tab.doc.text());
            }
        }
        buffers
    }

    fn refresh_semantics(&mut self) {
        let project_types = self.index.as_ref().map(ProjectTypes::build);
        let buffers = self.open_java_buffers();
        let problems = if let (Some(index), Some(types), Some(classpath)) = (
            self.index.as_ref(),
            project_types.as_ref(),
            self.classpath_index.as_ref(),
        ) {
            compute_semantic_problems(Some(index), Some(types), classpath, &buffers)
        } else {
            Vec::new()
        };
        self.project_types = project_types;
        self.semantic_problems = problems;
        self.main_classes = self
            .index
            .as_ref()
            .map(build::main_classes)
            .unwrap_or_default();
        self.test_targets = self
            .index
            .as_ref()
            .map(|index| {
                let mut tests: Vec<_> = index
                    .files
                    .values()
                    .flat_map(|file| file.test_methods.iter().cloned())
                    .collect();
                tests.sort_by(|left, right| {
                    left.class_name
                        .cmp(&right.class_name)
                        .then(left.method.cmp(&right.method))
                });
                tests
            })
            .unwrap_or_default();
        self.refresh_source_signatures();
    }

    fn refresh_source_signatures(&mut self) {
        self.source_signatures = self
            .index
            .as_ref()
            .map(build_source_signatures)
            .unwrap_or_default();
    }

    fn class_name_for_line(&self, path: &Path, line: usize) -> Option<String> {
        if let Some(index) = &self.index {
            if let Some(file) = index.symbols_for(path) {
                let source = fs::read_to_string(path).ok();
                let mut best: Option<&reclipse_java::JavaSymbol> = None;
                for symbol in &file.symbols {
                    if !symbol.kind.is_type()
                        || symbol.name == "<anonymous>"
                        || symbol.name.starts_with('<')
                        || symbol.line > line
                    {
                        continue;
                    }
                    if let Some(source) = &source {
                        if !type_encloses_line(source, symbol.line, line) {
                            continue;
                        }
                    }
                    let better = match best {
                        None => true,
                        Some(current) => {
                            symbol.line > current.line
                                || (symbol.line == current.line && symbol.depth >= current.depth)
                        }
                    };
                    if better {
                        best = Some(symbol);
                    }
                }
                let package = file.package.as_deref().unwrap_or("");
                if let Some(symbol) = best {
                    let chain = symbol
                        .container
                        .as_deref()
                        .filter(|container| !container.is_empty())
                        .map_or_else(
                            || symbol.name.clone(),
                            |container| format!("{container}.{}", symbol.name),
                        );
                    let binary = chain.replace('.', "$");
                    return Some(if package.is_empty() {
                        binary
                    } else {
                        format!("{package}.{binary}")
                    });
                }
            }
        }
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    }

    fn breakpoint_specs(&self) -> Vec<BreakpointSpec> {
        let mut specs = Vec::new();
        let Some(index) = &self.index else {
            return specs;
        };
        for (path, lines) in &self.breakpoints {
            if !index.files.contains_key(path) {
                continue;
            }
            for line in lines {
                if let Some(class_name) = self.class_name_for_line(path, *line) {
                    specs.push(BreakpointSpec {
                        path: path.clone(),
                        line: *line,
                        class_name,
                        condition: self
                            .breakpoint_conditions
                            .get(&(path.clone(), *line))
                            .cloned(),
                    });
                }
            }
        }
        specs
    }

    fn compile_project(&mut self) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de compilar");
            return;
        };
        let env = self
            .classpath_index
            .as_ref()
            .map(BuildEnv::from_classpath)
            .unwrap_or_default();
        self.javac_problems.clear();
        self.test_failures.clear();
        self.build_running = true;
        self.build_label = Some("javac".to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.show_console = true;
        self.log(
            LogLevel::Info,
            format!("javac: compilando {} arquivo(s)…", project.java_files.len()),
        );
        thread::spawn(move || {
            let message = match build::compile(&project, &env, &cancel) {
                Ok(result) => BuildMessage::Compiled(Box::new(result)),
                Err(error) => BuildMessage::Failed(error),
            };
            let _ = sender.send(message);
        });
    }

    fn effective_main(&mut self) -> Option<String> {
        let mut mains = self
            .index
            .as_ref()
            .map(build::main_classes)
            .unwrap_or_default();
        self.main_classes = mains.clone();
        if mains.is_empty() {
            return None;
        }
        let configured = self.launch.main_class.trim().to_owned();
        if !configured.is_empty() {
            if mains.contains(&configured) {
                return Some(configured);
            }
            self.log(
                LogLevel::Warning,
                format!("main configurada {configured} não encontrada; usando {mains:?}"),
            );
        }
        let main = mains.remove(0);
        if !mains.is_empty() {
            self.log(
                LogLevel::Warning,
                format!(
                    "Várias classes main encontradas ({}); executando {main}",
                    mains.len() + 1
                ),
            );
        }
        if self.launch.main_class.trim().is_empty() {
            self.launch.main_class = main.clone();
        }
        Some(main)
    }

    fn run_project(&mut self) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de executar");
            return;
        };
        let Some(main) = self.effective_main() else {
            self.toast("Nenhuma classe com public static void main encontrada");
            return;
        };
        let env = self
            .classpath_index
            .as_ref()
            .map(BuildEnv::from_classpath)
            .unwrap_or_default();
        let launch = self.launch.clone();
        self.last_run_main = Some(main.clone());
        self.javac_problems.clear();
        self.build_running = true;
        self.build_label = Some(format!("exec {main}"));
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.show_console = true;
        self.log(LogLevel::Info, format!("Compilando e executando {main}…"));
        thread::spawn(move || match build::compile(&project, &env, &cancel) {
            Ok(compiled) if compiled.success => {
                match build::run_main(&main, &launch, &env, &compiled, &project.root, &cancel) {
                    Ok(run) => {
                        let _ = sender.send(BuildMessage::RunFinished {
                            compile: Box::new(compiled),
                            run,
                            main,
                        });
                    }
                    Err(error) => {
                        let _ = sender.send(BuildMessage::Compiled(Box::new(compiled)));
                        let _ = sender.send(BuildMessage::Failed(error));
                    }
                }
            }
            Ok(compiled) => {
                let _ = sender.send(BuildMessage::Compiled(Box::new(compiled)));
            }
            Err(error) => {
                let _ = sender.send(BuildMessage::Failed(error));
            }
        });
    }

    fn cancel_build(&mut self) {
        if !self.build_running {
            return;
        }
        if let Some(cancel) = &self.build_cancel {
            cancel.store(true, Ordering::SeqCst);
            self.log(LogLevel::Warning, "Cancelando build/execução…");
        } else {
            self.build_running = false;
            self.build_rx = None;
        }
    }

    fn rerun_last(&mut self) {
        if self.build_running {
            self.toast("Build já em andamento");
            return;
        }
        if self.last_run_main.is_some() {
            self.run_project();
        } else {
            self.toast("Nada para re-executar ainda");
        }
    }

    /// Schedule background `javac` diagnostics (debounced). Called on save;
    /// the actual compile runs in `poll_javac_diagnostics` without blocking
    /// manual builds or the UI thread.
    fn start_debug(&mut self) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        if self
            .debug
            .as_ref()
            .is_some_and(|debug| debug.status.is_active())
        {
            self.toast("Debugger já está ativo");
            return;
        }
        self.debug = None;
        self.debug_classes = None;
        let breakpoints = self.breakpoint_specs();
        let pause = self.debug_pause_exceptions;
        self.show_console = true;
        if !self.launch.debug_attach.trim().is_empty() {
            let address = self.launch.debug_attach.trim().to_owned();
            match DebugSession::attach(address.clone(), breakpoints, pause) {
                Ok(session) => {
                    self.log(LogLevel::Info, format!("Debug: conectando em {address}…"));
                    self.debug = Some(DebugUi {
                        session,
                        status: DebugUiStatus::Starting,
                        detail: format!("attach {address}"),
                        threads: Vec::new(),
                        frames: Vec::new(),
                        variables: Vec::new(),
                        selected_frame: 0,
                        location: None,
                        paused_thread: None,
                        eval_input: String::new(),
                        eval_result: None,
                        exceptions: pause,
                        breakpoint_errors: Vec::new(),
                    });
                    self.toast(format!("Conectando o debugger em {address}"));
                }
                Err(error) => {
                    self.log(LogLevel::Error, format!("Debug: {error}"));
                    self.toast(error);
                }
            }
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de depurar");
            return;
        };
        let Some(main) = self.effective_main() else {
            self.toast("Nenhuma classe com public static void main encontrada");
            return;
        };
        let env = self
            .classpath_index
            .as_ref()
            .map(BuildEnv::from_classpath)
            .unwrap_or_default();
        let launch = self.launch.clone();
        self.build_running = true;
        self.build_label = Some(format!("debug {main}"));
        self.build_output_buffer.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.log(
            LogLevel::Info,
            format!("Compilando e preparando o debugger para {main}…"),
        );
        thread::spawn(move || match build::compile(&project, &env, &cancel) {
            Ok(compiled) if compiled.success => {
                let launched = build::java_binary(&env).and_then(|java| {
                    let spec = reclipse_debug::LaunchSpec {
                        java,
                        classpath: build::runtime_classpath_string(&compiled, &env, &project.root),
                        main_class: main.clone(),
                        jvm_args: launch.jvm_args_list(),
                        program_args: launch.program_args_list(),
                        env: launch.env_map(),
                        workdir: project.root.clone(),
                    };
                    DebugSession::launch(spec, breakpoints, pause)
                });
                match launched {
                    Ok(session) => {
                        let _ = sender.send(BuildMessage::DebugStarted {
                            session: Box::new(session),
                            classes: Box::new(compiled),
                            main,
                        });
                    }
                    Err(error) => {
                        let _ = sender.send(BuildMessage::Compiled(Box::new(compiled)));
                        let _ = sender.send(BuildMessage::Failed(error));
                    }
                }
            }
            Ok(compiled) => {
                let _ = sender.send(BuildMessage::Compiled(Box::new(compiled)));
            }
            Err(error) => {
                let _ = sender.send(BuildMessage::Failed(error));
            }
        });
    }

    fn debug_continue(&mut self) {
        let Some(debug) = self.debug.as_mut() else {
            self.toast("Nenhuma sessão de debug ativa");
            return;
        };
        if debug.status == DebugUiStatus::Paused {
            debug.session.send(DebugCommand::Resume);
            debug.status = DebugUiStatus::Running;
            debug.detail = "executando…".to_owned();
        }
    }

    fn debug_pause(&mut self) {
        if let Some(debug) = self.debug.as_mut() {
            if debug.status == DebugUiStatus::Running {
                debug.session.send(DebugCommand::Pause);
            }
        }
    }

    fn debug_step(&mut self, kind: StepKind) {
        let Some(debug) = self.debug.as_mut() else {
            return;
        };
        if debug.status == DebugUiStatus::Paused {
            debug.session.send(DebugCommand::Step(kind));
            debug.status = DebugUiStatus::Running;
            debug.detail = "passo…".to_owned();
        }
    }

    fn stop_debug(&mut self) {
        if let Some(debug) = self.debug.take() {
            debug.session.send(DebugCommand::Disconnect);
            self.log(LogLevel::Info, "Debug: sessão encerrada");
        }
    }

    fn debug_hot_swap(&mut self) {
        if self.hot_swap_running {
            self.toast("Hot swap já está compilando");
            return;
        }
        let Some(debug) = &self.debug else {
            self.toast("Nenhuma sessão de debug ativa");
            return;
        };
        if !debug.status.is_active() {
            self.toast("Hot swap requer uma sessão de debug ativa");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes do hot swap");
            return;
        };
        let env = self
            .classpath_index
            .as_ref()
            .map(BuildEnv::from_classpath)
            .unwrap_or_default();
        let (sender, receiver) = mpsc::channel();
        self.hot_swap_rx = Some(receiver);
        self.hot_swap_running = true;
        self.log(
            LogLevel::Info,
            "Hot swap: recompilando o projeto para redefinir classes carregadas…",
        );
        thread::spawn(move || {
            let cancel = AtomicBool::new(false);
            let result = match build::compile(&project, &env, &cancel) {
                Ok(compiled) if compiled.success => build::compiled_classes(&compiled),
                Ok(compiled) => Err(format!(
                    "compilação falhou ({} diagnóstico(s))",
                    compiled.diagnostics.len()
                )),
                Err(error) => Err(error),
            };
            let _ = sender.send(result);
        });
    }

    fn poll_hot_swap(&mut self) {
        let Some(receiver) = &self.hot_swap_rx else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(classes)) => {
                self.hot_swap_rx = None;
                self.hot_swap_running = false;
                let count = classes.len();
                if let Some(debug) = &self.debug {
                    debug
                        .session
                        .send(DebugCommand::RedefineClasses { classes });
                    self.log(
                        LogLevel::Info,
                        format!("Hot swap: enviando {count} classe(s) compilada(s)…"),
                    );
                } else {
                    self.log(
                        LogLevel::Warning,
                        "Hot swap: sessão encerrada antes de redefinir as classes",
                    );
                }
            }
            Ok(Err(error)) => {
                self.hot_swap_rx = None;
                self.hot_swap_running = false;
                self.log(LogLevel::Error, format!("Hot swap: {error}"));
                self.toast(format!("Hot swap falhou: {error}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.hot_swap_rx = None;
                self.hot_swap_running = false;
            }
        }
    }

    fn toggle_breakpoint(&mut self, path: PathBuf, line: usize) {
        if line == 0 || !is_java_path(&path) {
            return;
        }
        let inserted = {
            let lines = self.breakpoints.entry(path.clone()).or_default();
            if lines.remove(&line) {
                false
            } else {
                lines.insert(line);
                true
            }
        };
        if self.breakpoints.get(&path).is_some_and(BTreeSet::is_empty) {
            self.breakpoints.remove(&path);
        }
        if inserted {
            let Some(class_name) = self.class_name_for_line(&path, line) else {
                self.toast("Não foi possível determinar a classe do breakpoint");
                return;
            };
            if let Some(debug) = self.debug.as_ref() {
                if debug.status.is_active() {
                    debug
                        .session
                        .send(DebugCommand::SetBreakpoint(BreakpointSpec {
                            path: path.clone(),
                            line,
                            class_name,
                            condition: self
                                .breakpoint_conditions
                                .get(&(path.clone(), line))
                                .cloned(),
                        }));
                }
            }
            self.log(
                LogLevel::Info,
                format!(
                    "Breakpoint em {}:{}",
                    short_file(&path.to_string_lossy()),
                    line
                ),
            );
        } else {
            self.breakpoint_conditions.remove(&(path.clone(), line));
            if let Some(debug) = self.debug.as_ref() {
                if debug.status.is_active() {
                    debug.session.send(DebugCommand::ClearBreakpoint {
                        path: path.clone(),
                        line,
                    });
                }
            }
            self.log(
                LogLevel::Info,
                format!(
                    "Breakpoint removido de {}:{}",
                    short_file(&path.to_string_lossy()),
                    line
                ),
            );
        }
    }

    fn update_breakpoint_condition(&mut self, path: PathBuf, line: usize, condition: String) {
        let condition = condition.trim().to_owned();
        if condition.is_empty() {
            self.breakpoint_conditions.remove(&(path.clone(), line));
        } else {
            self.breakpoint_conditions
                .insert((path.clone(), line), condition.clone());
        }
        if self
            .breakpoints
            .get(&path)
            .is_some_and(|lines| lines.contains(&line))
        {
            if let Some(class_name) = self.class_name_for_line(&path, line) {
                if let Some(debug) = self.debug.as_ref() {
                    if debug.status.is_active() {
                        debug.session.send(DebugCommand::ClearBreakpoint {
                            path: path.clone(),
                            line,
                        });
                        debug
                            .session
                            .send(DebugCommand::SetBreakpoint(BreakpointSpec {
                                path: path.clone(),
                                line,
                                class_name,
                                condition: (!condition.is_empty()).then_some(condition.clone()),
                            }));
                    }
                }
            }
        }
        self.log(
            LogLevel::Info,
            if condition.is_empty() {
                format!(
                    "Condição removida de {}:{}",
                    short_file(&path.to_string_lossy()),
                    line
                )
            } else {
                format!(
                    "Condição de {}:{}: {condition}",
                    short_file(&path.to_string_lossy()),
                    line
                )
            },
        );
    }

    fn show_breakpoint_condition_dialog(&mut self, ctx: &Context) {
        let Some(mut edit) = self.breakpoint_condition_edit.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut remove = false;
        let mut close = false;
        egui::Window::new(format!(
            "Condição — {}:{}",
            short_file(&edit.path.to_string_lossy()),
            edit.line
        ))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(egui::vec2(520.0, 140.0))
        .show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "Expressão booleana avaliada no frame selecionado. Ex.: i == 3, count > 0 && ready",
                )
                .small()
                .color(theme::muted()),
            );
            let response = ui.text_edit_singleline(&mut edit.text);
            if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                save = true;
            }
            ui.horizontal(|ui| {
                if ui.button("Salvar").clicked() {
                    save = true;
                }
                if ui.button("Remover").clicked() {
                    remove = true;
                }
                if ui.button("Fechar").clicked() {
                    close = true;
                }
            });
        });
        if close {
            open = false;
        }
        if save {
            self.update_breakpoint_condition(edit.path, edit.line, edit.text);
        } else if remove {
            self.update_breakpoint_condition(edit.path, edit.line, String::new());
        } else if open {
            self.breakpoint_condition_edit = Some(edit);
        }
    }

    fn debug_line_for_path(&self, path: &Path) -> Option<usize> {
        let debug = self.debug.as_ref()?;
        if debug.status != DebugUiStatus::Paused {
            return None;
        }
        let location = debug.location.as_ref()?;
        let mapped = path_for_location(self.index.as_ref(), &self.source_signatures, location)?;
        if mapped == path {
            Some(location.line.saturating_sub(1))
        } else {
            None
        }
    }

    fn poll_debug(&mut self) {
        let mut events = Vec::new();
        if let Some(debug) = self.debug.as_ref() {
            while let Some(event) = debug.session.poll() {
                events.push(event);
            }
        }
        if events.is_empty() {
            return;
        }
        let watch_expressions = self.watches.clone();
        let mut logs: Vec<(LogLevel, String)> = Vec::new();
        let mut open: Option<(PathBuf, usize)> = None;
        let mut clear_classes = false;
        let mut toast: Option<String> = None;
        let mut watch_results_update: Option<Vec<reclipse_debug::WatchValue>> = None;
        let mut clear_watches = false;
        {
            let signatures = &self.source_signatures;
            let index = self.index.as_ref();
            let Some(debug) = self.debug.as_mut() else {
                return;
            };
            for event in events {
                match event {
                    DebugEvent::Started { description } => {
                        debug.status = DebugUiStatus::Running;
                        debug.detail = description.clone();
                        logs.push((LogLevel::Success, format!("Debug: {description}")));
                    }
                    DebugEvent::Stopped(info) => {
                        debug.status = DebugUiStatus::Paused;
                        debug.threads = info.threads.clone();
                        debug.frames = info.frames.clone();
                        debug.variables = info.variables.clone();
                        debug.selected_frame = 0;
                        debug.location = info.location.clone();
                        debug.paused_thread = Some(info.thread.id);
                        let place = info
                            .location
                            .as_ref()
                            .map(|location| {
                                format!(
                                    "{}:{}",
                                    location
                                        .source_file
                                        .as_deref()
                                        .unwrap_or(&location.signature),
                                    location.line
                                )
                            })
                            .unwrap_or_else(|| "local desconhecido".to_owned());
                        debug.detail = format!("{} · {place}", info.reason.label());
                        if let Some(error) = &info.condition_error {
                            debug.detail = format!("{} · condição: {error}", debug.detail);
                            logs.push((
                                LogLevel::Warning,
                                format!("Debug: condição do breakpoint: {error}"),
                            ));
                        }
                        if !watch_expressions.is_empty() {
                            debug.session.send(DebugCommand::EvaluateWatches {
                                expressions: watch_expressions.clone(),
                            });
                        }
                        logs.push((
                            LogLevel::Warning,
                            format!(
                                "Debug: {} em {} ({})",
                                info.reason.label(),
                                place,
                                info.thread.name
                            ),
                        ));
                        if let Some(location) = &info.location {
                            if location.line > 0 {
                                if let Some(path) = path_for_location(index, signatures, location) {
                                    open = Some((path, location.line));
                                }
                            }
                        }
                    }
                    DebugEvent::Variables {
                        frame_index,
                        variables,
                    } => {
                        debug.selected_frame = frame_index;
                        debug.variables = variables;
                    }
                    DebugEvent::Resumed => {
                        debug.status = DebugUiStatus::Running;
                        debug.detail = "executando…".to_owned();
                        debug.frames.clear();
                        debug.variables.clear();
                        debug.location = None;
                        debug.paused_thread = None;
                        clear_watches = true;
                    }
                    DebugEvent::WatchValues(values) => {
                        watch_results_update = Some(values);
                    }
                    DebugEvent::Output(text) => {
                        let lower = text.to_lowercase();
                        let level = if lower.contains("exception") || lower.contains("error") {
                            LogLevel::Warning
                        } else {
                            LogLevel::Info
                        };
                        logs.push((level, text));
                    }
                    DebugEvent::BreakpointResolved { path, line } => {
                        logs.push((
                            LogLevel::Success,
                            format!(
                                "Debug: breakpoint ativo em {}:{}",
                                short_file(&path.to_string_lossy()),
                                line
                            ),
                        ));
                    }
                    DebugEvent::BreakpointError {
                        path,
                        line,
                        message,
                    } => {
                        logs.push((
                            LogLevel::Warning,
                            format!(
                                "Debug: breakpoint {}:{} não resolvido: {message}",
                                short_file(&path.to_string_lossy()),
                                line
                            ),
                        ));
                        debug.breakpoint_errors.push(format!(
                            "{}:{}: {message}",
                            path.display(),
                            line
                        ));
                        if debug.breakpoint_errors.len() > 20 {
                            debug.breakpoint_errors.remove(0);
                        }
                    }
                    DebugEvent::EvaluateResult {
                        expression,
                        value,
                        error,
                    } => {
                        debug.eval_result = Some(match (value, error) {
                            (Some(value), _) => format!("{expression} = {value}"),
                            (None, Some(error)) => format!("{expression}: {error}"),
                            (None, None) => format!("{expression}: sem valor"),
                        });
                    }
                    DebugEvent::Exited { message } => {
                        debug.status = DebugUiStatus::Exited;
                        debug.detail = message.clone();
                        debug.frames.clear();
                        debug.variables.clear();
                        debug.location = None;
                        clear_classes = true;
                        clear_watches = true;
                        logs.push((LogLevel::Info, format!("Debug: {message}")));
                        toast = Some(message);
                    }
                    DebugEvent::Error(error) => {
                        debug.status = DebugUiStatus::Failed;
                        debug.detail = error.clone();
                        clear_classes = true;
                        clear_watches = true;
                        logs.push((LogLevel::Error, format!("Debug: {error}")));
                        toast = Some(error);
                    }
                    DebugEvent::Status(text) => {
                        debug.detail = text.clone();
                        logs.push((LogLevel::Info, format!("Debug: {text}")));
                    }
                }
            }
        }
        for (level, text) in logs {
            self.log(level, text);
        }
        if clear_classes {
            self.debug_classes = None;
        }
        if let Some(values) = watch_results_update {
            self.watch_results = values;
        }
        if clear_watches {
            self.watch_results.clear();
        }
        if let Some((path, line)) = open {
            self.open_at(path, line, 1);
        }
        if let Some(toast) = toast {
            self.toast(toast);
        }
    }

    fn schedule_javac_diagnostics(&mut self) {
        if self.project.is_none() {
            return;
        }
        // A newer save cancels an in-flight background check.
        if let Some(cancel) = &self.javac_diag_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        self.javac_diag_rx = None;
        self.javac_diag_cancel = None;
        self.javac_diag_pending_at = Some(Instant::now() + Duration::from_millis(800));
    }

    fn poll_javac_diagnostics(&mut self) {
        if let Some(rx) = &self.javac_diag_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.javac_diag_rx = None;
                    self.javac_diag_cancel = None;
                    self.apply_background_diagnostics(&result);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.javac_diag_rx = None;
                    self.javac_diag_cancel = None;
                }
            }
            return;
        }
        let Some(at) = self.javac_diag_pending_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.javac_diag_pending_at = None;
        if self.build_running || self.project.is_none() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let env = self
            .classpath_index
            .as_ref()
            .map(BuildEnv::from_classpath)
            .unwrap_or_default();
        if !javac_available(&env) {
            if !self.javac_missing_warned {
                self.javac_missing_warned = true;
                self.log(
                    LogLevel::Warning,
                    "javac indisponível; diagnósticos limitados a imports não resolvidos",
                );
            }
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.javac_diag_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.javac_diag_rx = Some(receiver);
        thread::spawn(move || {
            match build::compile(&project, &env, &cancel) {
                Ok(result) => {
                    let _ = sender.send(result);
                }
                Err(_) => {
                    // Cancelled or JDK vanished mid-flight: stay silent, the
                    // next save re-schedules. Manual builds surface errors.
                }
            }
        });
    }

    /// Merge background `javac` diagnostics without touching manual-build
    /// status (`last_build`) or toasting on success.
    fn apply_background_diagnostics(&mut self, result: &build::CompileResult) {
        let errors = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error)
            .count();
        let warnings = result.diagnostics.len().saturating_sub(errors);
        let changed = self.javac_problems != result.diagnostics;
        self.javac_problems = result.diagnostics.clone();
        if !changed {
            return;
        }
        if errors > 0 {
            self.show_problems = true;
            self.log(
                LogLevel::Warning,
                format!("javac (fundo): {errors} erro(s), {warnings} warning(s)"),
            );
        } else if warnings > 0 {
            self.log(
                LogLevel::Info,
                format!("javac (fundo): ok, {warnings} warning(s)"),
            );
        } else {
            self.log(LogLevel::Success, "javac (fundo): sem erros".to_owned());
        }
    }

    fn test_project(&mut self) {
        self.test_project_filtered(None);
    }

    fn test_project_filtered(&mut self, filter: Option<build::TestFilter>) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de testar");
            return;
        };
        self.test_failures.clear();
        self.build_running = true;
        let label = filter
            .as_ref()
            .map(|filter| match &filter.method {
                Some(method) => format!("{}#{}", filter.class_name, method),
                None => filter.class_name.clone(),
            })
            .unwrap_or_else(|| "test".to_owned());
        self.build_label = Some(label.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.show_console = true;
        let env = std::env::vars().collect();
        self.log(LogLevel::Info, format!("Executando testes ({label})…"));
        thread::spawn(move || {
            let output_sender = sender.clone();
            let message = match build::run_tests_filtered(
                &project,
                &env,
                &cancel,
                filter.as_ref(),
                &mut |text| {
                    let _ = output_sender.send(BuildMessage::Output(text.to_owned()));
                },
            ) {
                Ok(result) => BuildMessage::TestsFinished(Box::new(result)),
                Err(error) => BuildMessage::Failed(error),
            };
            let _ = sender.send(message);
        });
    }

    fn run_test_class(&mut self, class_name: &str) {
        self.test_project_filtered(Some(build::TestFilter {
            class_name: class_name.to_owned(),
            method: None,
        }));
    }

    fn fetch_dependencies(&mut self) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de baixar dependências");
            return;
        };
        self.build_running = true;
        self.build_label = Some("deps".to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.show_console = true;
        self.log(
            LogLevel::Info,
            "Baixando dependências para o repositório local (mvn/gradle)…",
        );
        let env = std::env::vars().collect();
        thread::spawn(move || {
            let output_sender = sender.clone();
            let message = match build::resolve_dependencies(&project, &env, &cancel, &mut |text| {
                let _ = output_sender.send(BuildMessage::Output(text.to_owned()));
            }) {
                Ok(result) => BuildMessage::External {
                    label: result.tool,
                    success: result.success,
                    output: result.output,
                    elapsed_ms: result.elapsed_ms,
                    reindex_classpath: true,
                },
                Err(error) => BuildMessage::Failed(error),
            };
            let _ = sender.send(message);
        });
    }

    fn run_lifecycle_phase(&mut self, phase: build::LifecyclePhase) {
        if self.build_running {
            self.toast("Build já em andamento (cancele para recomeçar)");
            return;
        }
        let Some(project) = self.project.clone() else {
            self.toast("Abra um projeto antes de compilar");
            return;
        };
        self.build_running = true;
        self.build_label = Some(phase.label().to_owned());
        self.build_output_buffer.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        self.build_cancel = Some(cancel.clone());
        let (sender, receiver) = mpsc::channel();
        self.build_rx = Some(receiver);
        self.show_console = true;
        let env = std::env::vars().collect();
        self.log(
            LogLevel::Info,
            format!("Executando lifecycle: {}…", phase.label()),
        );
        thread::spawn(move || {
            let output_sender = sender.clone();
            let message = match build::run_lifecycle(&project, &env, &cancel, phase, &mut |text| {
                let _ = output_sender.send(BuildMessage::Output(text.to_owned()));
            }) {
                Ok(result) => BuildMessage::External {
                    label: result.tool,
                    success: result.success,
                    output: result.output,
                    elapsed_ms: result.elapsed_ms,
                    reindex_classpath: false,
                },
                Err(error) => BuildMessage::Failed(error),
            };
            let _ = sender.send(message);
        });
    }

    fn apply_external_result(
        &mut self,
        label: &str,
        success: bool,
        _output: &str,
        elapsed_ms: u128,
        reindex_classpath: bool,
    ) {
        if success {
            self.log(LogLevel::Success, format!("{label}: ok em {elapsed_ms} ms"));
        } else {
            self.log(
                LogLevel::Error,
                format!("{label}: falhou em {elapsed_ms} ms"),
            );
        }
        self.toast(if success {
            format!("{label}: concluído")
        } else {
            format!("{label}: falhou (ver console)")
        });
        if success && reindex_classpath {
            if let Some(project) = self.project.clone() {
                self.start_classpath(project);
            }
        }
    }

    fn poll_build(&mut self) {
        let Some(receiver) = &self.build_rx else {
            return;
        };
        let mut messages = Vec::new();
        let mut disconnected = false;
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
        if messages.is_empty() && !disconnected {
            return;
        }
        let had_messages = !messages.is_empty();
        self.build_rx = None;
        self.build_running = false;
        self.build_cancel = None;
        self.build_label = None;
        for message in messages {
            match message {
                BuildMessage::Output(text) => self.push_build_output(&text),
                BuildMessage::Compiled(compiled) => {
                    self.flush_build_output();
                    self.apply_compile_result(&compiled);
                }
                BuildMessage::DebugStarted {
                    session,
                    classes,
                    main,
                } => {
                    self.flush_build_output();
                    self.build_running = false;
                    self.build_cancel = None;
                    self.debug_classes = Some(*classes);
                    self.debug = Some(DebugUi {
                        session: *session,
                        status: DebugUiStatus::Starting,
                        detail: format!("iniciando {main}"),
                        threads: Vec::new(),
                        frames: Vec::new(),
                        variables: Vec::new(),
                        selected_frame: 0,
                        location: None,
                        paused_thread: None,
                        eval_input: String::new(),
                        eval_result: None,
                        exceptions: self.debug_pause_exceptions,
                        breakpoint_errors: Vec::new(),
                    });
                    self.show_console = true;
                    self.log(
                        LogLevel::Info,
                        format!("Debug {main}: aguardando eventos da JVM…"),
                    );
                }
                BuildMessage::RunFinished { compile, run, main } => {
                    self.flush_build_output();
                    self.apply_compile_result(&compile);
                    if run.output.contains("execução cancelada") {
                        self.log(LogLevel::Warning, format!("{main}: execução cancelada"));
                        self.toast(format!("{main}: execução cancelada"));
                        continue;
                    }
                    if run.success {
                        self.log(
                            LogLevel::Success,
                            format!("{main}: execução concluída em {} ms", run.elapsed_ms),
                        );
                    } else {
                        self.log(
                            LogLevel::Error,
                            format!("{main}: execução falhou em {} ms", run.elapsed_ms),
                        );
                    }
                    for line in run.output.lines().take(300) {
                        if !line.trim().is_empty() {
                            self.log(
                                if run.success {
                                    LogLevel::Info
                                } else {
                                    LogLevel::Error
                                },
                                line,
                            );
                        }
                    }
                    self.toast(if run.success {
                        format!("{main}: execução concluída")
                    } else {
                        format!("{main}: execução falhou")
                    });
                }
                BuildMessage::TestsFinished(result) => {
                    self.flush_build_output();
                    self.apply_test_result(&result);
                }
                BuildMessage::External {
                    label,
                    success,
                    output,
                    elapsed_ms,
                    reindex_classpath,
                } => {
                    self.flush_build_output();
                    self.apply_external_result(
                        &label,
                        success,
                        &output,
                        elapsed_ms,
                        reindex_classpath,
                    );
                }
                BuildMessage::Failed(error) => {
                    self.flush_build_output();
                    self.log(LogLevel::Error, format!("Build: {error}"));
                    self.toast(error);
                }
            }
        }
        if disconnected && !had_messages {
            // The worker always sends at least one message; a silent disconnect
            // is reported as a build failure so the UI never stays "running".
            self.toast("O processo de build terminou inesperadamente");
        }
    }

    fn push_build_output(&mut self, text: &str) {
        self.build_output_buffer.push_str(text);
        while let Some(position) = self.build_output_buffer.find('\n') {
            let line: String = self.build_output_buffer.drain(..=position).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if !line.trim().is_empty() {
                let lower = line.to_lowercase();
                let level = if lower.contains("error")
                    || lower.contains("failed")
                    || lower.contains("falhou")
                {
                    LogLevel::Error
                } else if lower.contains("warning") || lower.contains("warn") {
                    LogLevel::Warning
                } else {
                    LogLevel::Info
                };
                self.log(level, line);
            }
        }
    }

    fn flush_build_output(&mut self) {
        let line = self.build_output_buffer.trim().to_owned();
        self.build_output_buffer.clear();
        if !line.is_empty() {
            self.log(LogLevel::Info, line);
        }
    }

    fn apply_compile_result(&mut self, result: &build::CompileResult) {
        self.last_build = Some((result.success, result.elapsed_ms));
        self.javac_problems = result.diagnostics.clone();
        let errors = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error)
            .count();
        let warnings = result.diagnostics.len().saturating_sub(errors);
        for diagnostic in result.diagnostics.iter().take(24) {
            self.log(
                if diagnostic.is_error {
                    LogLevel::Error
                } else {
                    LogLevel::Warning
                },
                format!(
                    "javac: {}:{}: {}",
                    short_file(&diagnostic.file.to_string_lossy()),
                    diagnostic.line,
                    diagnostic.message
                ),
            );
        }
        if result.success {
            self.log(
                LogLevel::Success,
                format!("javac: compilação ok em {} ms", result.elapsed_ms),
            );
            self.toast(format!(
                "javac: compilação ok em {} ms ({warnings} warning(s))",
                result.elapsed_ms
            ));
        } else {
            self.show_problems = true;
            if result.diagnostics.is_empty() {
                for line in result.output.lines().take(24) {
                    if !line.trim().is_empty() {
                        self.log(LogLevel::Error, line);
                    }
                }
            }
            self.toast(format!("javac: {errors} erro(s), {warnings} warning(s)"));
        }
    }

    fn apply_test_result(&mut self, result: &build::TestResult) {
        self.last_build = Some((result.success, result.elapsed_ms));
        self.test_failures = result.summary.failures_detail.clone();
        self.show_console = true;
        self.log(
            if result.success {
                LogLevel::Success
            } else {
                LogLevel::Error
            },
            format!(
                "{}: {} teste(s), {} falha(s), {} erro(s), {} ignorado(s) em {} ms",
                result.tool,
                result.summary.total,
                result.summary.failures,
                result.summary.errors,
                result.summary.skipped,
                result.elapsed_ms
            ),
        );
        for failure in result.summary.failures_detail.iter().take(30) {
            let identity = failure
                .class_name
                .as_deref()
                .map(|class| match &failure.method {
                    Some(method) => format!("{class}#{method}"),
                    None => class.to_owned(),
                })
                .unwrap_or_else(|| "teste".to_owned());
            self.log(
                LogLevel::Error,
                if failure.file.as_os_str().is_empty() {
                    format!("teste: {identity}: {}", failure.message)
                } else {
                    format!(
                        "teste: {}:{}: {}",
                        short_file(&failure.file.to_string_lossy()),
                        failure.line,
                        failure.message
                    )
                },
            );
        }
        if !result.success {
            self.show_problems = true;
            if result.summary.failures_detail.is_empty() {
                for line in result.output.lines().take(24) {
                    if !line.trim().is_empty() {
                        self.log(LogLevel::Error, line);
                    }
                }
            }
        }
        self.toast(if result.success {
            format!("{}: testes ok", result.tool)
        } else {
            format!(
                "{}: {} falha(s)/{} erro(s)",
                result.tool, result.summary.failures, result.summary.errors
            )
        });
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
                    let buffers = self.open_java_buffers();
                    self.semantic_problems = compute_semantic_problems(
                        self.index.as_ref(),
                        self.project_types.as_ref(),
                        &index,
                        &buffers,
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
            self.schedule_javac_diagnostics();
        }
        let _ = crate::recovery::discard(&path);
        self.backup_state.remove(&path);
        self.tabs[index].external_change = None;
        self.watch_ignored
            .insert(path.clone(), Instant::now() + Duration::from_secs(2));
        if is_java {
            self.jdtls_notify_open(&path, text.clone());
        }
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
        match self.secondary_tab {
            Some(secondary) if secondary == index => self.secondary_tab = None,
            Some(secondary) if secondary > index => self.secondary_tab = Some(secondary - 1),
            _ => {}
        }
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

    fn organize_imports(&mut self) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let source = self.tabs[active].doc.text();
        let file = reclipse_java::parse_source(&path, &source);
        match organize_imports(&source, &file) {
            Ok(edits) => {
                let count = edits.len();
                for edit in edits {
                    let start = source[..edit.start_byte].chars().count();
                    let end = source[..edit.end_byte].chars().count();
                    self.tabs[active].doc.edit(start, end, &edit.replacement);
                }
                self.tabs[active].invalidate_highlight();
                self.tabs[active].focus();
                self.toast(format!("{count} import(s) organizado(s)"));
            }
            Err(error) => self.toast(error),
        }
    }

    fn format_file(&mut self) {
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let source = self.tabs[active].doc.text();
        match build::format_source(&source) {
            Ok(formatted) => {
                let len = source.chars().count();
                self.tabs[active].doc.edit(0, len, &formatted);
                self.tabs[active].invalidate_highlight();
                self.tabs[active].focus();
                self.toast("Arquivo formatado");
            }
            Err(error) => self.toast(error),
        }
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

        let cursor_byte = byte_index_of_char(&text, cursor);
        let in_package = file
            .package_range
            .is_some_and(|(start, end)| cursor_byte >= start && cursor_byte < end);
        if in_package {
            self.begin_package_rename();
            return;
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

    fn begin_package_rename(&mut self) {
        if self.rename.is_some() {
            return;
        }
        self.completion = None;
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        let path = self.tabs[active].doc.path.clone();
        let Some(package) = self
            .index
            .as_ref()
            .and_then(|index| index.symbols_for(&path))
            .and_then(|file| file.package.clone())
        else {
            self.toast("O arquivo ativo não declara um pacote");
            return;
        };
        let preview = match (
            self.project.as_ref(),
            self.index.as_ref(),
            self.classpath_index.as_ref(),
        ) {
            (Some(project), Some(index), Some(classpath)) => plan_package_rename(
                &package,
                &format!("{package}.renamed"),
                project,
                index,
                classpath,
            )
            .ok()
            .map(|plan| plan.total_edits()),
            _ => None,
        };
        self.rename = Some(RenameState {
            target: RenameTarget::Package {
                old: package.clone(),
            },
            old_name: package.clone(),
            new_name: package,
            kind: RenameKind::Package,
            preview,
            focus_pending: true,
            error: None,
        });
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
            RenameTarget::Package { old } => match self.project.as_ref() {
                Some(project) => plan_package_rename(old, &new_name, project, index, classpath),
                None => Err("nenhum projeto aberto".to_owned()),
            },
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
                let moved = plan.moves.len();
                let kind = plan.kind;
                self.rename = None;
                match (kind, moved) {
                    (RenameKind::Package, moved) => self.toast(format!(
                        "pacote {} renomeado para {new_name} ({count} edições; {moved} arquivo(s) movido(s))",
                        plan.old_name
                    )),
                    (_, 1) => self.toast(format!(
                        "{} renomeado para {new_name} ({count} edições; 1 arquivo movido)",
                        plan.old_name
                    )),
                    _ => self.toast(format!(
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
        for (from, to) in &plan.moves {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("falha ao criar {}: {error}", parent.display()))?;
            }
            fs::rename(from, to).map_err(|error| {
                format!(
                    "falha ao renomear {} para {}: {error}",
                    from.display(),
                    to.display()
                )
            })?;
        }
        let new_path_for = |path: &Path| {
            plan.moves
                .iter()
                .find(|(from, _)| from == path)
                .map(|(_, to)| to.clone())
                .unwrap_or_else(|| path.to_path_buf())
        };
        for (old_path, _) in &new_texts {
            let new_path = new_path_for(old_path);
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
                let new_path = new_path_for(old_path);
                if new_path != *old_path {
                    removed.push(old_path.clone());
                }
                updates.push((new_path, text.clone()));
            }
            index.update_files(&removed, updates);
            if let Some(project) = &mut self.project {
                replace_project_paths(project, &plan.moves);
            }
            replace_project_paths(&mut index.project, &plan.moves);
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
            console_history: self
                .console
                .iter()
                .rev()
                .take(600)
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            breakpoints: self
                .breakpoints
                .iter()
                .flat_map(|(path, lines)| {
                    lines.iter().map(|line| crate::session::BreakpointSession {
                        path: path.clone(),
                        line: *line,
                        condition: self
                            .breakpoint_conditions
                            .get(&(path.clone(), *line))
                            .cloned()
                            .unwrap_or_default(),
                    })
                })
                .collect(),
            watches: self.watches.clone(),
            font_size: self.font_size,
            theme: self.theme.config_name().to_owned(),
            launch: self.launch.clone(),
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

    fn toggle_split(&mut self) {
        if self.secondary_tab.is_some() {
            self.secondary_tab = None;
            self.toast("Editor dividido fechado");
            return;
        }
        let Some(active) = self.active_tab else {
            self.toast("Nenhum arquivo ativo");
            return;
        };
        if self.tabs.len() < 2 {
            self.toast("Abra outro arquivo para dividir o editor");
            return;
        }
        let secondary = (0..self.tabs.len())
            .find(|index| *index != active)
            .unwrap_or(active);
        self.secondary_tab = Some(secondary);
        self.toast("Editor dividido; arraste abas entre os painéis");
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
        let mut text = text.into();
        if text.chars().count() > CONSOLE_MAX_LINE_CHARS {
            text = text
                .chars()
                .take(CONSOLE_MAX_LINE_CHARS - 16)
                .collect::<String>()
                + "… [truncated]";
        }
        self.console.push(LogLine {
            at: self.app_started_at.elapsed(),
            level,
            text,
        });
        if self.console.len() > CONSOLE_MAX_LINES {
            let excess = self.console.len() - CONSOLE_MAX_LINES;
            self.console.drain(0..excess);
            self.console_dropped += excess;
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
                    PaletteEntry::new("Projeto: compilar com javac", "", Action::Compile),
                    PaletteEntry::new(
                        "Projeto: build compile (mvn/gradle)",
                        "",
                        Action::BuildCompile,
                    ),
                    PaletteEntry::new(
                        "Projeto: build package (mvn/gradle)",
                        "",
                        Action::BuildPackage,
                    ),
                    PaletteEntry::new("Projeto: build clean (mvn/gradle)", "", Action::BuildClean),
                    PaletteEntry::new("Projeto: executar", "Ctrl+F11", Action::Run),
                    PaletteEntry::new("Projeto: testar (mvn/gradle)", "", Action::Test),
                    PaletteEntry::new(
                        "Projeto: baixar dependências (mvn/gradle)",
                        "",
                        Action::FetchDeps,
                    ),
                    PaletteEntry::new("Projeto: cancelar build/execução", "", Action::CancelBuild),
                    PaletteEntry::new("Projeto: re-executar última main", "", Action::Rerun),
                    PaletteEntry::new(
                        "Projeto: configuração de execução…",
                        "",
                        Action::LaunchConfig,
                    ),
                    PaletteEntry::new("Projeto: depurar (JDWP)", "F11", Action::Debug),
                    PaletteEntry::new("Debug: continuar", "F8", Action::DebugContinue),
                    PaletteEntry::new("Debug: pausar", "F12", Action::DebugPause),
                    PaletteEntry::new("Debug: passo a passo", "F6", Action::DebugStepOver),
                    PaletteEntry::new("Debug: entrar", "F5", Action::DebugStepInto),
                    PaletteEntry::new("Debug: sair", "F7", Action::DebugStepOut),
                    PaletteEntry::new("Debug: encerrar", "Ctrl+F2", Action::DebugStop),
                    PaletteEntry::new("Debug: hot swap", "", Action::DebugHotSwap),
                    PaletteEntry::new(
                        "Ir para definição",
                        self.keymap.label(KeyAction::GoToDefinition),
                        Action::GoToDefinition,
                    ),
                    PaletteEntry::new("Editar: renomear", "Alt+Shift+R", Action::Rename),
                    PaletteEntry::new("Editar: renomear pacote", "", Action::RenamePackage),
                    PaletteEntry::new(
                        "Código: organizar imports",
                        "Ctrl+Shift+I",
                        Action::OrganizeImports,
                    ),
                    PaletteEntry::new("Código: formatar arquivo", "Ctrl+Alt+F", Action::FormatFile),
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
                    PaletteEntry::new("Exibir: editor dividido", "", Action::ToggleSplit),
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

        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::F11)) {
            self.run_project();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F11)) {
            self.start_debug();
            return;
        }
        if self
            .debug
            .as_ref()
            .is_some_and(|debug| debug.status == DebugUiStatus::Paused)
        {
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F8)) {
                self.debug_continue();
                return;
            }
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F6)) {
                self.debug_step(StepKind::Over);
                return;
            }
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F7)) {
                self.debug_step(StepKind::Out);
                return;
            }
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F5)) {
                self.debug_step(StepKind::Into);
                return;
            }
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F12)) {
            self.debug_pause();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::F2)) {
            self.stop_debug();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::I)) {
            self.organize_imports();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL | Modifiers::ALT, Key::F)) {
            self.format_file();
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
            let mut lines = symbol.detail.clone().into_iter().collect::<Vec<_>>();
            let source = self.tabs[active].doc.text();
            if let Some(doc) = symbol_javadoc(&source, symbol.name_start_byte) {
                lines.push(doc);
            }
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
            let disk = fs::read_to_string(&location.file).ok();
            if let Some(source) = disk.as_deref() {
                if let Some(doc) = symbol_javadoc(source, location.symbol.name_start_byte) {
                    lines.push(doc);
                }
            }
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

                    ui.menu_button("Código", |ui| {
                        if ui
                            .add(
                                egui::Button::new("Organizar imports")
                                    .shortcut_text("Ctrl+Shift+I"),
                            )
                            .clicked()
                        {
                            ui.close_menu();
                            self.organize_imports();
                        }
                        if ui
                            .add(egui::Button::new("Formatar arquivo").shortcut_text("Ctrl+Alt+F"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.format_file();
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
                        ui.separator();
                        if ui.button("Compilar com javac").clicked() {
                            ui.close_menu();
                            self.compile_project();
                        }
                        if ui.button("Build compile (mvn/gradle)").clicked() {
                            ui.close_menu();
                            self.run_lifecycle_phase(build::LifecyclePhase::Compile);
                        }
                        if ui.button("Package (mvn/gradle)").clicked() {
                            ui.close_menu();
                            self.run_lifecycle_phase(build::LifecyclePhase::Package);
                        }
                        if ui.button("Clean (mvn/gradle)").clicked() {
                            ui.close_menu();
                            self.run_lifecycle_phase(build::LifecyclePhase::Clean);
                        }
                        if ui
                            .add(egui::Button::new("Executar").shortcut_text("Ctrl+F11"))
                            .clicked()
                        {
                            ui.close_menu();
                            self.run_project();
                        }
                        if ui.button("Renomear pacote…").clicked() {
                            ui.close_menu();
                            self.begin_package_rename();
                        }
                        ui.add_enabled(false, egui::Button::new("Debug (M4)"))
                            .on_hover_text("Debugger JDWP/JDI planejado para o M4");
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
                        if ui
                            .selectable_label(self.secondary_tab.is_some(), "Editor dividido")
                            .clicked()
                        {
                            ui.close_menu();
                            self.toggle_split();
                        }
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
            .unwrap_or(0)
            + self
                .javac_problems
                .iter()
                .filter(|diagnostic| diagnostic.is_error)
                .count();
        let semantic_problems = self.semantic_problems.len()
            + self
                .javac_problems
                .iter()
                .filter(|diagnostic| !diagnostic.is_error)
                .count();
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
                    if self.build_running {
                        ui.spinner();
                        let label = self.build_label.as_deref().unwrap_or("build…");
                        ui.label(RichText::new(label).small().color(theme::yellow()));
                        if toolbar_button(ui, "Cancelar", "Cancelar build/execução/testes") {
                            actions.push(Action::CancelBuild);
                        }
                    } else {
                        let run = ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Executar").size(12.0).color(theme::bg()),
                                )
                                .fill(theme::accent())
                                .stroke(egui::Stroke::new(1.0, theme::accent())),
                            )
                            .on_hover_text("Compilar e executar a classe main (Ctrl+F11)");
                        if run.clicked() {
                            actions.push(Action::Run);
                        }
                        if toolbar_button(ui, "Compilar", "Compilar com javac") {
                            actions.push(Action::Compile);
                        }
                        if toolbar_button(
                            ui,
                            "Build",
                            "Compile do Maven/Gradle com output incremental",
                        ) {
                            actions.push(Action::BuildCompile);
                        }
                        if toolbar_button(ui, "Testar", "mvn/gradle test com parser") {
                            actions.push(Action::Test);
                        }
                        if toolbar_button(ui, "Launch…", "main, args, JVM, env") {
                            actions.push(Action::LaunchConfig);
                        }
                        if let Some(debug) = &self.debug {
                            let status = debug.status;
                            if status == DebugUiStatus::Paused
                                && toolbar_button(ui, "Continuar", "F8 · resume")
                            {
                                actions.push(Action::DebugContinue);
                            }
                            if status == DebugUiStatus::Paused
                                && toolbar_button(ui, "Passo", "F6 · step over")
                            {
                                actions.push(Action::DebugStepOver);
                            }
                            if status == DebugUiStatus::Paused
                                && toolbar_button(ui, "Entrar", "F5 · step into")
                            {
                                actions.push(Action::DebugStepInto);
                            }
                            if status == DebugUiStatus::Paused
                                && toolbar_button(ui, "Sair", "F7 · step out")
                            {
                                actions.push(Action::DebugStepOut);
                            }
                            if status == DebugUiStatus::Running
                                && toolbar_button(ui, "Pausar", "Suspender a JVM (F12)")
                            {
                                actions.push(Action::DebugPause);
                            }
                            if toolbar_button(ui, "Parar", "Encerrar a sessão de debug") {
                                actions.push(Action::DebugStop);
                            }
                            if debug.status.is_active()
                                && toolbar_button(
                                    ui,
                                    "Hot swap",
                                    "Recompilar e redefinir classes carregadas",
                                )
                            {
                                actions.push(Action::DebugHotSwap);
                            }
                        } else if toolbar_button(ui, "Debug", "Iniciar debugger JDWP (F11)") {
                            actions.push(Action::Debug);
                        }
                    }
                    ui.separator();
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
                        if self.build_running {
                            ui.label(
                                RichText::new(self.build_label.as_deref().unwrap_or("build…"))
                                    .small()
                                    .color(theme::yellow()),
                            );
                            ui.separator();
                        } else if let Some((success, elapsed)) = self.last_build {
                            ui.label(
                                RichText::new(format!("javac {} ms", elapsed))
                                    .small()
                                    .color(if success {
                                        theme::green()
                                    } else {
                                        theme::red()
                                    }),
                            )
                            .on_hover_text(if success {
                                "Última compilação concluída"
                            } else {
                                "Última compilação falhou"
                            });
                            ui.separator();
                        }
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
            + self.semantic_problems.len()
            + self.javac_problems.len();
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
        let mut cancel = false;
        let mut rerun = false;
        let mut open_launch = false;
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
                        if self.build_running {
                            if ui.small_button("Cancelar").clicked() {
                                cancel = true;
                            }
                        } else {
                            if ui.small_button("Re-executar").clicked() {
                                rerun = true;
                            }
                            if ui.small_button("Launch…").clicked() {
                                open_launch = true;
                            }
                        }
                        ui.label(
                            RichText::new(if self.build_running {
                                self.build_label.as_deref().unwrap_or("build…")
                            } else {
                                "Rust-native diagnostics"
                            })
                            .small()
                            .color(theme::muted()),
                        );
                        ui.label(
                            RichText::new(format!(
                                "{} linhas{}",
                                self.console.len(),
                                if self.console_dropped > 0 {
                                    format!(" (+{} antigas)", self.console_dropped)
                                } else {
                                    String::new()
                                }
                            ))
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
                                let root = self.root.clone();
                                let mut open_link: Option<(PathBuf, usize)> = None;
                                for line in &self.console {
                                    let color = match line.level {
                                        LogLevel::Info => theme::muted(),
                                        LogLevel::Success => theme::green(),
                                        LogLevel::Warning => theme::yellow(),
                                        LogLevel::Error => theme::red(),
                                    };
                                    let seconds = line.at.as_secs();
                                    let clean = strip_ansi(&line.text);
                                    let text = format!(
                                        "[{:02}:{:02}] {}",
                                        seconds / 60,
                                        seconds % 60,
                                        line.text
                                    );
                                    if let Some((file, lineno)) =
                                        build::parse_file_link(&clean, root.as_deref())
                                    {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "[{:02}:{:02}] ",
                                                    seconds / 60,
                                                    seconds % 60
                                                ))
                                                .monospace()
                                                .size(11.0)
                                                .color(color),
                                            );
                                            let label = format!(
                                                "{}:{}",
                                                short_file(&file.to_string_lossy()),
                                                lineno
                                            );
                                            if ui
                                                .link(
                                                    RichText::new(format!(
                                                        "{label} {}",
                                                        clean.chars().take(220).collect::<String>()
                                                    ))
                                                    .color(color),
                                                )
                                                .clicked()
                                            {
                                                open_link = Some((file, lineno));
                                            }
                                        });
                                    } else {
                                        ui.label(ansi_layout(
                                            &text,
                                            color,
                                            egui::FontId::monospace(11.0),
                                        ));
                                    }
                                }
                                if let Some((file, lineno)) = open_link {
                                    let file = if file.is_absolute() {
                                        file
                                    } else if let Some(root) = &self.root {
                                        root.join(file)
                                    } else {
                                        file
                                    };
                                    self.open_at(file, lineno, 1);
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
            self.console_dropped = 0;
        }
        if close {
            self.show_console = false;
        }
        if cancel {
            self.cancel_build();
        }
        if rerun {
            self.rerun_last();
        }
        if open_launch {
            self.launch_open = true;
        }
    }

    fn show_launch_dialog(&mut self, ctx: &Context) {
        if !self.launch_open {
            return;
        }
        let mut open = true;
        let mut apply_main: Option<String> = None;
        egui::Window::new("Configuração de execução")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(560.0, 380.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("main, argumentos, JVM e ambiente (persistidos na sessão).")
                        .small()
                        .color(theme::muted()),
                );
                ui.separator();
                ui.label("Classe main (vazio = auto-detectar):");
                ui.text_edit_singleline(&mut self.launch.main_class);
                if !self.main_classes.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Detectadas:").small().color(theme::muted()));
                        for main in self.main_classes.clone() {
                            if ui.small_button(&main).clicked() {
                                apply_main = Some(main);
                            }
                        }
                    });
                }
                ui.label("Argumentos do programa:");
                ui.text_edit_singleline(&mut self.launch.program_args);
                ui.label("Argumentos da JVM (ex: -Xmx1g -Dkey=val):");
                ui.text_edit_singleline(&mut self.launch.jvm_args);
                ui.label("Attach de debug (host:porta; vazio = lançar JVM):");
                ui.text_edit_singleline(&mut self.launch.debug_attach);
                ui.label("Ambiente (KEY=VAL por linha):");
                ui.text_edit_multiline(&mut self.launch.env_vars);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Executar").clicked() {
                        self.launch_open = false;
                        self.run_project();
                    }
                    if ui.button("Debug").clicked() {
                        self.launch_open = false;
                        self.start_debug();
                    }
                    if ui.button("Fechar").clicked() {
                        self.launch_open = false;
                    }
                });
            });
        if let Some(main) = apply_main {
            self.launch.main_class = main;
        }
        if !open {
            self.launch_open = false;
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
                        if self.build_running {
                            ui.spinner();
                            ui.label(
                                RichText::new(self.build_label.as_deref().unwrap_or("build…"))
                                    .small()
                                    .color(theme::yellow()),
                            );
                        } else if let Some((success, elapsed)) = self.last_build {
                            ui.label(
                                RichText::new(format!(
                                    "javac {} · {} ms",
                                    if success { "ok" } else { "falhou" },
                                    elapsed
                                ))
                                .small()
                                .color(if success {
                                    theme::green()
                                } else {
                                    theme::red()
                                }),
                            );
                        }
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
                        self.tests_section(ui);
                        ui.add_space(10.0);
                        self.debug_section(ui);
                        ui.add_space(10.0);
                        self.rust_telemetry_card(ui);
                        self.classpath_warnings_card(ui);
                    });
            });
        for action in actions {
            self.execute_action(action);
        }
    }

    fn tests_section(&mut self, ui: &mut Ui) {
        section_header(ui, "TESTES");
        let tests = self.test_targets.clone();
        if tests.is_empty() {
            ui.label(
                RichText::new("Nenhum teste JUnit descoberto")
                    .small()
                    .color(theme::muted()),
            );
            return;
        }
        let failures = self.test_failures.clone();
        let mut run: Option<build::TestFilter> = None;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} método(s)", tests.len()))
                    .small()
                    .color(theme::muted()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("Todos")
                    .on_hover_text("Executar todos")
                    .clicked()
                {
                    run = Some(build::TestFilter {
                        class_name: String::new(),
                        method: None,
                    });
                }
            });
        });
        let mut current_class = String::new();
        for test in &tests {
            if test.class_name != current_class {
                current_class = test.class_name.clone();
                let class_failed = failures
                    .iter()
                    .any(|failure| failure.class_name.as_deref() == Some(&current_class));
                ui.horizontal(|ui| {
                    if ui
                        .small_button("▶")
                        .on_hover_text("Executar a classe")
                        .clicked()
                    {
                        run = Some(build::TestFilter {
                            class_name: current_class.clone(),
                            method: None,
                        });
                    }
                    ui.label(RichText::new(&current_class).small().strong().color(
                        if class_failed {
                            theme::red()
                        } else {
                            theme::accent()
                        },
                    ))
                    .on_hover_text(format!(
                        "{} · {}",
                        current_class,
                        match test.framework {
                            reclipse_java::TestFramework::JUnit4 => "JUnit 4",
                            reclipse_java::TestFramework::JUnit5 => "JUnit 5",
                        }
                    ));
                });
            }
            let method_failed = failures.iter().any(|failure| {
                failure.class_name.as_deref() == Some(&current_class)
                    && failure.method.as_deref() == Some(&test.method)
            });
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if ui
                    .small_button("▶")
                    .on_hover_text("Executar este teste")
                    .clicked()
                {
                    run = Some(build::TestFilter {
                        class_name: test.class_name.clone(),
                        method: Some(test.method.clone()),
                    });
                }
                let label = ui.selectable_label(
                    false,
                    RichText::new(&test.method)
                        .size(12.0)
                        .color(if method_failed {
                            theme::red()
                        } else {
                            theme::text()
                        }),
                );
                if label.clicked() {
                    self.open_at(test.file.clone(), test.line, 1);
                }
            });
        }
        if let Some(filter) = run {
            if filter.class_name.is_empty() {
                self.test_project();
            } else if filter.method.is_some() {
                self.test_project_filtered(Some(filter));
            } else {
                self.run_test_class(&filter.class_name);
            }
        }
    }

    fn debug_section(&mut self, ui: &mut Ui) {
        section_header(ui, "DEBUG");
        let mut debug = self.debug.take();
        let mut start = false;
        let mut close = false;
        let mut command: Option<DebugCommand> = None;
        let mut select_frame: Option<(u64, usize)> = None;
        let mut open: Option<(PathBuf, usize)> = None;
        let mut remove_breakpoint: Option<(PathBuf, usize)> = None;
        let mut edit_condition: Option<(PathBuf, usize, String)> = None;
        let mut hot_swap = false;
        let mut pause_exceptions = debug
            .as_ref()
            .map_or(self.debug_pause_exceptions, |debug| debug.exceptions);

        if let Some(debug) = &mut debug {
            let color = match debug.status {
                DebugUiStatus::Starting | DebugUiStatus::Running => theme::yellow(),
                DebugUiStatus::Paused => theme::accent(),
                DebugUiStatus::Exited => theme::muted(),
                DebugUiStatus::Failed => theme::red(),
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("debug · {}", debug.status.label()))
                        .small()
                        .strong()
                        .color(color),
                );
                if debug.status.is_active() {
                    if ui.small_button("Parar").clicked() {
                        close = true;
                    }
                } else if ui.small_button("Limpar").clicked() {
                    close = true;
                }
            });
            ui.label(RichText::new(&debug.detail).small().color(theme::muted()));
            if debug.status == DebugUiStatus::Paused {
                let paused = debug
                    .paused_thread
                    .and_then(|id| {
                        debug
                            .threads
                            .iter()
                            .find(|thread| thread.id == id)
                            .map(|thread| thread.name.as_str())
                    })
                    .unwrap_or("—");
                let names = debug.threads.len();
                ui.label(
                    RichText::new(format!("thread: {paused} · {names} thread(s)"))
                        .size(10.5)
                        .color(theme::muted()),
                );
                ui.horizontal(|ui| {
                    if ui.small_button("Continuar").on_hover_text("F8").clicked() {
                        command = Some(DebugCommand::Resume);
                    }
                    if ui
                        .small_button("Passo")
                        .on_hover_text("F6 · step over")
                        .clicked()
                    {
                        command = Some(DebugCommand::Step(StepKind::Over));
                    }
                    if ui
                        .small_button("Entrar")
                        .on_hover_text("F5 · step into")
                        .clicked()
                    {
                        command = Some(DebugCommand::Step(StepKind::Into));
                    }
                    if ui
                        .small_button("Sair")
                        .on_hover_text("F7 · step out")
                        .clicked()
                    {
                        command = Some(DebugCommand::Step(StepKind::Out));
                    }
                });
            } else if debug.status == DebugUiStatus::Running && ui.small_button("Pausar").clicked()
            {
                command = Some(DebugCommand::Pause);
            }

            if !debug.frames.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Call stack").small().color(theme::muted()));
                let frames: Vec<FrameInfo> = debug.frames.iter().take(20).cloned().collect();
                for frame in frames {
                    let source = frame
                        .location
                        .source_file
                        .as_deref()
                        .map(short_file)
                        .map(str::to_owned)
                        .unwrap_or_else(|| simple_signature(&frame.location.signature));
                    let label = format!(
                        "#{} {}  {}:{}",
                        frame.index, frame.location.method, source, frame.location.line
                    );
                    if ui
                        .selectable_label(
                            debug.selected_frame == frame.index,
                            RichText::new(label).size(11.5).monospace(),
                        )
                        .clicked()
                    {
                        select_frame = Some((frame.thread_id, frame.index));
                        if frame.location.line > 0 {
                            if let Some(path) = path_for_location(
                                self.index.as_ref(),
                                &self.source_signatures,
                                &frame.location,
                            ) {
                                open = Some((path, frame.location.line));
                            }
                        }
                    }
                }
            }

            if !debug.variables.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Variáveis").small().color(theme::muted()));
                for variable in debug.variables.iter().take(80) {
                    ui.label(
                        RichText::new(format!(
                            "{} = {}  ({})",
                            variable.name, variable.value, variable.type_name
                        ))
                        .size(11.5)
                        .monospace()
                        .color(theme::text()),
                    );
                }
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut debug.eval_input)
                        .hint_text("variável ou literal")
                        .desired_width(120.0),
                );
                if ui.small_button("Avaliar").clicked() && !debug.eval_input.trim().is_empty() {
                    command = Some(DebugCommand::Evaluate {
                        expression: debug.eval_input.clone(),
                    });
                }
            });
            if let Some(result) = &debug.eval_result {
                ui.label(
                    RichText::new(result)
                        .size(11.0)
                        .monospace()
                        .color(theme::accent()),
                );
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Watches").small().color(theme::muted()));
            let mut remove_watch = None;
            for (index, watch) in self.watches.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .small_button("×")
                        .on_hover_text("Remover watch")
                        .clicked()
                    {
                        remove_watch = Some(index);
                    }
                    let result = self
                        .watch_results
                        .iter()
                        .find(|result| result.expression == *watch);
                    let text = match result {
                        Some(result) if result.error.is_some() => {
                            format!("{}: {}", watch, result.error.as_deref().unwrap_or("erro"))
                        }
                        Some(result) if result.value.is_some() => {
                            format!("{} = {}", watch, result.value.as_deref().unwrap_or("?"))
                        }
                        _ => format!("{watch} = …"),
                    };
                    ui.label(RichText::new(text).size(11.0).monospace().color(
                        if result.is_some_and(|result| result.error.is_some()) {
                            theme::red()
                        } else {
                            theme::text()
                        },
                    ));
                });
            }
            if let Some(index) = remove_watch {
                self.watches.remove(index);
                if debug.status == DebugUiStatus::Paused {
                    debug.session.send(DebugCommand::EvaluateWatches {
                        expressions: self.watches.clone(),
                    });
                }
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.watch_input)
                        .hint_text("watch (ex.: i, total + 1)")
                        .desired_width(150.0),
                );
                if ui.small_button("Adicionar").clicked() && !self.watch_input.trim().is_empty() {
                    let expression = self.watch_input.trim().to_owned();
                    if !self.watches.contains(&expression) {
                        self.watches.push(expression);
                    }
                    self.watch_input.clear();
                    if debug.status == DebugUiStatus::Paused {
                        debug.session.send(DebugCommand::EvaluateWatches {
                            expressions: self.watches.clone(),
                        });
                    }
                }
            });
            if self.hot_swap_running {
                ui.label(
                    RichText::new("hot swap: compilando…")
                        .size(10.5)
                        .color(theme::yellow()),
                );
            } else if debug.status.is_active() && ui.small_button("Hot swap").clicked() {
                hot_swap = true;
            }
            if !debug.breakpoint_errors.is_empty() {
                for error in debug.breakpoint_errors.iter().rev().take(3) {
                    ui.label(RichText::new(error).size(10.5).color(theme::red()));
                }
            }
            if ui
                .checkbox(&mut pause_exceptions, "Pausar em exceções")
                .changed()
            {
                command = Some(DebugCommand::SetPauseOnExceptions(pause_exceptions));
            }
        } else {
            let count: usize = self.breakpoints.values().map(BTreeSet::len).sum();
            ui.label(
                RichText::new(if count == 0 {
                    "Clique na gutter para criar breakpoints. F11 inicia o debug.".to_owned()
                } else {
                    format!("{count} breakpoint(s) configurado(s).")
                })
                .small()
                .color(theme::muted()),
            );
            ui.horizontal(|ui| {
                if ui.small_button("Debug (F11)").clicked() {
                    start = true;
                }
                if !self.launch.debug_attach.trim().is_empty() {
                    ui.label(
                        RichText::new(format!("attach {}", self.launch.debug_attach.trim()))
                            .small()
                            .color(theme::muted()),
                    );
                }
            });
            if ui
                .checkbox(&mut pause_exceptions, "Pausar em exceções")
                .changed()
            {
                self.debug_pause_exceptions = pause_exceptions;
            }
        }

        if !self.breakpoints.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new("Breakpoints").small().color(theme::muted()));
            let entries: Vec<(PathBuf, Vec<usize>)> = self
                .breakpoints
                .iter()
                .take(12)
                .map(|(path, lines)| (path.clone(), lines.iter().copied().collect()))
                .collect();
            for (path, lines) in entries {
                for line in lines {
                    ui.horizontal(|ui| {
                        if ui.small_button("×").on_hover_text("Remover").clicked() {
                            remove_breakpoint = Some((path.clone(), line));
                        }
                        let condition = self
                            .breakpoint_conditions
                            .get(&(path.clone(), line))
                            .cloned()
                            .unwrap_or_default();
                        let label = if condition.is_empty() {
                            format!("{}:{}", short_file(&path.to_string_lossy()), line)
                        } else {
                            format!(
                                "{}:{}  [{}]",
                                short_file(&path.to_string_lossy()),
                                line,
                                truncate_text(&condition, 40)
                            )
                        };
                        ui.label(
                            RichText::new(label)
                                .size(11.0)
                                .monospace()
                                .color(theme::muted()),
                        );
                        if ui
                            .small_button("cond")
                            .on_hover_text("Editar condição do breakpoint")
                            .clicked()
                        {
                            edit_condition = Some((path.clone(), line, condition));
                        }
                    });
                }
            }
        }

        if let Some(debug) = &mut debug {
            debug.exceptions = pause_exceptions;
            if let Some((_, frame_index)) = select_frame {
                debug.selected_frame = frame_index;
            }
            match &command {
                Some(DebugCommand::Resume) | Some(DebugCommand::Step(_)) => {
                    debug.status = DebugUiStatus::Running;
                    debug.detail = "executando…".to_owned();
                }
                Some(DebugCommand::Pause) => {
                    debug.detail = "pausando…".to_owned();
                }
                _ => {}
            }
        }
        if let Some(debug) = &debug {
            if let Some(command) = command {
                debug.session.send(command);
            }
            if close {
                debug.session.send(DebugCommand::Disconnect);
            }
            if let Some((thread_id, frame_index)) = select_frame {
                debug.session.send(DebugCommand::SelectFrame {
                    thread_id,
                    frame_index,
                });
            }
        }
        if close {
            debug = None;
        }
        self.debug_pause_exceptions = pause_exceptions;
        self.debug = debug;
        if let Some((path, line)) = open {
            self.open_at(path, line, 1);
        }
        if let Some((path, line)) = remove_breakpoint {
            self.toggle_breakpoint(path, line);
        }
        if let Some((path, line, text)) = edit_condition {
            self.breakpoint_condition_edit = Some(BreakpointConditionEdit { path, line, text });
        }
        if hot_swap {
            self.debug_hot_swap();
        }
        if start {
            self.start_debug();
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
                telemetry_row(
                    ui,
                    "javac",
                    &format!(
                        "{} erro(s)",
                        self.javac_problems
                            .iter()
                            .filter(|diagnostic| diagnostic.is_error)
                            .count()
                    ),
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
        let javac_total = self.javac_problems.len();
        let javac: Vec<build::JavacDiagnostic> =
            self.javac_problems.iter().take(300).cloned().collect();
        let test_total = self.test_failures.len();
        let tests: Vec<build::TestFailure> = self.test_failures.iter().take(100).cloned().collect();
        let jdt_total = self.jdtls_problems.len();
        let jdt: Vec<JdtProblem> = self.jdtls_problems.iter().take(300).cloned().collect();
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
        if total == 0
            && semantic_total == 0
            && javac_total == 0
            && test_total == 0
            && jdt_total == 0
        {
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

        if javac_total > 0 {
            ui.add_space(4.0);
            let errors = javac
                .iter()
                .filter(|diagnostic| diagnostic.is_error)
                .count();
            ui.label(
                RichText::new(format!(
                    "javac: {errors} erro(s), {} warning(s)",
                    javac_total.saturating_sub(errors)
                ))
                .small()
                .color(if errors > 0 {
                    theme::red()
                } else {
                    theme::yellow()
                }),
            );
            for diagnostic in javac {
                let relative = root
                    .as_ref()
                    .map(|root| {
                        diagnostic
                            .file
                            .strip_prefix(root)
                            .unwrap_or(&diagnostic.file)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|| diagnostic.file.display().to_string());
                let label = format!(
                    "{}:{}  {}",
                    short_file(&relative),
                    diagnostic.line,
                    diagnostic.message
                );
                let color = if diagnostic.is_error {
                    theme::red()
                } else {
                    theme::yellow()
                };
                if ui
                    .selectable_label(false, RichText::new(label).size(12.0).color(color))
                    .clicked()
                {
                    actions.push(Action::OpenAt(diagnostic.file, diagnostic.line, 1));
                }
            }
        }

        if test_total > 0 {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("testes: {test_total} falha(s)"))
                    .small()
                    .color(theme::red()),
            );
            for failure in tests {
                let relative = root
                    .as_ref()
                    .map(|root| {
                        failure
                            .file
                            .strip_prefix(root)
                            .unwrap_or(&failure.file)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|| failure.file.display().to_string());
                let label = if failure.file.as_os_str().is_empty() {
                    format!(
                        "{}  {}",
                        failure
                            .class_name
                            .as_deref()
                            .map(|class| match &failure.method {
                                Some(method) => format!("{class}#{method}"),
                                None => class.to_owned(),
                            })
                            .unwrap_or_else(|| "teste".to_owned()),
                        failure.message
                    )
                } else {
                    format!(
                        "{}:{}  {}",
                        short_file(&relative),
                        failure.line,
                        failure.message
                    )
                };
                if ui
                    .selectable_label(false, RichText::new(label).size(12.0).color(theme::red()))
                    .clicked()
                    && !failure.file.as_os_str().is_empty()
                {
                    actions.push(Action::OpenAt(failure.file, failure.line, 1));
                }
            }
        }

        if jdt_total > 0 {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("JDT LS: {jdt_total} diagnóstico(s)"))
                    .small()
                    .color(theme::yellow()),
            );
            for problem in jdt {
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
                    .selectable_label(
                        false,
                        RichText::new(label).size(12.0).color(theme::yellow()),
                    )
                    .clicked()
                {
                    actions.push(Action::OpenAt(problem.file, problem.line, problem.column));
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
                if let Some(secondary) = self.secondary_tab.filter(|index| *index < self.tabs.len())
                {
                    let left = self
                        .active_tab
                        .filter(|index| *index != secondary)
                        .or_else(|| (0..self.tabs.len()).find(|index| *index != secondary));
                    let font_size = self.font_size;
                    let mut drop_left = None;
                    let mut drop_right = None;
                    ui.columns(2, |columns| {
                        let (_, payload) =
                            columns[0].dnd_drop_zone::<TabDrag, _>(egui::Frame::NONE, |ui| {
                                if let Some(index) = left {
                                    self.editor_pane(ui, index, font_size);
                                } else {
                                    self.welcome(ui);
                                }
                            });
                        if let Some(payload) = payload {
                            drop_left = Some(payload.index);
                        }
                        let (_, payload) =
                            columns[1].dnd_drop_zone::<TabDrag, _>(egui::Frame::NONE, |ui| {
                                self.editor_pane(ui, secondary, font_size);
                            });
                        if let Some(payload) = payload {
                            drop_right = Some(payload.index);
                        }
                    });
                    if let Some(index) = drop_left {
                        if index < self.tabs.len() {
                            let old_active = self.active_tab;
                            self.active_tab = Some(index);
                            if self.secondary_tab == Some(index) {
                                self.secondary_tab = old_active.filter(|active| *active != index);
                            }
                        }
                    }
                    if let Some(index) = drop_right {
                        if index < self.tabs.len() {
                            let old_secondary = self.secondary_tab;
                            self.secondary_tab = Some(index);
                            if self.active_tab == Some(index) {
                                self.active_tab = old_secondary.filter(|active| *active != index);
                            }
                        }
                    }
                } else {
                    self.secondary_tab = None;
                    let mut dropped = None;
                    let font_size = self.font_size;
                    let (_, payload) = ui.dnd_drop_zone::<TabDrag, _>(egui::Frame::NONE, |ui| {
                        if let Some(active) = self.active_tab {
                            self.editor_pane(ui, active, font_size);
                        } else {
                            self.welcome(ui);
                        }
                    });
                    if let Some(payload) = payload {
                        dropped = Some(payload.index);
                    }
                    if let Some(index) = dropped {
                        if index < self.tabs.len() {
                            self.active_tab = Some(index);
                        }
                    }
                }
            });
    }

    fn editor_pane(&mut self, ui: &mut Ui, index: usize, font_size: f32) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let title = tab.title();
        let path = tab.doc.path.display().to_string();
        let doc_path = tab.doc.path.clone();
        egui::Frame::NONE
            .fill(theme::panel())
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("J")
                            .monospace()
                            .size(9.0)
                            .color(theme::clr_type()),
                    );
                    ui.label(RichText::new(title).small().color(
                        if self.active_tab == Some(index) {
                            theme::text()
                        } else {
                            theme::muted()
                        },
                    ));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("arraste abas").small().color(theme::muted()));
                    });
                });
            })
            .response
            .on_hover_text(path);
        let breakpoint_lines: Vec<usize> = self
            .breakpoints
            .get(&doc_path)
            .map(|lines| lines.iter().map(|line| line.saturating_sub(1)).collect())
            .unwrap_or_default();
        let debug_line = self.debug_line_for_path(&doc_path);
        let mut toggled = None;
        if let Some(tab) = self.tabs.get_mut(index) {
            toggled = tab.show(ui, font_size, &breakpoint_lines, debug_line);
        }
        if let Some(line) = toggled {
            self.toggle_breakpoint(doc_path, line + 1);
        }
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
        let mut split = false;

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
                                let drag = ui.dnd_drag_source(
                                    egui::Id::new(("reclipse_tab_drag", index)),
                                    TabDrag { index },
                                    |ui| ui.selectable_label(is_active, text),
                                );
                                let response = drag.inner;
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
                            ui.separator();
                            if ui
                                .small_button(if self.secondary_tab.is_some() {
                                    "Split on"
                                } else {
                                    "Split"
                                })
                                .on_hover_text("Dividir editor / fechar split")
                                .clicked()
                            {
                                split = true;
                            }
                        });
                    });
            });

        if open_folder {
            self.open_folder_dialog();
        }
        if split {
            self.toggle_split();
        }
        if let Some(index) = activate {
            if self.secondary_tab == Some(index) {
                self.secondary_tab = self.active_tab.filter(|active| *active != index);
            }
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
                        "Debug: F11 iniciar · F8 continuar · F6 passo · F5 entrar · F7 sair · \
                         F12 pausar · Ctrl+F2 encerrar",
                    )
                    .small()
                    .color(theme::muted()),
                );
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
            Action::Compile => self.compile_project(),
            Action::BuildCompile => self.run_lifecycle_phase(build::LifecyclePhase::Compile),
            Action::BuildPackage => self.run_lifecycle_phase(build::LifecyclePhase::Package),
            Action::BuildClean => self.run_lifecycle_phase(build::LifecyclePhase::Clean),
            Action::Run => self.run_project(),
            Action::Test => self.test_project(),
            Action::FetchDeps => self.fetch_dependencies(),
            Action::CancelBuild => self.cancel_build(),
            Action::Rerun => self.rerun_last(),
            Action::LaunchConfig => {
                self.launch_open = true;
            }
            Action::Debug => self.start_debug(),
            Action::DebugContinue => self.debug_continue(),
            Action::DebugPause => self.debug_pause(),
            Action::DebugStepOver => self.debug_step(StepKind::Over),
            Action::DebugStepInto => self.debug_step(StepKind::Into),
            Action::DebugStepOut => self.debug_step(StepKind::Out),
            Action::DebugStop => self.stop_debug(),
            Action::DebugHotSwap => self.debug_hot_swap(),
            Action::ToggleSplit => self.toggle_split(),
            Action::ToggleProject => self.show_project = !self.show_project,
            Action::ToggleOutline => self.show_outline = !self.show_outline,
            Action::ToggleProblems => self.show_problems = !self.show_problems,
            Action::GoToDefinition => self.goto_definition(),
            Action::Rename => self.begin_rename(),
            Action::RenamePackage => self.begin_package_rename(),
            Action::OrganizeImports => self.organize_imports(),
            Action::FormatFile => self.format_file(),
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

    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    fn setup_screenshot_debug(&mut self) {
        if self.screenshot.debug_done {
            return;
        }
        self.screenshot.debug_done = true;
        let Some(project) = self.project.clone() else {
            return;
        };
        let path = self
            .index
            .as_ref()
            .and_then(|index| {
                index
                    .files
                    .iter()
                    .find(|(path, file)| {
                        is_java_path(path)
                            && file.symbols.iter().any(|symbol| {
                                symbol.name == "main"
                                    && symbol
                                        .detail
                                        .as_deref()
                                        .is_some_and(|detail| detail.contains("static"))
                            })
                    })
                    .map(|(path, _)| path.clone())
            })
            .or_else(|| project.java_files.first().cloned());
        let Some(path) = path else {
            return;
        };
        let Ok(source) = fs::read_to_string(&path) else {
            return;
        };
        let line = source
            .lines()
            .position(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("//")
                    && !trimmed.starts_with('*')
                    && (trimmed.contains("System.out.println")
                        || trimmed.contains("printModes")
                        || trimmed.contains("total +="))
            })
            .map(|line| line + 1);
        if let Some(line) = line {
            self.open_at(path.clone(), line, 1);
            let already = self
                .breakpoints
                .get(&path)
                .is_some_and(|lines| lines.contains(&line));
            if !already {
                self.toggle_breakpoint(path.clone(), line);
            }
            self.breakpoint_conditions
                .insert((path, line), "user != null".to_owned());
            if !self.watches.iter().any(|watch| watch == "user.name") {
                self.watches.push("user.name".to_owned());
            }
            self.start_debug();
        }
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
                    if self.screenshot.run_project && !self.screenshot.run_done {
                        if self.classpath_index.is_some() {
                            self.run_project();
                            self.screenshot.run_done = true;
                        }
                    }
                    if self.screenshot.debug
                        && !self.screenshot.debug_done
                        && self.classpath_index.is_some()
                    {
                        self.setup_screenshot_debug();
                    }
                    if self.screenshot.debug {
                        let ready = self.debug.as_ref().is_some_and(|debug| {
                            matches!(
                                debug.status,
                                DebugUiStatus::Paused
                                    | DebugUiStatus::Failed
                                    | DebugUiStatus::Exited
                            )
                        });
                        if !ready {
                            self.screenshot.ready_frames = 0;
                            ctx.request_repaint_after(Duration::from_millis(60));
                            return;
                        }
                    }
                    if self.screenshot.split && !self.screenshot.split_done {
                        if self.tabs.len() < 2 {
                            if let Some(path) = self.project.as_ref().and_then(|project| {
                                project
                                    .java_files
                                    .iter()
                                    .find(|path| {
                                        !self.tabs.iter().any(|tab| &tab.doc.path == *path)
                                    })
                                    .cloned()
                            }) {
                                self.open_file(path);
                            }
                        }
                        self.toggle_split();
                        self.screenshot.split_done = true;
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
        self.poll_build();
        self.poll_javac_diagnostics();
        self.poll_debug();
        self.poll_hot_swap();
        if self.debug.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.poll_jdtls();
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
        self.show_launch_dialog(ctx);
        self.show_breakpoint_condition_dialog(ctx);
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
            || self.build_running
            || self.javac_diag_pending_at.is_some()
            || self.javac_diag_rx.is_some()
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

fn replace_project_paths(project: &mut Project, moves: &[(PathBuf, PathBuf)]) {
    for (from, to) in moves {
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

fn strip_ansi(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\u{1b}' && characters.get(index + 1) == Some(&'[') {
            index += 2;
            while index < characters.len() && characters[index] != 'm' {
                index += 1;
            }
            if index < characters.len() {
                index += 1;
            }
        } else {
            output.push(characters[index]);
            index += 1;
        }
    }
    output
}

fn ansi_layout(text: &str, base: Color32, font: egui::FontId) -> egui::text::LayoutJob {
    let characters: Vec<char> = text.chars().collect();
    let mut job = egui::text::LayoutJob::default();
    let mut color = base;
    let mut buffer = String::new();
    let mut index = 0;
    let flush = |job: &mut egui::text::LayoutJob, buffer: &mut String, color: Color32| {
        if buffer.is_empty() {
            return;
        }
        let format = egui::TextFormat {
            font_id: font.clone(),
            color,
            ..Default::default()
        };
        job.append(buffer, 0.0, format);
        buffer.clear();
    };
    while index < characters.len() {
        if characters[index] == '\u{1b}' && characters.get(index + 1) == Some(&'[') {
            flush(&mut job, &mut buffer, color);
            index += 2;
            let mut codes = String::new();
            while index < characters.len() && characters[index] != 'm' {
                codes.push(characters[index]);
                index += 1;
            }
            if index < characters.len() {
                index += 1;
            }
            apply_sgr(&codes, &mut color, base);
        } else {
            buffer.push(characters[index]);
            index += 1;
        }
    }
    flush(&mut job, &mut buffer, color);
    job
}

fn apply_sgr(codes: &str, color: &mut Color32, base: Color32) {
    let parts: Vec<u16> = codes
        .split(';')
        .filter_map(|part| part.parse().ok())
        .collect();
    let mut index = 0;
    while index < parts.len() {
        match parts[index] {
            0 => *color = base,
            30 => *color = Color32::from_rgb(0x2a, 0x31, 0x3d),
            31 => *color = theme::red(),
            32 => *color = theme::green(),
            33 => *color = theme::yellow(),
            34 => *color = theme::accent(),
            35 => *color = theme::clr_annotation(),
            36 => *color = theme::clr_type(),
            37 => *color = theme::text(),
            90..=97 => {
                *color = match parts[index] {
                    91 => theme::red(),
                    92 => theme::green(),
                    93 => theme::yellow(),
                    94 => theme::clr_type(),
                    95 => theme::clr_annotation(),
                    96 => theme::clr_type(),
                    97 => theme::text(),
                    _ => theme::muted(),
                }
            }
            38 => {
                if parts.get(index + 1) == Some(&5) {
                    let value = parts.get(index + 2).copied().unwrap_or(7) as u8;
                    *color = ansi_256(value);
                    index += 2;
                } else if parts.get(index + 1) == Some(&2) {
                    let r = parts.get(index + 2).copied().unwrap_or(255) as u8;
                    let g = parts.get(index + 3).copied().unwrap_or(255) as u8;
                    let b = parts.get(index + 4).copied().unwrap_or(255) as u8;
                    *color = Color32::from_rgb(r, g, b);
                    index += 4;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn ansi_256(value: u8) -> Color32 {
    if value < 16 {
        return match value {
            0 => Color32::from_rgb(0x15, 0x18, 0x1d),
            1 => theme::red(),
            2 => theme::green(),
            3 => theme::yellow(),
            4 => theme::accent(),
            5 => theme::clr_annotation(),
            6 => theme::clr_type(),
            7 => theme::text(),
            8 => theme::muted(),
            9 => theme::red(),
            10 => theme::green(),
            11 => theme::yellow(),
            12 => theme::clr_type(),
            13 => theme::clr_annotation(),
            14 => theme::clr_type(),
            _ => theme::text(),
        };
    }
    if value < 232 {
        let index = value - 16;
        let levels = [0, 95, 135, 175, 215, 255];
        let r = levels[(index / 36) as usize];
        let g = levels[((index % 36) / 6) as usize];
        let b = levels[(index % 6) as usize];
        return Color32::from_rgb(r, g, b);
    }
    let gray = 8 + (value - 232) * 10;
    Color32::from_rgb(gray, gray, gray)
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

fn build_source_signatures(index: &ProjectIndex) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for (path, file) in &index.files {
        let package = file.package.as_deref().unwrap_or("");
        for symbol in &file.symbols {
            if !symbol.kind.is_type()
                || symbol.name == "<anonymous>"
                || symbol.name.starts_with('<')
            {
                continue;
            }
            let chain = symbol
                .container
                .as_deref()
                .filter(|container| !container.is_empty())
                .map_or_else(
                    || symbol.name.clone(),
                    |container| format!("{container}.{}", symbol.name),
                );
            let binary = chain.replace('.', "$");
            let fq = if package.is_empty() {
                binary
            } else {
                format!("{package}.{binary}")
            };
            map.insert(format!("L{};", fq.replace('.', "/")), path.clone());
        }
        if let Some(stem) = path.file_stem() {
            let name = stem.to_string_lossy();
            if !name.is_empty() {
                let fq = if package.is_empty() {
                    name.into_owned()
                } else {
                    format!("{package}.{name}")
                };
                map.entry(format!("L{};", fq.replace('.', "/")))
                    .or_insert_with(|| path.clone());
            }
        }
    }
    map
}

fn path_for_location(
    index: Option<&ProjectIndex>,
    signatures: &HashMap<String, PathBuf>,
    location: &SourceLocation,
) -> Option<PathBuf> {
    let signature = location
        .signature
        .split('<')
        .next()
        .unwrap_or(&location.signature);
    if let Some(path) = signatures.get(signature) {
        return Some(path.clone());
    }
    let source = location.source_file.as_deref()?;
    index.and_then(|index| {
        index
            .files
            .keys()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy() == source)
            })
            .cloned()
    })
}

fn type_encloses_line(source: &str, start_line: usize, target_line: usize) -> bool {
    if target_line < start_line {
        return false;
    }
    if target_line == start_line {
        return true;
    }
    let mut state = BraceScanState::default();
    let mut depth = 0_i32;
    for line in source
        .lines()
        .skip(start_line.saturating_sub(1))
        .take(target_line - start_line + 1)
    {
        scan_braces(line, &mut state, &mut depth);
    }
    depth > 0
}

#[derive(Default)]
struct BraceScanState {
    block_comment: bool,
    in_string: bool,
    in_char: bool,
    escaped: bool,
}

fn scan_braces(line: &str, state: &mut BraceScanState, depth: &mut i32) {
    let characters: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if state.block_comment {
            if character == '*' && characters.get(index + 1) == Some(&'/') {
                state.block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if state.in_string || state.in_char {
            if state.escaped {
                state.escaped = false;
            } else if character == '\\' {
                state.escaped = true;
            } else if state.in_string && character == '"' {
                state.in_string = false;
            } else if state.in_char && character == '\'' {
                state.in_char = false;
            }
            index += 1;
            continue;
        }
        if character == '/' && characters.get(index + 1) == Some(&'/') {
            break;
        }
        if character == '/' && characters.get(index + 1) == Some(&'*') {
            state.block_comment = true;
            index += 2;
            continue;
        }
        match character {
            '"' => state.in_string = true,
            '\'' => state.in_char = true,
            '{' => *depth += 1,
            '}' => *depth -= 1,
            _ => {}
        }
        index += 1;
    }
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut output: String = text.chars().take(max.saturating_sub(1)).collect();
    output.push('…');
    output
}

fn simple_signature(signature: &str) -> String {
    let signature = signature.split('<').next().unwrap_or(signature);
    if let Some(value) = signature
        .strip_prefix('L')
        .and_then(|value| value.strip_suffix(';'))
    {
        return value.rsplit('/').next().unwrap_or(value).replace('$', ".");
    }
    signature.to_owned()
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

/// First sentence of the `/** ... */` Javadoc block attached to a declaration.
/// Allows modifiers, annotations and the return type between the comment and
/// `name_start` (e.g. `/** ... */ public String greet`), but nothing else
/// (`(`, `)`, `;`, `{`, `}` in between means the comment belongs elsewhere).
fn symbol_javadoc(source: &str, name_start: usize) -> Option<String> {
    let before = source.get(..name_start.min(source.len()))?;
    let comment_end = before.rfind("*/")?;
    let between = before[comment_end + 2..].trim();
    if between.contains(['(', ')', ';', '{', '}']) {
        return None;
    }
    let open = before[..comment_end].rfind("/**")?;
    // The opener must belong to this block, not an earlier closed one.
    if before[open..comment_end].contains("*/") {
        return None;
    }
    let body = before[open + 3..comment_end].trim();
    if body.is_empty() {
        return None;
    }
    Some(javadoc_first_sentence(body))
}

fn javadoc_first_sentence(body: &str) -> String {
    // Join continuation lines, dropping the leading `*`.
    let mut text = String::new();
    for line in body.lines() {
        let line = line.trim().trim_start_matches('*').trim();
        if !text.is_empty() && !text.ends_with(['-', '/']) {
            text.push(' ');
        }
        text.push_str(line);
    }
    // Unwrap inline tags: `{@link Foo}` -> `Foo`, `{@code x}` -> `x`.
    let mut clean = String::new();
    let mut rest = text.as_str();
    loop {
        let Some(open) = rest.find("{@") else {
            let tail = rest.trim();
            if !tail.is_empty() {
                if !clean.is_empty() {
                    clean.push(' ');
                }
                clean.push_str(tail);
            }
            break;
        };
        let head = rest[..open].trim();
        if !head.is_empty() {
            if !clean.is_empty() {
                clean.push(' ');
            }
            clean.push_str(head);
        }
        let Some(close_rel) = rest[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        let tag = rest[open + 2..close].trim();
        let mut content = tag
            .split_once(char::is_whitespace)
            .map(|(_, content)| content.trim())
            .unwrap_or("");
        content = content.split('#').next_back().unwrap_or(content).trim();
        content = content.trim_end_matches("()");
        if !content.is_empty() {
            if !clean.is_empty() {
                clean.push(' ');
            }
            clean.push_str(content);
        }
        rest = rest[close + 1..].trim_start();
    }
    // Strip remaining HTML tags.
    let mut plain = String::with_capacity(clean.len());
    let mut in_tag = false;
    for character in clean.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => plain.push(character),
            _ => {}
        }
    }
    let mut plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    for (bad, good) in [
        (" .", "."),
        (" ,", ","),
        (" ;", ";"),
        (" :", ":"),
        (" !", "!"),
        (" ?", "?"),
    ] {
        plain = plain.replace(bad, good);
    }
    // First sentence: cut at `. `, `.\n`, or a trailing `.`.
    let mut end = plain.len();
    for (index, window) in plain.char_indices() {
        if window == '.' {
            let after = plain.get(index + 1..).unwrap_or("");
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                end = index + 1;
                break;
            }
        }
    }
    let mut sentence: String = plain.chars().take(end).collect();
    sentence = sentence.trim().to_owned();
    if sentence.chars().count() > 200 {
        sentence = sentence.chars().take(197).collect::<String>() + "...";
    }
    sentence
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

fn javac_available(env: &build::BuildEnv) -> bool {
    if let Some(home) = &env.jdk_home {
        let candidate = home
            .join("bin")
            .join(format!("javac{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return true;
        }
    }
    std::process::Command::new("javac")
        .arg("-version")
        .output()
        .is_ok()
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
    buffers: &HashMap<PathBuf, String>,
) -> Vec<SemanticProblem> {
    use reclipse_java::semantic::{
        override_problems, static_import_conflicts, unresolved_member_references,
        unresolved_type_references,
    };
    let (Some(index), Some(project_types)) = (index, project_types) else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    let push =
        |file: &PathBuf, line: usize, message: String, problems: &mut Vec<SemanticProblem>| {
            if problems.len() < 2_000 {
                problems.push(SemanticProblem {
                    file: file.clone(),
                    line,
                    message,
                });
            }
        };
    for (path, file) in &index.files {
        for (import, line) in unresolved_imports(file, project_types, classpath) {
            push(
                path,
                line,
                format!("import não resolvido: {import}"),
                &mut problems,
            );
            if problems.len() >= 2_000 {
                return problems;
            }
        }
        let source: Option<String> = buffers
            .get(path)
            .cloned()
            .or_else(|| fs::read_to_string(path).ok());
        let Some(source) = source.as_deref() else {
            continue;
        };
        for (name, line, owners) in
            static_import_conflicts(file, source, project_types, Some(index), classpath)
        {
            push(
                path,
                line,
                format!("static import ambíguo para '{name}': {}", owners.join(", ")),
                &mut problems,
            );
            if problems.len() >= 2_000 {
                return problems;
            }
        }
        for (name, line) in unresolved_type_references(file, project_types, classpath) {
            push(
                path,
                line,
                format!("tipo não resolvido: {name}"),
                &mut problems,
            );
            if problems.len() >= 2_000 {
                return problems;
            }
        }
        for (message, line) in
            unresolved_member_references(file, source, project_types, Some(index), classpath)
        {
            push(
                path,
                line,
                format!("membro não encontrado: {message}"),
                &mut problems,
            );
            if problems.len() >= 2_000 {
                return problems;
            }
        }
        for (message, line) in override_problems(file, project_types, Some(index), classpath) {
            push(path, line, message, &mut problems);
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

    #[test]
    fn detects_enclosing_type_braces() {
        let source = "class Outer {\n    class Inner {\n        void run() {}\n    }\n    void after() {}\n}\n";
        assert!(type_encloses_line(source, 1, 2));
        assert!(type_encloses_line(source, 2, 3));
        assert!(type_encloses_line(source, 1, 5));
        assert!(!type_encloses_line(source, 2, 5));
        assert!(!type_encloses_line(source, 1, 7));
        let strings = "class A {\n    String text = \"}{\";\n    void run() {}\n}\n";
        assert!(type_encloses_line(strings, 1, 3));
        assert!(!type_encloses_line(strings, 1, 5));
    }

    #[test]
    fn maps_debug_signatures_to_source_paths() {
        let mut signatures = HashMap::new();
        signatures.insert(
            "Lcom/acme/App$Inner;".to_owned(),
            PathBuf::from("src/com/acme/App.java"),
        );
        let location = SourceLocation {
            signature: "Lcom/acme/App$Inner;".to_owned(),
            source_file: Some("App.java".to_owned()),
            line: 42,
            method: "run".to_owned(),
            code_index: 7,
        };
        assert_eq!(
            path_for_location(None, &signatures, &location),
            Some(PathBuf::from("src/com/acme/App.java"))
        );
        assert_eq!(simple_signature("Lcom/acme/App$Inner;"), "App.Inner");
    }

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m ok"), "error ok");
        assert_eq!(strip_ansi("\u{1b}[1;32mbuild\u{1b}[0m"), "build");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[cfg(windows)]
    #[test]
    fn streams_process_output() {
        use std::collections::HashMap;
        use std::path::Path;
        use std::sync::atomic::AtomicBool;
        let cancel = AtomicBool::new(false);
        let mut captured = String::new();
        let output = build::run_piped_stream(
            Path::new("cmd"),
            &["/C".to_owned(), "echo reclipse-stream".to_owned()],
            &HashMap::new(),
            Path::new("."),
            &cancel,
            &mut |text| captured.push_str(text),
        )
        .unwrap();
        assert!(output.combined.contains("reclipse-stream"));
        assert!(captured.contains("reclipse-stream"));
    }

    #[test]
    fn extracts_javadoc_first_sentence() {
        let source = "/**\n * Greets the user.\n * Second sentence here.\n * @param name the name\n */\npublic String greet(String name) { return name; }";
        let offset = source.find("greet").unwrap();
        assert_eq!(
            symbol_javadoc(source, offset).as_deref(),
            Some("Greets the user.")
        );
        // Plain block comments are not Javadoc.
        let plain = "/* just a comment */\nvoid run() {}";
        assert_eq!(symbol_javadoc(plain, plain.find("run").unwrap()), None);
        // Inline tags are unwrapped.
        assert_eq!(
            javadoc_first_sentence("See {@link User} and {@code main}. Next."),
            "See User and main."
        );
    }
}
