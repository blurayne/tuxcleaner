use std::collections::{BTreeMap, BTreeSet};
use std::io::{Write, stdout};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::analyze::{AnalysisReport, DiskEntry, LargeFile, analyze_cancellable};
use crate::distro::Distribution;
use crate::executor::{CommandRunner, Executor};
use crate::history::{HistoryRecord, HistoryStore};
use crate::model::{ActionResult, CleanupAction, ScanReport};
use crate::purge::{PurgeCandidate, scan_artifacts};
use crate::scanner::Scanner;
use crate::size::format_bytes;
use crate::status::{self, SystemStatus};
use crate::uninstall::{ApplicationCatalog, ApplicationReport, UninstallPreview};
use crate::update::{self, UpdateInfo, UpdateResult};

mod execution;
mod view;

use execution::*;
use view::*;

const ANALYZE_MINIMUM_SIZE: u64 = 500_000_000;
const ANALYZE_MAX_DEPTH: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Clean,
    Uninstall,
    Analyze,
    Purge,
    Status,
    History,
    Update,
}

const MENU: &[(MenuAction, &str, &str)] = &[
    (
        MenuAction::Clean,
        "Clean",
        "Review known package, application, and developer caches",
    ),
    (
        MenuAction::Uninstall,
        "Uninstall",
        "List desktop applications and review exact removal plans",
    ),
    (
        MenuAction::Analyze,
        "Analyze",
        "Explore disk usage and remove selected large personal files",
    ),
    (
        MenuAction::Purge,
        "Purge",
        "Find old project build artifacts",
    ),
    (
        MenuAction::Status,
        "Status",
        "Show CPU, memory, disk, and uptime information",
    ),
    (
        MenuAction::History,
        "History",
        "Review previous cleanup operations",
    ),
    (
        MenuAction::Update,
        "Update",
        "Install a checksum-verified GitHub release",
    ),
];

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

enum Screen {
    Home,
    Analyze(Box<AnalyzeState>),
    Workflow(Box<WorkflowState>),
}

enum UiOutcome {
    Continue,
    Authorize,
    Quit,
}

struct App {
    home: PathBuf,
    screen: Screen,
    menu_state: ListState,
}

enum WorkflowData {
    Clean(ScanReport),
    Uninstall(ApplicationReport),
    Purge(Vec<PurgeCandidate>),
    Status(SystemStatus),
    History(Vec<HistoryRecord>),
    Update(UpdateInfo),
}

enum WorkflowResult {
    Actions(Vec<ActionResult>),
    Update(UpdateResult),
}

struct WorkflowState {
    action: MenuAction,
    data: Option<WorkflowData>,
    loading: Option<Receiver<Result<WorkflowData, String>>>,
    preparing: Option<Receiver<Result<Vec<UninstallPreview>, String>>>,
    executing: Option<Receiver<Result<WorkflowResult, String>>>,
    pending_execution: Option<WorkflowExecution>,
    authorization_requested: bool,
    list_state: ListState,
    selected: BTreeSet<usize>,
    previews: Vec<UninstallPreview>,
    confirming: bool,
    result: Option<WorkflowResult>,
    error: Option<String>,
    status: String,
    spinner: usize,
}

struct TuiCommandRunner;

impl CommandRunner for TuiCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        requires_root: bool,
    ) -> std::io::Result<std::process::Output> {
        if requires_root && !is_effective_root() {
            std::process::Command::new("sudo")
                .arg("-n")
                .arg("--")
                .arg(program)
                .args(args)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .output()
        } else {
            std::process::Command::new(program)
                .args(args)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .output()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalyzeMode {
    Browse,
    TopFiles,
}

struct LocationSnapshot {
    path: PathBuf,
    report: AnalysisReport,
    selected: Option<usize>,
}

struct AnalyzeState {
    home: PathBuf,
    path: PathBuf,
    report: Option<AnalysisReport>,
    scan: Option<Receiver<Result<AnalysisReport, String>>>,
    scan_cancel: Option<Arc<AtomicBool>>,
    scan_error: Option<String>,
    navigation: Vec<LocationSnapshot>,
    mode: AnalyzeMode,
    list_state: ListState,
    selected_files: BTreeMap<PathBuf, u64>,
    pending_delete: BTreeMap<PathBuf, u64>,
    confirming_delete: bool,
    results: Option<Vec<ActionResult>>,
    status: String,
    filter: String,
    filtering: bool,
    show_help: bool,
    spinner: usize,
    history_store: Option<HistoryStore>,
}

impl App {
    fn new(home: PathBuf) -> Self {
        Self {
            home,
            screen: Screen::Home,
            menu_state: ListState::default().with_selected(Some(0)),
        }
    }

    fn poll(&mut self) {
        match &mut self.screen {
            Screen::Analyze(analyze) => {
                analyze.poll_scan();
                analyze.spinner = (analyze.spinner + 1) % 4;
            }
            Screen::Workflow(workflow) => {
                workflow.poll();
                workflow.spinner = (workflow.spinner + 1) % 4;
            }
            Screen::Home => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UiOutcome {
        match &mut self.screen {
            Screen::Home => {
                let current = self.menu_state.selected().unwrap_or(0);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.menu_state.select(Some(current.saturating_sub(1)));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.menu_state
                            .select(Some((current + 1).min(MENU.len() - 1)));
                    }
                    KeyCode::Enter => {
                        let action = MENU[current].0;
                        if action == MenuAction::Analyze {
                            self.screen =
                                Screen::Analyze(Box::new(AnalyzeState::new(self.home.clone())));
                        } else {
                            self.screen = Screen::Workflow(Box::new(WorkflowState::new(
                                action,
                                self.home.clone(),
                            )));
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return UiOutcome::Quit,
                    _ => {}
                }
            }
            Screen::Analyze(analyze) => {
                if analyze.show_help {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                        analyze.show_help = false;
                    }
                    return UiOutcome::Continue;
                }
                if analyze.results.is_some() {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                        analyze.results = None;
                    }
                    return UiOutcome::Continue;
                }
                if analyze.confirming_delete {
                    match key.code {
                        KeyCode::Enter => analyze.confirm_delete(),
                        KeyCode::Esc | KeyCode::Char('q') => analyze.cancel_delete(),
                        _ => {}
                    }
                    return UiOutcome::Continue;
                }
                if analyze.filtering {
                    analyze.handle_filter_key(key);
                    return UiOutcome::Continue;
                }

                match key.code {
                    KeyCode::Char('q') => return UiOutcome::Quit,
                    KeyCode::Char('?') => analyze.show_help = true,
                    KeyCode::Up | KeyCode::Char('k') => analyze.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => analyze.move_selection(1),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        analyze.enter_selected()
                    }
                    KeyCode::Char(' ') => analyze.toggle_selection(),
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        analyze.begin_delete()
                    }
                    KeyCode::Char('t') => analyze.toggle_mode(),
                    KeyCode::Char('/') => analyze.begin_filter(),
                    KeyCode::Char('r') => analyze.refresh(),
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h')
                        if analyze.clear_filter_or_go_back() =>
                    {
                        self.screen = Screen::Home;
                    }
                    _ => {}
                }
            }
            Screen::Workflow(workflow) => {
                if workflow.handle_key(key, &self.home) {
                    self.screen = Screen::Home;
                } else if workflow.authorization_requested {
                    workflow.authorization_requested = false;
                    return UiOutcome::Authorize;
                }
            }
        }
        UiOutcome::Continue
    }

    fn finish_authorization(&mut self, result: Result<(), String>) {
        let Screen::Workflow(workflow) = &mut self.screen else {
            return;
        };
        match result {
            Ok(()) => workflow.start_pending_execution(self.home.clone()),
            Err(error) => {
                workflow.pending_execution = None;
                workflow.error = Some(error);
                workflow.status = "Administrator authorization failed".into();
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        match &mut self.screen {
            Screen::Home => draw_home(frame, &mut self.menu_state),
            Screen::Analyze(analyze) => draw_analyze(frame, analyze),
            Screen::Workflow(workflow) => draw_workflow(frame, workflow),
        }
    }
}

impl WorkflowState {
    fn new(action: MenuAction, home: PathBuf) -> Self {
        let loading = start_workflow_load(action, home);
        Self {
            action,
            data: None,
            loading: Some(loading),
            preparing: None,
            executing: None,
            pending_execution: None,
            authorization_requested: false,
            list_state: ListState::default(),
            selected: BTreeSet::new(),
            previews: Vec::new(),
            confirming: false,
            result: None,
            error: None,
            status: format!("Loading {}...", action_title(action)),
            spinner: 0,
        }
    }

    fn poll(&mut self) {
        if let Some(receiver) = &self.loading {
            match receiver.try_recv() {
                Ok(Ok(data)) => {
                    let length = workflow_len(&data);
                    self.data = Some(data);
                    self.loading = None;
                    self.list_state.select((length > 0).then_some(0));
                    self.status = if length == 0 {
                        "No matching items found".into()
                    } else {
                        workflow_ready_status(self.data.as_ref().unwrap())
                    };
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.loading = None;
                    self.status = "Loading failed".into();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("background task stopped unexpectedly".into());
                    self.loading = None;
                }
            }
        }

        if let Some(receiver) = &self.preparing {
            match receiver.try_recv() {
                Ok(Ok(previews)) => {
                    self.previews = previews;
                    self.preparing = None;
                    self.confirming = true;
                    self.status = "Removal plan ready for review".into();
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.preparing = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("preview task stopped unexpectedly".into());
                    self.preparing = None;
                }
            }
        }

        if let Some(receiver) = &self.executing {
            match receiver.try_recv() {
                Ok(Ok(result)) => {
                    self.result = Some(result);
                    self.executing = None;
                    self.confirming = false;
                    self.selected.clear();
                    self.status = "Operation completed".into();
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.executing = None;
                    self.confirming = false;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("operation stopped unexpectedly".into());
                    self.executing = None;
                    self.confirming = false;
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, home: &Path) -> bool {
        if self.error.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.error = None;
                if self.data.is_none() {
                    return true;
                }
            }
            return false;
        }
        if self.result.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                return true;
            }
            return false;
        }
        if self.executing.is_some() {
            self.status = "Operation in progress; wait for the result before going back".into();
            return false;
        }
        if self.loading.is_some() || self.preparing.is_some() {
            return matches!(key.code, KeyCode::Esc);
        }
        if self.confirming {
            match key.code {
                KeyCode::Enter => self.execute(home.to_path_buf()),
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.confirming = false;
                    self.previews.clear();
                    self.status = "Operation cancelled".into();
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => return true,
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char(' ') if workflow_is_selectable(self.data.as_ref()) => {
                self.toggle_selection()
            }
            KeyCode::Enter if workflow_is_actionable(self.data.as_ref()) => {
                self.begin_confirmation(home.to_path_buf())
            }
            KeyCode::Char('r') => {
                self.data = None;
                self.selected.clear();
                self.list_state.select(None);
                self.loading = Some(start_workflow_load(self.action, home.to_path_buf()));
                self.status = format!("Loading {}...", action_title(self.action));
            }
            _ => {}
        }
        false
    }

    fn move_selection(&mut self, direction: isize) {
        let length = self.data.as_ref().map(workflow_len).unwrap_or(0);
        if length == 0 {
            self.list_state.select(None);
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(length - 1)
        };
        self.list_state.select(Some(next));
    }

    fn toggle_selection(&mut self) {
        let Some(index) = self.list_state.selected() else {
            return;
        };
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        let total = selected_workflow_bytes(self.data.as_ref(), &self.selected);
        self.status = if self.selected.is_empty() {
            workflow_ready_status(self.data.as_ref().unwrap())
        } else {
            format!("{} selected · {}", self.selected.len(), format_bytes(total))
        };
    }

    fn begin_confirmation(&mut self, home: PathBuf) {
        if matches!(self.data, Some(WorkflowData::Update(_))) {
            self.confirming = true;
            return;
        }
        if self.selected.is_empty() {
            self.status = "Select at least one item with Space".into();
            return;
        }
        if let Some(WorkflowData::Uninstall(report)) = &self.data {
            let applications: Vec<_> = self
                .selected
                .iter()
                .filter_map(|index| report.applications.get(*index).cloned())
                .collect();
            self.status = "Preparing package-manager removal plan...".into();
            self.preparing = Some(start_uninstall_preview(home, applications));
        } else {
            self.confirming = true;
        }
    }

    fn execute(&mut self, home: PathBuf) {
        let Some(data) = &self.data else {
            return;
        };
        let selected = self.selected.clone();
        let previews = self.previews.clone();
        let request = match data {
            WorkflowData::Clean(report) => WorkflowExecution::Clean {
                distribution: report.distribution.clone(),
                items: selected
                    .iter()
                    .filter_map(|index| report.items.get(*index).cloned())
                    .collect(),
            },
            WorkflowData::Uninstall(report) => WorkflowExecution::Uninstall {
                distribution: report.distribution.clone(),
                applications: selected
                    .iter()
                    .filter_map(|index| report.applications.get(*index).cloned())
                    .collect(),
                previews,
            },
            WorkflowData::Purge(candidates) => WorkflowExecution::Purge {
                candidates: selected
                    .iter()
                    .filter_map(|index| candidates.get(*index).cloned())
                    .collect(),
            },
            WorkflowData::Update(_) => WorkflowExecution::Update,
            WorkflowData::Status(_) | WorkflowData::History(_) => return,
        };
        self.confirming = false;
        if request.needs_privilege() && !is_effective_root() {
            self.status = "Waiting for administrator authorization...".into();
            self.pending_execution = Some(request);
            self.authorization_requested = true;
        } else {
            self.start_execution(home, request);
        }
    }

    fn start_pending_execution(&mut self, home: PathBuf) {
        let Some(request) = self.pending_execution.take() else {
            self.error = Some("the privileged operation was not prepared".into());
            return;
        };
        self.start_execution(home, request);
    }

    fn start_execution(&mut self, home: PathBuf, request: WorkflowExecution) {
        self.status = format!("Running {}...", action_title(self.action));
        self.executing = Some(start_workflow_execution(home, request));
    }
}

impl AnalyzeState {
    fn new(home: PathBuf) -> Self {
        let mut state = Self {
            path: home.clone(),
            home,
            report: None,
            scan: None,
            scan_cancel: None,
            scan_error: None,
            navigation: Vec::new(),
            mode: AnalyzeMode::Browse,
            list_state: ListState::default(),
            selected_files: BTreeMap::new(),
            pending_delete: BTreeMap::new(),
            confirming_delete: false,
            results: None,
            status: "Scanning disk usage...".into(),
            filter: String::new(),
            filtering: false,
            show_help: false,
            spinner: 0,
            history_store: HistoryStore::system_default().ok(),
        };
        state.start_scan();
        state
    }

    fn start_scan(&mut self) {
        self.cancel_scan();
        let path = self.path.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = analyze_cancellable(
                &path,
                ANALYZE_MINIMUM_SIZE,
                ANALYZE_MAX_DEPTH,
                &worker_cancelled,
            )
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });
        self.report = None;
        self.scan = Some(receiver);
        self.scan_cancel = Some(cancelled);
        self.scan_error = None;
        self.list_state.select(None);
        self.status = "Scanning disk usage...".into();
    }

    fn poll_scan(&mut self) {
        let Some(receiver) = &self.scan else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(report)) => {
                let count = report.entries.len();
                self.status = format!(
                    "Scanned {} across {} files",
                    format_bytes(report.total_size),
                    report.total_files
                );
                self.report = Some(report);
                self.scan = None;
                self.scan_cancel = None;
                self.select_first_if_available(count);
            }
            Ok(Err(error)) => {
                self.scan_error = Some(error);
                self.scan = None;
                self.scan_cancel = None;
                self.status = "Scan failed".into();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.scan_error = Some("disk scan stopped unexpectedly".into());
                self.scan = None;
                self.scan_cancel = None;
                self.status = "Scan failed".into();
            }
        }
    }

    fn select_first_if_available(&mut self, count: usize) {
        self.list_state.select((count > 0).then_some(0));
    }

    fn visible_entries(&self) -> Vec<&DiskEntry> {
        let query = self.filter.to_ascii_lowercase();
        self.report
            .as_ref()
            .map(|report| {
                report
                    .entries
                    .iter()
                    .filter(|entry| {
                        query.is_empty()
                            || entry.name.to_ascii_lowercase().contains(&query)
                            || entry
                                .path
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .contains(&query)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_large_files(&self) -> Vec<&LargeFile> {
        let query = self.filter.to_ascii_lowercase();
        self.report
            .as_ref()
            .map(|report| {
                report
                    .large_files
                    .iter()
                    .filter(|file| {
                        query.is_empty()
                            || file
                                .path
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .contains(&query)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_len(&self) -> usize {
        match self.mode {
            AnalyzeMode::Browse => self.visible_entries().len(),
            AnalyzeMode::TopFiles => self.visible_large_files().len(),
        }
    }

    fn move_selection(&mut self, direction: isize) {
        let length = self.visible_len();
        if length == 0 {
            self.list_state.select(None);
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(length - 1)
        };
        self.list_state.select(Some(next));
    }

    fn enter_selected(&mut self) {
        if self.mode == AnalyzeMode::TopFiles || self.scan.is_some() {
            return;
        }
        let Some(index) = self.list_state.selected() else {
            return;
        };
        let Some(entry) = self.visible_entries().get(index).copied() else {
            return;
        };
        if !entry.is_dir {
            self.status = "Use Space to select the file, then d to remove it".into();
            return;
        }
        let next_path = entry.path.clone();
        let selected = self.list_state.selected();
        if let Some(report) = self.report.take() {
            self.navigation.push(LocationSnapshot {
                path: self.path.clone(),
                report,
                selected,
            });
        }
        self.path = next_path;
        self.filter.clear();
        self.start_scan();
    }

    fn focused_file(&self) -> Option<(PathBuf, u64)> {
        let index = self.list_state.selected()?;
        match self.mode {
            AnalyzeMode::Browse => {
                let entry = *self.visible_entries().get(index)?;
                is_selectable_personal_file(&self.home, &entry.path, entry.is_dir, entry.size)
                    .then(|| (entry.path.clone(), entry.size))
            }
            AnalyzeMode::TopFiles => {
                let file = *self.visible_large_files().get(index)?;
                is_selectable_personal_file(&self.home, &file.path, false, file.size)
                    .then(|| (file.path.clone(), file.size))
            }
        }
    }

    fn toggle_selection(&mut self) {
        if self.scan.is_some() {
            self.status = "Selection is available after the scan finishes".into();
            return;
        }
        let Some((path, size)) = self.focused_file() else {
            self.status = "Only non-hidden personal files can be selected".into();
            return;
        };
        if self.selected_files.remove(&path).is_none() {
            self.selected_files.insert(path, size);
        }
        self.update_selection_status();
    }

    fn begin_delete(&mut self) {
        if self.scan.is_some() {
            self.status = "Removal is available after the scan finishes".into();
            return;
        }
        self.pending_delete = if self.selected_files.is_empty() {
            self.focused_file().into_iter().collect()
        } else {
            self.selected_files.clone()
        };
        if self.pending_delete.is_empty() {
            self.status = "Select a non-hidden personal file first".into();
            return;
        }
        self.confirming_delete = true;
    }

    fn cancel_delete(&mut self) {
        self.confirming_delete = false;
        self.pending_delete.clear();
        self.status = "Removal cancelled".into();
    }

    fn confirm_delete(&mut self) {
        let executor = Executor::new(self.home.clone());
        let results: Vec<_> = self
            .pending_delete
            .iter()
            .map(|(path, size)| LargeFile {
                path: path.clone(),
                size: *size,
                modified_unix: None,
                app_data: false,
            })
            .map(|file| file.cleanup_item())
            .map(|item| executor.execute(&item, false))
            .collect();
        let distribution = Distribution::detect()
            .map(|value| value.name)
            .unwrap_or_else(|_| "Linux".into());
        if let Some(store) = &self.history_store {
            let _ = store.append(&HistoryRecord {
                timestamp: Utc::now(),
                distribution,
                command: "large-file-cleanup".into(),
                results: results.clone(),
            });
        }
        self.confirming_delete = false;
        self.pending_delete.clear();
        self.selected_files.clear();
        self.results = Some(results);
        self.start_scan();
    }

    fn update_selection_status(&mut self) {
        let total: u64 = self.selected_files.values().sum();
        self.status = if self.selected_files.is_empty() {
            self.report
                .as_ref()
                .map(|report| format!("Scanned {}", format_bytes(report.total_size)))
                .unwrap_or_default()
        } else {
            format!(
                "{} selected, {}",
                self.selected_files.len(),
                format_bytes(total)
            )
        };
    }

    fn toggle_mode(&mut self) {
        if self.scan.is_some() {
            return;
        }
        self.mode = match self.mode {
            AnalyzeMode::Browse => AnalyzeMode::TopFiles,
            AnalyzeMode::TopFiles => AnalyzeMode::Browse,
        };
        self.filter.clear();
        self.filtering = false;
        self.selected_files.clear();
        self.list_state = ListState::default();
        self.select_first_if_available(self.visible_len());
        self.status = match self.mode {
            AnalyzeMode::Browse => "Directory explorer".into(),
            AnalyzeMode::TopFiles => "Largest files in this location".into(),
        };
    }

    fn begin_filter(&mut self) {
        if self.report.is_some() {
            self.filtering = true;
            self.selected_files.clear();
            self.status = "Type to filter, Enter to apply, Esc to clear".into();
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace | KeyCode::Delete => {
                self.filter.pop();
            }
            KeyCode::Char(character) => self.filter.push(character),
            _ => {}
        }
        let length = self.visible_len();
        self.select_first_if_available(length);
        self.selected_files.clear();
    }

    fn clear_filter_or_go_back(&mut self) -> bool {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.select_first_if_available(self.visible_len());
            return false;
        }
        if self.mode == AnalyzeMode::TopFiles {
            self.toggle_mode();
            return false;
        }
        if let Some(snapshot) = self.navigation.pop() {
            self.cancel_scan();
            self.path = snapshot.path;
            self.report = Some(snapshot.report);
            self.scan = None;
            self.scan_error = None;
            self.list_state = ListState::default().with_selected(snapshot.selected);
            self.status = "Returned to previous location".into();
            return false;
        }
        true
    }

    fn refresh(&mut self) {
        if self.scan.is_some() {
            self.status = "A scan is already in progress".into();
            return;
        }
        self.filter.clear();
        self.filtering = false;
        self.selected_files.clear();
        self.start_scan();
    }

    fn cancel_scan(&mut self) {
        if let Some(cancelled) = self.scan_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.scan = None;
    }

    fn render_items(&self) -> Vec<ListItem<'static>> {
        let total = self.report.as_ref().map_or(0, |report| report.total_size);
        match self.mode {
            AnalyzeMode::Browse => self
                .visible_entries()
                .into_iter()
                .map(|entry| {
                    let selected = self.selected_files.contains_key(&entry.path);
                    let selectable = is_selectable_personal_file(
                        &self.home,
                        &entry.path,
                        entry.is_dir,
                        entry.size,
                    );
                    disk_item(entry, total, selected, selectable)
                })
                .collect(),
            AnalyzeMode::TopFiles => self
                .visible_large_files()
                .into_iter()
                .map(|file| {
                    let selected = self.selected_files.contains_key(&file.path);
                    let selectable =
                        is_selectable_personal_file(&self.home, &file.path, false, file.size);
                    file_item(file, total, selected, selectable)
                })
                .collect(),
        }
    }
}

impl Drop for AnalyzeState {
    fn drop(&mut self) {
        self.cancel_scan();
    }
}

pub fn interactive_app() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(home);

    loop {
        app.poll();
        terminal.draw(|frame| app.draw(frame))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.handle_key(key) {
            UiOutcome::Continue => {}
            UiOutcome::Authorize => {
                suspend_terminal(&mut terminal)?;
                let authorization = authorize_sudo();
                resume_terminal(&mut terminal)?;
                app.finish_authorization(authorization);
            }
            UiOutcome::Quit => {
                terminal.show_cursor()?;
                return Ok(());
            }
        }
    }
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    terminal.show_cursor()?;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    println!();
    println!("TuxCleaner needs administrator authorization for the selected operation.");
    println!("Enter your sudo password below. Password characters are intentionally not shown.");
    stdout().flush()?;
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, Hide)?;
    Ok(())
}

fn authorize_sudo() -> Result<(), String> {
    if is_effective_root() {
        return Ok(());
    }
    let status = std::process::Command::new("sudo")
        .arg("-v")
        .status()
        .map_err(|error| format!("failed to start sudo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "sudo authorization failed or was cancelled ({status})"
        ))
    }
}

fn is_effective_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use tempfile::tempdir;

    use super::*;

    fn ready_analyze(home: &Path, file: PathBuf) -> AnalyzeState {
        let size = fs::metadata(&file).unwrap().len();
        AnalyzeState {
            home: home.to_path_buf(),
            path: home.to_path_buf(),
            report: Some(AnalysisReport {
                root: home.to_path_buf(),
                entries: vec![DiskEntry {
                    name: file.file_name().unwrap().to_string_lossy().into_owned(),
                    path: file.clone(),
                    size,
                    is_dir: false,
                }],
                large_files: vec![LargeFile {
                    path: file,
                    size,
                    modified_unix: None,
                    app_data: false,
                }],
                total_size: size,
                total_files: 1,
                skipped_entries: 0,
            }),
            scan: None,
            scan_cancel: None,
            scan_error: None,
            navigation: Vec::new(),
            mode: AnalyzeMode::Browse,
            list_state: ListState::default().with_selected(Some(0)),
            selected_files: BTreeMap::new(),
            pending_delete: BTreeMap::new(),
            confirming_delete: false,
            results: None,
            status: "Ready".into(),
            filter: String::new(),
            filtering: false,
            show_help: false,
            spinner: 0,
            history_store: Some(HistoryStore::new(home.join("history.jsonl"))),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn escape_from_analyze_root_returns_to_home() {
        let root = tempdir().unwrap();
        let file = root.path().join("large.bin");
        fs::File::create(&file)
            .unwrap()
            .set_len(ANALYZE_MINIMUM_SIZE)
            .unwrap();
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file)));

        assert!(matches!(
            app.handle_key(key(KeyCode::Esc)),
            UiOutcome::Continue
        ));
        assert!(matches!(app.screen, Screen::Home));
    }

    #[test]
    fn analyze_requires_confirmation_before_permanently_removing_a_file() {
        let root = tempdir().unwrap();
        let file = root.path().join("Downloads/large.bin");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::File::create(&file)
            .unwrap()
            .set_len(ANALYZE_MINIMUM_SIZE)
            .unwrap();
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file.clone())));

        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(file.exists());
        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert!(analyze.confirming_delete);

        app.handle_key(key(KeyCode::Enter));
        assert!(!file.exists());
    }

    #[test]
    fn hidden_files_are_not_selectable() {
        let home = Path::new("/home/tester");
        assert!(!is_selectable_personal_file(
            home,
            Path::new("/home/tester/.config/private.bin"),
            false,
            ANALYZE_MINIMUM_SIZE
        ));
        assert!(is_selectable_personal_file(
            home,
            Path::new("/home/tester/Downloads/archive.iso"),
            false,
            ANALYZE_MINIMUM_SIZE
        ));
        assert!(!is_selectable_personal_file(
            home,
            Path::new("/home/tester/Downloads/note.txt"),
            false,
            ANALYZE_MINIMUM_SIZE - 1
        ));
    }

    #[test]
    fn analyze_view_makes_removal_and_back_navigation_discoverable() {
        let root = tempdir().unwrap();
        let file = root.path().join("large.bin");
        fs::File::create(&file)
            .unwrap()
            .set_len(ANALYZE_MINIMUM_SIZE)
            .unwrap();
        let mut state = ready_analyze(root.path(), file);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw_analyze(frame, &mut state))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("large.bin"));
        assert!(rendered.contains("d Delete"));
        assert!(rendered.contains("Esc back"));
    }

    #[test]
    fn entering_analyze_renders_loading_immediately_and_escape_cancels_scan() {
        let root = tempdir().unwrap();
        let state = AnalyzeState::new(root.path().to_path_buf());
        let cancelled = Arc::clone(state.scan_cancel.as_ref().unwrap());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(state));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| app.draw(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("Scanning disk usage"));

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::Home));
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn every_main_menu_action_opens_an_immediate_loading_screen() {
        let root = tempdir().unwrap();
        for action in [
            MenuAction::Clean,
            MenuAction::Uninstall,
            MenuAction::Purge,
            MenuAction::Status,
            MenuAction::History,
            MenuAction::Update,
        ] {
            let (_sender, receiver) = mpsc::channel();
            let mut workflow = WorkflowState {
                action,
                data: None,
                loading: Some(receiver),
                preparing: None,
                executing: None,
                pending_execution: None,
                authorization_requested: false,
                list_state: ListState::default(),
                selected: BTreeSet::new(),
                previews: Vec::new(),
                confirming: false,
                result: None,
                error: None,
                status: format!("Loading {}...", action_title(action)),
                spinner: 0,
            };
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal
                .draw(|frame| draw_workflow(frame, &mut workflow))
                .unwrap();

            let rendered: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                rendered.contains(&format!("Loading {}", action_title(action))),
                "missing loading state for {}",
                action_title(action)
            );
            assert!(workflow.handle_key(key(KeyCode::Esc), root.path()));
        }
    }

    #[test]
    fn destructive_workflow_cannot_go_back_while_execution_is_active() {
        let root = tempdir().unwrap();
        let (_sender, receiver) = mpsc::channel();
        let mut workflow = WorkflowState {
            action: MenuAction::Clean,
            data: None,
            loading: None,
            preparing: None,
            executing: Some(receiver),
            pending_execution: None,
            authorization_requested: false,
            list_state: ListState::default(),
            selected: BTreeSet::new(),
            previews: Vec::new(),
            confirming: false,
            result: None,
            error: None,
            status: "Running Clean...".into(),
            spinner: 0,
        };

        assert!(!workflow.handle_key(key(KeyCode::Esc), root.path()));
        assert!(workflow.status.contains("wait for the result"));
    }

    #[test]
    fn workflow_detects_every_root_command_before_execution() {
        let direct = CleanupAction::Command {
            program: "apt-get".into(),
            args: vec!["clean".into()],
            requires_root: true,
        };
        let sequence = CleanupAction::CommandSequence {
            commands: vec![crate::model::CommandSpec {
                program: "paccache".into(),
                args: vec!["-rk1".into()],
                requires_root: true,
            }],
        };
        let personal = CleanupAction::RemovePersonalFile {
            path: PathBuf::from("/home/tester/Downloads/file.iso"),
        };

        assert!(cleanup_action_requires_root(&direct));
        assert!(cleanup_action_requires_root(&sequence));
        assert!(!cleanup_action_requires_root(&personal));
    }
}
