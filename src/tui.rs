use std::collections::{BTreeMap, BTreeSet};
use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
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

use crate::analyze::{DiskEntry, LargeFile, ScanUpdate, spawn_streaming_scan};
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
/// Number of ancestor levels (above the active location) that are allowed to keep a live
/// background scan running. The plan's "N" — with the active location included, up to
/// `ANALYZE_LIVE_SCAN_DEPTH + 1` levels may hold a live handle at once.
const ANALYZE_LIVE_SCAN_DEPTH: usize = 2;
const ANALYZE_LIVE_SCAN_CAP: usize = ANALYZE_LIVE_SCAN_DEPTH + 1;
/// How often the active location's display order is rebuilt from live data.
const ANALYZE_REORDER_INTERVAL: Duration = Duration::from_millis(400);
/// Upper bound on `ScanUpdate` messages drained per location per poll tick, so a very chatty
/// scan can never starve the UI thread.
const ANALYZE_MAX_UPDATES_PER_TICK: usize = 4096;

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
        // Popped before leaving the alternate screen, mirroring the push order in
        // `interactive_app` (push happens after `EnterAlternateScreen`). Runs unconditionally, on
        // every teardown path including a panic unwinding through this scope, so the terminal is
        // never left with the kitty keyboard protocol's DISAMBIGUATE_ESCAPE_CODES flag still
        // active. A terminal that never understood the push in the first place silently ignores
        // this pop too; both directions of the exchange are one-way private-mode escape
        // sequences that unsupported terminals are specified to discard.
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
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

/// Why `AnalyzeState::focused_file` could not resolve a selectable file under the cursor.
/// Distinguishing "nothing is under the cursor at all" from "something is under the cursor but
/// it cannot be selected" lets `toggle_selection` report the specific, actionable reason instead
/// of one generic message for every cause.
#[derive(Debug)]
enum FocusRefusal {
    NoSelection,
    NotSelectable(PersonalFileRefusal),
}

/// A live streaming scan in progress for one `Location`.
struct ScanHandle {
    receiver: Receiver<ScanUpdate>,
    cancel: Arc<AtomicBool>,
}

impl ScanHandle {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// One level of the drill-down stack. The last entry in `AnalyzeState::locations` is the
/// currently displayed ("active") location; earlier entries are ancestors that may still have a
/// live background scan draining (up to `ANALYZE_LIVE_SCAN_CAP`), preserving whatever partial
/// data they gathered so Back does not force a re-scan.
struct Location {
    path: PathBuf,
    /// Backing store keyed by top-level child path, upserted on every `Progress` update.
    entries: BTreeMap<PathBuf, DiskEntry>,
    /// The cumulative file count last reported for each top-level child, kept alongside
    /// `entries` so incoming (cumulative) `Progress` updates can be turned into deltas for the
    /// running `total_files` counter.
    bucket_files: BTreeMap<PathBuf, u64>,
    /// Displayed order, rebuilt from `entries` at most every `ANALYZE_REORDER_INTERVAL`.
    sorted: Vec<DiskEntry>,
    large_files: Vec<LargeFile>,
    total_size: u64,
    total_files: u64,
    skipped: u64,
    /// Selection tracked by path identity rather than list index, so a background re-sort can
    /// never cause the cursor (or an Enter keypress) to silently jump to a different row.
    /// Used in `Browse` mode, where `sorted` is the identity-tracked collection.
    selected: Option<PathBuf>,
    /// The `TopFiles` mode counterpart to `selected`: identity-tracked selection into
    /// `large_files`. Needed because `large_files` keeps growing during a live scan (see the
    /// `ScanUpdate::Large` arm in `drain_location`) and is freshly re-sorted by size on every
    /// `visible_large_files()` call, so a numeric list index alone cannot survive either event.
    selected_large_file: Option<PathBuf>,
    last_reorder: Instant,
    complete: bool,
    error: Option<String>,
    /// Set whenever `entries`/`large_files` change or the location becomes active again;
    /// cleared once `reorder` runs. Lets `poll` reorder promptly on completion or activation
    /// without waiting out the throttle interval.
    needs_reorder: bool,
    scan: Option<ScanHandle>,
}

impl Location {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: BTreeMap::new(),
            bucket_files: BTreeMap::new(),
            sorted: Vec::new(),
            large_files: Vec::new(),
            total_size: 0,
            total_files: 0,
            skipped: 0,
            selected: None,
            selected_large_file: None,
            last_reorder: Instant::now(),
            complete: false,
            error: None,
            needs_reorder: true,
            scan: None,
        }
    }

    fn has_data(&self) -> bool {
        !self.entries.is_empty() || !self.large_files.is_empty()
    }

    fn start_scan(&mut self) {
        self.cancel_scan();
        let (receiver, cancel) =
            spawn_streaming_scan(self.path.clone(), ANALYZE_MINIMUM_SIZE, ANALYZE_MAX_DEPTH);
        self.entries.clear();
        self.bucket_files.clear();
        self.sorted.clear();
        self.large_files.clear();
        self.total_size = 0;
        self.total_files = 0;
        self.skipped = 0;
        self.selected = None;
        self.selected_large_file = None;
        self.complete = false;
        self.error = None;
        self.needs_reorder = true;
        self.scan = Some(ScanHandle { receiver, cancel });
    }

    fn cancel_scan(&mut self) {
        if let Some(handle) = self.scan.take() {
            handle.cancel();
        }
    }

    fn apply_progress(&mut self, top: PathBuf, size: u64, files: u64, is_dir: bool) {
        let name = top
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| top.display().to_string());
        let previous_size = self.entries.get(&top).map_or(0, |entry| entry.size);
        let previous_files = self.bucket_files.get(&top).copied().unwrap_or(0);
        self.total_size = self
            .total_size
            .saturating_sub(previous_size)
            .saturating_add(size);
        self.total_files = self
            .total_files
            .saturating_sub(previous_files)
            .saturating_add(files);
        self.bucket_files.insert(top.clone(), files);
        self.entries.insert(
            top.clone(),
            DiskEntry {
                name,
                path: top,
                size,
                is_dir,
            },
        );
        self.needs_reorder = true;
    }

    fn reorder(&mut self) {
        let (sorted, selected) =
            reorder_and_reconcile(&self.entries, &self.sorted, self.selected.as_deref());
        self.sorted = sorted;
        self.selected = selected;
        self.last_reorder = Instant::now();
        self.needs_reorder = false;
    }
}

/// Pure, unit-testable core of the reorder/reconcile step: rebuilds the size-desc (name
/// tie-break) display order from the backing map, then resolves the identity-tracked selection
/// against the new order. If the previously selected path is still present, it stays selected
/// regardless of where it moved to. If it vanished, the same position in the previous order is
/// used as a stable fallback; if there was no previous order either, the first row is selected.
fn reorder_and_reconcile(
    entries: &BTreeMap<PathBuf, DiskEntry>,
    previous_sorted: &[DiskEntry],
    selected: Option<&Path>,
) -> (Vec<DiskEntry>, Option<PathBuf>) {
    let mut sorted: Vec<DiskEntry> = entries.values().cloned().collect();
    sorted.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    let resolved = match selected {
        Some(path) if sorted.iter().any(|entry| entry.path == path) => Some(path.to_path_buf()),
        Some(path) => previous_sorted
            .iter()
            .position(|entry| entry.path == path)
            .and_then(|index| sorted.get(index))
            .map(|entry| entry.path.clone())
            .or_else(|| sorted.first().map(|entry| entry.path.clone())),
        None => sorted.first().map(|entry| entry.path.clone()),
    };

    (sorted, resolved)
}

/// Drains a bounded number of pending updates for one location's live scan (if any), updating
/// its accumulated data. Never treats a plain disconnect as an error: the streaming protocol
/// always sends an explicit `Done` (or `Error`) before the sender is dropped, so an observed
/// disconnect with neither seen this tick just means the scan was cancelled (e.g. evicted by the
/// depth cap) and is not a user-facing failure.
fn drain_location(location: &mut Location) {
    let Some(handle) = location.scan.take() else {
        return;
    };
    let mut budget = ANALYZE_MAX_UPDATES_PER_TICK;
    loop {
        if budget == 0 {
            location.scan = Some(handle);
            return;
        }
        match handle.receiver.try_recv() {
            Ok(ScanUpdate::Progress {
                top,
                size,
                files,
                is_dir,
            }) => location.apply_progress(top, size, files, is_dir),
            Ok(ScanUpdate::Large(file)) => {
                location.large_files.push(file);
                // Growing `large_files` shifts rows in the size-sorted `TopFiles` view exactly
                // like a `Progress` update shifts rows in `Browse`, so it must trigger the same
                // reorder/reconcile pass; see `reconcile_list_state`.
                location.needs_reorder = true;
            }
            Ok(ScanUpdate::Skipped(skipped)) => location.skipped = skipped,
            Ok(ScanUpdate::Done {
                total_size,
                total_files,
                skipped,
            }) => {
                location.total_size = total_size;
                location.total_files = total_files;
                location.skipped = skipped;
                location.complete = true;
                location.needs_reorder = true;
                return;
            }
            Ok(ScanUpdate::Error(error)) => {
                location.error = Some(error);
                location.complete = true;
                return;
            }
            Err(TryRecvError::Empty) => {
                location.scan = Some(handle);
                return;
            }
            Err(TryRecvError::Disconnected) => return,
        }
        budget -= 1;
    }
}

/// Size and directory-ness of a path pending removal in `analyze`, tracked alongside its size so
/// `draw_delete_confirmation` can warn about recursive directory removal without re-statting the
/// filesystem during rendering.
#[derive(Debug, Clone, Copy)]
struct PendingRemoval {
    size: u64,
    is_dir: bool,
    /// Whether this specific path was selected or is being deleted through one of the forcing
    /// gestures (Shift+D, Shift+Space, `S`), and so should be validated and executed with
    /// `Executor::validate_personal_file_forced` / `CleanupAction::RemovePersonalFile.force`
    /// rather than the normal, unforced path.
    force: bool,
}

/// Severity of `AnalyzeState::status`, driving how the status line and results overlay are
/// colored. Kept as a separate type (rather than inline `Color` values scattered across every
/// call site) so `AnalyzeState::set_status` can be the single place that ties a message to its
/// color, and so the renderer in `src/tui/view.rs` has one small, exhaustive match instead of
/// duplicating this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusSeverity {
    /// Routine progress or confirmation messages ("Scanning disk usage...", selection summaries).
    /// Rendered in the terminal's default color.
    Info,
    /// A refusal or guard message explaining why an action did not happen (below the size floor,
    /// not selectable, nothing selected). Rendered in yellow.
    Warning,
    /// A genuine failure, such as a removal that returned an error from the executor. Rendered
    /// in red.
    Error,
}

struct AnalyzeState {
    home: PathBuf,
    /// Drill-down stack; the last entry is the active (displayed) location.
    locations: Vec<Location>,
    mode: AnalyzeMode,
    list_state: ListState,
    selected_files: BTreeMap<PathBuf, PendingRemoval>,
    pending_delete: BTreeMap<PathBuf, PendingRemoval>,
    confirming_delete: bool,
    results: Option<Vec<ActionResult>>,
    status: String,
    /// Always set together with `status` through `set_status`, never assigned on its own, so it
    /// can never go stale and describe a different message than the one currently displayed.
    status_severity: StatusSeverity,
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
                analyze.poll();
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
                    // Most terminals send an identical byte sequence for Space and Shift+Space,
                    // so crossterm normally reports no SHIFT modifier for either and this arm
                    // alone would never see it. `interactive_app` requests the kitty keyboard
                    // protocol's DISAMBIGUATE_ESCAPE_CODES flag specifically so a supporting
                    // terminal attaches `KeyModifiers::SHIFT` to this same `Char(' ')` event
                    // instead of reporting a different code entirely; matching on the modifier
                    // here (rather than a second, separate `KeyCode` pattern) is what picks that
                    // up. On a terminal that does not support the protocol, `key.modifiers` is
                    // simply empty here and plain Space keeps working exactly as before.
                    KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        analyze.toggle_selection_forced()
                    }
                    KeyCode::Char(' ') => analyze.toggle_selection(),
                    // Portable force-select alias: Shift+Space cannot be relied on outside the
                    // kitty protocol, so `S` always works, unconditionally, on every terminal.
                    KeyCode::Char('S') => analyze.toggle_selection_forced(),
                    // Shift+D: portable everywhere (`KeyCode::Char('D')` does not depend on the
                    // keyboard enhancement flags at all), so no terminal caveat applies here.
                    KeyCode::Char('D') => analyze.begin_delete_forced(),
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
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {}
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
        let mut root = Location::new(home.clone());
        root.start_scan();
        Self {
            home,
            locations: vec![root],
            mode: AnalyzeMode::Browse,
            list_state: ListState::default(),
            selected_files: BTreeMap::new(),
            pending_delete: BTreeMap::new(),
            confirming_delete: false,
            results: None,
            status: "Scanning disk usage...".into(),
            status_severity: StatusSeverity::Info,
            filter: String::new(),
            filtering: false,
            show_help: false,
            spinner: 0,
            history_store: HistoryStore::system_default().ok(),
        }
    }

    /// The only place `status` and `status_severity` are assigned; every other method in this
    /// impl block goes through this setter instead of writing either field directly, so the two
    /// can never drift apart and describe different messages.
    fn set_status(&mut self, message: impl Into<String>, severity: StatusSeverity) {
        self.status = message.into();
        self.status_severity = severity;
    }

    fn active(&self) -> &Location {
        self.locations.last().expect("at least one location")
    }

    fn active_mut(&mut self) -> &mut Location {
        self.locations.last_mut().expect("at least one location")
    }

    fn active_has_data(&self) -> bool {
        self.active().has_data()
    }

    /// Drains every location's pending scan updates, then rebuilds the active location's display
    /// order (and reconciles `list_state` to it) whenever the throttle interval elapsed or the
    /// active location's data just changed (including finishing, or having just been revealed).
    fn poll(&mut self) {
        for location in &mut self.locations {
            drain_location(location);
        }
        let index = self.locations.len() - 1;
        let should_reorder = {
            let active = &self.locations[index];
            active.needs_reorder || active.last_reorder.elapsed() >= ANALYZE_REORDER_INTERVAL
        };
        if should_reorder {
            self.locations[index].reorder();
            self.reconcile_list_state();
        }
    }

    /// Syncs `list_state`'s numeric index to the active location's identity-tracked selection.
    ///
    /// Both modes need this: in `Browse`, background directory scanning mutates `entries` (via
    /// `apply_progress`) and can reorder `sorted`; in `TopFiles`, it mutates `large_files` (via
    /// the `ScanUpdate::Large` arm in `drain_location`), and `visible_large_files()` re-sorts by
    /// size on every call. Either way, without this reconciliation the numeric index in
    /// `list_state` stays put while the row it points at silently changes identity -- so a
    /// `d` keypress could target a different file than the one the user was actually looking at.
    fn reconcile_list_state(&mut self) {
        match self.mode {
            AnalyzeMode::Browse => {
                let selected_path = self.active().selected.clone();
                let (index, resolved_path) = {
                    let visible = self.visible_entries();
                    let index = selected_path
                        .as_deref()
                        .and_then(|path| visible.iter().position(|entry| entry.path == path))
                        .or((!visible.is_empty()).then_some(0));
                    let resolved_path = index
                        .and_then(|index| visible.get(index))
                        .map(|entry| entry.path.clone());
                    (index, resolved_path)
                };
                self.list_state.select(index);
                if let Some(path) = resolved_path {
                    self.active_mut().selected = Some(path);
                }
            }
            AnalyzeMode::TopFiles => {
                let selected_path = self.active().selected_large_file.clone();
                let (index, resolved_path) = {
                    let visible = self.visible_large_files();
                    let index = selected_path
                        .as_deref()
                        .and_then(|path| visible.iter().position(|file| file.path == path))
                        .or((!visible.is_empty()).then_some(0));
                    let resolved_path = index
                        .and_then(|index| visible.get(index))
                        .map(|file| file.path.clone());
                    (index, resolved_path)
                };
                self.list_state.select(index);
                if let Some(path) = resolved_path {
                    self.active_mut().selected_large_file = Some(path);
                }
            }
        }
    }

    fn select_first_if_available(&mut self, count: usize) {
        if count == 0 {
            self.list_state.select(None);
            return;
        }
        self.list_state.select(Some(0));
        match self.mode {
            AnalyzeMode::Browse => {
                if let Some(path) = self
                    .visible_entries()
                    .first()
                    .map(|entry| entry.path.clone())
                {
                    self.active_mut().selected = Some(path);
                }
            }
            AnalyzeMode::TopFiles => {
                if let Some(path) = self
                    .visible_large_files()
                    .first()
                    .map(|file| file.path.clone())
                {
                    self.active_mut().selected_large_file = Some(path);
                }
            }
        }
    }

    fn visible_entries(&self) -> Vec<&DiskEntry> {
        let query = self.filter.to_ascii_lowercase();
        self.active()
            .sorted
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
    }

    fn visible_large_files(&self) -> Vec<&LargeFile> {
        let query = self.filter.to_ascii_lowercase();
        let mut files: Vec<&LargeFile> = self
            .active()
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
            .collect();
        files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        files
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
        match self.mode {
            AnalyzeMode::Browse => {
                if let Some(path) = self
                    .visible_entries()
                    .get(next)
                    .map(|entry| entry.path.clone())
                {
                    self.active_mut().selected = Some(path);
                }
            }
            AnalyzeMode::TopFiles => {
                if let Some(path) = self
                    .visible_large_files()
                    .get(next)
                    .map(|file| file.path.clone())
                {
                    self.active_mut().selected_large_file = Some(path);
                }
            }
        }
    }

    fn enter_selected(&mut self) {
        if self.mode == AnalyzeMode::TopFiles {
            return;
        }
        // `selected` is `None` until the first entry has streamed in and been reconciled, so
        // this alone blocks entering before there is anything to enter -- no separate "scan
        // still running" guard is needed. Reading it directly (rather than indexing through
        // `list_state`/`sorted`) keeps this immune to a reorder landing in the same tick.
        let Some(target) = self.active().selected.clone() else {
            return;
        };
        let Some(entry) = self.active().entries.get(&target).cloned() else {
            return;
        };
        if !entry.is_dir {
            self.set_status(
                "Use Space to select the file, then d to remove it",
                StatusSeverity::Warning,
            );
            return;
        }
        self.push_location(entry.path);
    }

    fn push_location(&mut self, path: PathBuf) {
        let mut location = Location::new(path);
        location.start_scan();
        self.locations.push(location);
        self.enforce_scan_depth_cap();
        self.filter.clear();
        self.filtering = false;
        self.selected_files.clear();
        self.list_state = ListState::default();
        self.set_status("Scanning disk usage...", StatusSeverity::Info);
    }

    /// Keeps at most `ANALYZE_LIVE_SCAN_CAP` locations with a live scan handle at once, evicting
    /// the oldest ancestor first. Eviction only cancels the handle; whatever data that location
    /// already gathered is kept, so revisiting it later still shows partial/complete results
    /// instead of an empty screen.
    fn enforce_scan_depth_cap(&mut self) {
        let mut live: Vec<usize> = self
            .locations
            .iter()
            .enumerate()
            .filter(|(_, location)| location.scan.is_some())
            .map(|(index, _)| index)
            .collect();
        while live.len() > ANALYZE_LIVE_SCAN_CAP {
            let oldest = live.remove(0);
            self.locations[oldest].cancel_scan();
        }
    }

    /// Resolves the row under the cursor to a removable path, if any. `force` selects which of
    /// `personal_file_selectability` / `personal_file_selectability_forced` gates the row; the
    /// resulting `PendingRemoval.force` records the same flag, so the eventual removal is
    /// validated and executed the same way it was offered here.
    fn focused_file_impl(&self, force: bool) -> Result<(PathBuf, PendingRemoval), FocusRefusal> {
        let index = self
            .list_state
            .selected()
            .ok_or(FocusRefusal::NoSelection)?;
        let selectability_of = |path: &Path, is_dir: bool, size: u64| {
            if force {
                personal_file_selectability_forced(&self.home, path, is_dir, size)
            } else {
                personal_file_selectability(&self.home, path, is_dir, size)
            }
        };
        match self.mode {
            AnalyzeMode::Browse => {
                let entry = *self
                    .visible_entries()
                    .get(index)
                    .ok_or(FocusRefusal::NoSelection)?;
                selectability_of(&entry.path, entry.is_dir, entry.size)
                    .map(|()| {
                        (
                            entry.path.clone(),
                            PendingRemoval {
                                size: entry.size,
                                is_dir: entry.is_dir,
                                force,
                            },
                        )
                    })
                    .map_err(FocusRefusal::NotSelectable)
            }
            AnalyzeMode::TopFiles => {
                let file = *self
                    .visible_large_files()
                    .get(index)
                    .ok_or(FocusRefusal::NoSelection)?;
                selectability_of(&file.path, false, file.size)
                    .map(|()| {
                        (
                            file.path.clone(),
                            PendingRemoval {
                                size: file.size,
                                is_dir: false,
                                force,
                            },
                        )
                    })
                    .map_err(FocusRefusal::NotSelectable)
            }
        }
    }

    fn focused_file(&self) -> Result<(PathBuf, PendingRemoval), FocusRefusal> {
        self.focused_file_impl(false)
    }

    /// The forced counterpart to `focused_file`, used by the analyze screen's forcing gestures
    /// (Shift+Space, `S`, and Shift+D when nothing is already selected).
    fn focused_file_forced(&self) -> Result<(PathBuf, PendingRemoval), FocusRefusal> {
        self.focused_file_impl(true)
    }

    fn toggle_selection_impl(&mut self, force: bool) {
        if !self.active_has_data() {
            self.set_status(
                "Selection is available once items appear",
                StatusSeverity::Warning,
            );
            return;
        }
        let focused = if force {
            self.focused_file_forced()
        } else {
            self.focused_file()
        };
        match focused {
            Ok((path, entry)) => {
                if self.selected_files.remove(&path).is_none() {
                    self.selected_files.insert(path, entry);
                }
                self.update_selection_status();
            }
            Err(FocusRefusal::NoSelection) => {
                self.set_status("Nothing is selected", StatusSeverity::Warning);
            }
            Err(FocusRefusal::NotSelectable(refusal)) => {
                self.set_status(
                    personal_file_refusal_message(refusal),
                    StatusSeverity::Warning,
                );
            }
        }
    }

    fn toggle_selection(&mut self) {
        self.toggle_selection_impl(false);
    }

    /// Force-select (Shift+Space or `S`): like `toggle_selection`, but bypasses the size floor,
    /// the protected-location denylist, and the git-repository-root guard for the row under the
    /// cursor. Never bypasses the row being outside home, since `personal_file_selectability`
    /// itself never relaxes that check regardless of `force` (see `AGENTS.md`).
    fn toggle_selection_forced(&mut self) {
        self.toggle_selection_impl(true);
    }

    fn begin_delete_impl(&mut self, force: bool) {
        if !self.active_has_data() {
            self.set_status(
                "Removal is available once items appear",
                StatusSeverity::Warning,
            );
            return;
        }
        self.pending_delete = if self.selected_files.is_empty() {
            let focused = if force {
                self.focused_file_forced()
            } else {
                self.focused_file()
            };
            focused.ok().into_iter().collect()
        } else if force {
            // Force-delete escalates every already-selected entry, so Shift+D reliably removes
            // the whole batch even if some entries were only ever reachable through force-select.
            // This is harmless for an entry that was already selectable unforced: recomputing its
            // reason at confirmation time (see `draw_delete_confirmation`) finds nothing to
            // report as overridden.
            self.selected_files
                .iter()
                .map(|(path, entry)| {
                    (
                        path.clone(),
                        PendingRemoval {
                            force: true,
                            ..*entry
                        },
                    )
                })
                .collect()
        } else {
            self.selected_files.clone()
        };
        if self.pending_delete.is_empty() {
            self.set_status("Select a personal file first", StatusSeverity::Warning);
            return;
        }
        self.confirming_delete = true;
    }

    fn begin_delete(&mut self) {
        self.begin_delete_impl(false);
    }

    /// Force-delete (Shift+D): behaves like `begin_delete`, but targets rows the normal rules
    /// refuse. The confirmation dialog (`draw_delete_confirmation`) is still shown -- forcing
    /// never skips the last confirmation step.
    fn begin_delete_forced(&mut self) {
        self.begin_delete_impl(true);
    }

    fn cancel_delete(&mut self) {
        self.confirming_delete = false;
        self.pending_delete.clear();
        self.set_status("Removal cancelled", StatusSeverity::Info);
    }

    fn confirm_delete(&mut self) {
        let executor = Executor::new(self.home.clone());
        let results: Vec<_> = self
            .pending_delete
            .iter()
            .map(|(path, entry)| {
                let file = LargeFile {
                    path: path.clone(),
                    size: entry.size,
                    modified_unix: None,
                    app_data: false,
                };
                executor.execute(&file.cleanup_item(entry.force), false)
            })
            .collect();
        let failed = results.iter().filter(|result| !result.success).count();
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
        self.active_mut().start_scan();
        self.list_state = ListState::default();
        if failed > 0 {
            self.set_status(format!("{failed} removal(s) failed"), StatusSeverity::Error);
        } else {
            self.set_status("Scanning disk usage...", StatusSeverity::Info);
        }
    }

    fn update_selection_status(&mut self) {
        let total: u64 = self.selected_files.values().map(|entry| entry.size).sum();
        if self.selected_files.is_empty() {
            self.set_status(
                format!("Scanned {}", format_bytes(self.active().total_size)),
                StatusSeverity::Info,
            );
        } else {
            self.set_status(
                format!(
                    "{} selected, {}",
                    self.selected_files.len(),
                    format_bytes(total)
                ),
                StatusSeverity::Info,
            );
        }
    }

    fn toggle_mode(&mut self) {
        if !self.active_has_data() {
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
        let message = match self.mode {
            AnalyzeMode::Browse => "Directory explorer",
            AnalyzeMode::TopFiles => "Largest files in this location",
        };
        self.set_status(message, StatusSeverity::Info);
    }

    fn begin_filter(&mut self) {
        if self.active_has_data() {
            self.filtering = true;
            self.selected_files.clear();
            self.set_status(
                "Type to filter, Enter to apply, Esc to clear",
                StatusSeverity::Info,
            );
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
        if self.locations.len() > 1 {
            // Deliberately does not cancel the popped location's scan: the plan intends
            // ancestors to keep draining in the background (up to the depth cap) precisely so
            // Back can reveal further-along or complete data instead of forcing a re-scan. Only
            // depth-cap eviction (see `enforce_scan_depth_cap`) ever cancels a handle.
            self.locations.pop();
            self.selected_files.clear();
            self.list_state = ListState::default();
            self.active_mut().reorder();
            self.reconcile_list_state();
            self.set_status("Returned to previous location", StatusSeverity::Info);
            return false;
        }
        true
    }

    fn refresh(&mut self) {
        if self.active().scan.is_some() {
            self.set_status("A scan is already in progress", StatusSeverity::Warning);
            return;
        }
        self.filter.clear();
        self.filtering = false;
        self.selected_files.clear();
        self.active_mut().start_scan();
        self.list_state = ListState::default();
        self.set_status("Scanning disk usage...", StatusSeverity::Info);
    }

    fn render_items(&self) -> Vec<ListItem<'static>> {
        let total = self.active().total_size;
        match self.mode {
            AnalyzeMode::Browse => self
                .visible_entries()
                .into_iter()
                .map(|entry| {
                    let selected = self.selected_files.contains_key(&entry.path);
                    let selectability = personal_file_selectability(
                        &self.home,
                        &entry.path,
                        entry.is_dir,
                        entry.size,
                    );
                    disk_item(entry, total, selected, selectability)
                })
                .collect(),
            AnalyzeMode::TopFiles => self
                .visible_large_files()
                .into_iter()
                .map(|file| {
                    let selected = self.selected_files.contains_key(&file.path);
                    let selectability =
                        personal_file_selectability(&self.home, &file.path, false, file.size);
                    file_item(file, total, selected, selectability)
                })
                .collect(),
        }
    }
}

impl Drop for AnalyzeState {
    fn drop(&mut self) {
        for location in &mut self.locations {
            location.cancel_scan();
        }
    }
}

pub fn interactive_app() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    enable_raw_mode()?;
    // Requesting DISAMBIGUATE_ESCAPE_CODES is what lets a supporting terminal report Shift+Space
    // as `Char(' ')` with `KeyModifiers::SHIFT` instead of an identical byte sequence to plain
    // Space (see the Space handling in `handle_key`, below). A terminal that does not implement
    // the kitty keyboard protocol is specified to ignore an unrecognized private-mode escape
    // sequence, so this is safe to send unconditionally rather than probing for support first;
    // `TerminalGuard::drop` pops it again on every teardown path.
    execute!(
        stdout(),
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
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
    // Popped before leaving the alternate screen for the sudo prompt, matching
    // `TerminalGuard::drop`'s ordering, so the terminal is not left in the enhanced keyboard mode
    // while showing an ordinary password prompt outside TuxCleaner's own input handling.
    execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
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
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        Hide,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
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

    use crate::model::{CleanupGroup, CleanupItem, Risk};

    use super::*;

    fn ready_analyze(home: &Path, file: PathBuf) -> AnalyzeState {
        let size = fs::metadata(&file).unwrap().len();
        let mut location = Location::new(home.to_path_buf());
        let entry = DiskEntry {
            name: file.file_name().unwrap().to_string_lossy().into_owned(),
            path: file.clone(),
            size,
            is_dir: false,
        };
        location.entries.insert(file.clone(), entry.clone());
        location.sorted = vec![entry];
        location.large_files = vec![LargeFile {
            path: file.clone(),
            size,
            modified_unix: None,
            app_data: false,
        }];
        location.total_size = size;
        location.total_files = 1;
        location.complete = true;
        location.needs_reorder = false;
        location.selected = Some(file);
        AnalyzeState {
            home: home.to_path_buf(),
            locations: vec![location],
            mode: AnalyzeMode::Browse,
            list_state: ListState::default().with_selected(Some(0)),
            selected_files: BTreeMap::new(),
            pending_delete: BTreeMap::new(),
            confirming_delete: false,
            results: None,
            status: "Ready".into(),
            status_severity: StatusSeverity::Info,
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

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// Test-only convenience wrapper around `personal_file_selectability`, mirroring what used
    /// to be the standalone `is_selectable_personal_file` helper. Production code now goes
    /// through `personal_file_selectability` directly so it can render the specific refusal
    /// reason inline (see `append_refusal_tag`), rather than collapsing it to a bool first.
    fn selectable(home: &Path, path: &Path, is_dir: bool, size: u64) -> bool {
        personal_file_selectability(home, path, is_dir, size).is_ok()
    }

    /// The forced counterpart to `selectable`, wrapping `personal_file_selectability_forced`.
    fn selectable_forced(home: &Path, path: &Path, is_dir: bool, size: u64) -> bool {
        personal_file_selectability_forced(home, path, is_dir, size).is_ok()
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
    fn hidden_files_are_selectable_but_protected_locations_are_not() {
        // The owner of this fork has explicitly relaxed the hidden-data half of the "large
        // personal files and hidden application data are reported only" invariant: an ordinary
        // hidden directory (e.g. `.ollama`) is now selectable, while `.config` (and the other
        // specifically protected locations) remain refused regardless of hiddenness.
        let home = Path::new("/home/tester");
        assert!(!selectable(
            home,
            Path::new("/home/tester/.config/private.bin"),
            false,
            ANALYZE_MINIMUM_SIZE
        ));
        assert!(selectable(
            home,
            Path::new("/home/tester/.ollama/models/blobs/sha256-abc"),
            false,
            ANALYZE_MINIMUM_SIZE
        ));
        assert!(selectable(
            home,
            Path::new("/home/tester/Downloads/archive.iso"),
            false,
            ANALYZE_MINIMUM_SIZE
        ));
        assert!(!selectable(
            home,
            Path::new("/home/tester/Downloads/note.txt"),
            false,
            ANALYZE_MINIMUM_SIZE - 1
        ));
    }

    #[test]
    fn selectability_reasons_are_specific_to_the_refusal_cause() {
        let home = Path::new("/home/tester");
        // Directories are selectable in general now; a plain (non-git) directory at or above the
        // floor is accepted, exactly like a file. The nonexistent path is fine here because
        // `is_git_repository_root` only looks for a `.git` entry and a missing directory simply
        // has none.
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/home/tester/Downloads"),
                true,
                ANALYZE_MINIMUM_SIZE
            ),
            Ok(())
        );
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/home/tester/Downloads/note.txt"),
                false,
                ANALYZE_MINIMUM_SIZE - 1
            ),
            Err(PersonalFileRefusal::BelowMinimumSize)
        );
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/home/tester/.config/big.bin"),
                false,
                ANALYZE_MINIMUM_SIZE
            ),
            Err(PersonalFileRefusal::ProtectedLocation)
        );
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/home/tester/go/pkg/mod/archive.bin"),
                false,
                ANALYZE_MINIMUM_SIZE
            ),
            Err(PersonalFileRefusal::ProtectedLocation)
        );
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/somewhere/else/archive.iso"),
                false,
                ANALYZE_MINIMUM_SIZE
            ),
            Err(PersonalFileRefusal::OutsideHome)
        );
        assert_eq!(
            personal_file_selectability(
                home,
                Path::new("/home/tester/Downloads/archive.iso"),
                false,
                ANALYZE_MINIMUM_SIZE
            ),
            Ok(())
        );
    }

    /// A directory that is itself a git repository root (a `.git` entry lives directly inside
    /// it) must be refused with the specific `GitRepository` reason, at a size well above the
    /// selection floor, and both the pre-selection check and the execution-time re-check must
    /// agree. Unlike the other refusal cases above, this needs a real directory on disk because
    /// `is_git_repository_root` stats the path for a `.git` entry.
    #[test]
    fn git_repository_root_directories_are_refused_with_a_specific_reason() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let project = home.join("code/project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join("README.md"), b"hello").unwrap();

        // `size` is the value the scanner already computed for this directory and is passed in
        // independently of what is physically on disk here, so a tiny fixture file is enough to
        // exercise a "well above the floor" size without actually writing hundreds of megabytes.
        assert_eq!(
            personal_file_selectability(&home, &project, true, ANALYZE_MINIMUM_SIZE),
            Err(PersonalFileRefusal::GitRepository)
        );
        assert!(!selectable(&home, &project, true, ANALYZE_MINIMUM_SIZE));

        let executor = Executor::new(home);
        assert!(executor.validate_personal_file(&project).is_err());
        assert!(project.exists(), "the repository must not be removed");
        assert!(project.join(".git").exists());
    }

    /// Anti-drift check: `personal_file_selectability` (the TUI's pre-selection gate) and
    /// `Executor::validate_personal_file` (the execution-time re-check) must agree on every path
    /// the UI would offer, or a user could select and confirm removal of a file that execution
    /// then silently refuses. This exercises real files on disk, since `validate_personal_file`
    /// stats the path.
    #[test]
    fn selectable_paths_are_always_accepted_by_validate_personal_file() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let executor = Executor::new(home.clone());
        let big = ANALYZE_MINIMUM_SIZE;

        // (relative path, size, is_dir, expected_selectable)
        let cases: &[(&str, u64, bool, bool)] = &[
            ("Downloads/archive.iso", big, false, true),
            (".ollama/models/blobs/sha256-abc", big, false, true),
            (
                ".local/share/containers/storage/disk.qcow2",
                big,
                false,
                true,
            ),
            // A plain (non-git) directory is now selectable and removable, recursively, exactly
            // like a large file.
            ("Videos/recordings", big, true, true),
            ("go/pkg/mod/archive.bin", big, false, false),
            (".ssh/id_rsa_backup", big, false, false),
            (".gnupg/secring.gpg", big, false, false),
            (".config/app/state.bin", big, false, false),
            (".git/objects/pack/big.pack", big, false, false),
            ("code/project/.git/objects/pack/big.pack", big, false, false),
            // Denylisted locations must be refused as directories too, not just as files.
            (".ssh", big, true, false),
            (".gnupg", big, true, false),
            (".config", big, true, false),
            (".git", big, true, false),
            ("go/pkg", big, true, false),
        ];

        for (relative, size, is_dir, expected_selectable) in cases {
            let path = home.join(relative);
            if *is_dir {
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("payload.bin"), vec![0u8; 8]).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::File::create(&path).unwrap().set_len(*size).unwrap();
            }

            let is_row_selectable = selectable(&home, &path, *is_dir, *size);
            assert_eq!(
                is_row_selectable, *expected_selectable,
                "unexpected selectability for {relative}"
            );
            if is_row_selectable {
                assert!(
                    executor.validate_personal_file(&path).is_ok(),
                    "validate_personal_file disagreed with personal_file_selectability for {relative}"
                );
            } else {
                // Every denylisted or protected location must be rejected by both functions;
                // sub-threshold cases are a UI-only concern (see `PersonalFileRefusal`) and are
                // covered separately above, so this branch only ever hits denylisted or
                // git-repository-root paths here.
                assert!(
                    executor.validate_personal_file(&path).is_err(),
                    "protected location {relative} was unexpectedly accepted by validate_personal_file"
                );
            }
        }
    }

    /// Forced selection (Shift+Space / `S`) succeeds for a path the normal, unforced rules would
    /// refuse: below the size floor, a git-repository root, and each of the five denylist
    /// entries.
    #[test]
    fn forced_selection_accepts_paths_the_unforced_rules_refuse() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let big = ANALYZE_MINIMUM_SIZE;

        // Below the size floor.
        let small = home.join("Downloads/note.txt");
        fs::create_dir_all(small.parent().unwrap()).unwrap();
        fs::write(&small, b"x").unwrap();
        assert!(!selectable(&home, &small, false, big - 1));
        assert!(selectable_forced(&home, &small, false, big - 1));

        // A git-repository root.
        let project = home.join("code/project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join("README.md"), vec![0u8; 8]).unwrap();
        assert!(!selectable(&home, &project, true, big));
        assert!(selectable_forced(&home, &project, true, big));

        // Each denylist entry.
        for relative in [".ssh", ".gnupg", ".config", ".git", "go/pkg"] {
            let dir = home.join(relative);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data.bin"), vec![0u8; 8]).unwrap();
            assert!(
                !selectable(&home, &dir, true, big),
                "expected unforced selection to still refuse {relative}"
            );
            assert!(
                selectable_forced(&home, &dir, true, big),
                "expected forced selection to accept {relative}"
            );
        }
    }

    /// Forced validation in the executor accepts the same paths forced selection in the TUI
    /// offers: a git-repository root and each denylist entry (the size floor is a TUI-only
    /// concept; the executor has no size check to relax).
    #[test]
    fn forced_selection_agrees_with_forced_executor_validation() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let executor = Executor::new(home.clone());
        let big = ANALYZE_MINIMUM_SIZE;

        let project = home.join("code/project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join("README.md"), vec![0u8; 8]).unwrap();
        assert!(selectable_forced(&home, &project, true, big));
        assert!(executor.validate_personal_file_forced(&project).is_ok());

        for relative in [".ssh", ".gnupg", ".config", ".git", "go/pkg"] {
            let dir = home.join(relative);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data.bin"), vec![0u8; 8]).unwrap();
            assert!(selectable_forced(&home, &dir, true, big));
            assert!(
                executor.validate_personal_file_forced(&dir).is_ok(),
                "forced executor validation disagreed with forced selectability for {relative}"
            );
        }
    }

    /// Force must never override a path being outside home, even at the TUI's pre-selection
    /// layer. The remaining hard checks (home itself, `..`, symlink, symlinked ancestor) apply to
    /// paths that can only ever reach `Executor::validate_personal_file_forced` -- see
    /// `forced_validation_still_refuses_every_hard_check` in `src/executor.rs`, which covers each
    /// of those individually.
    #[test]
    fn forced_selection_still_refuses_a_path_outside_home() {
        let home = Path::new("/home/tester");
        assert_eq!(
            personal_file_selectability_forced(
                home,
                Path::new("/somewhere/else/archive.iso"),
                false,
                ANALYZE_MINIMUM_SIZE
            ),
            Err(PersonalFileRefusal::OutsideHome)
        );
    }

    /// A large directory under home is selectable and accepted by `validate_personal_file`, and
    /// removing it through the executor actually deletes it and everything inside it.
    #[test]
    fn large_directory_is_selectable_and_removed_recursively() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let dir = home.join("Videos/recordings");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("clip1.mp4"), vec![0u8; 8]).unwrap();
        fs::write(dir.join("nested/clip2.mp4"), vec![0u8; 8]).unwrap();

        assert!(selectable(&home, &dir, true, ANALYZE_MINIMUM_SIZE));

        let executor = Executor::new(home);
        assert!(executor.validate_personal_file(&dir).is_ok());

        let item = CleanupItem {
            id: "large-file:test".into(),
            group: CleanupGroup::User,
            label: "recordings".into(),
            estimated_bytes: 16,
            risk: Risk::Explicit,
            action: CleanupAction::RemovePersonalFile {
                path: dir.clone(),
                force: false,
            },
        };
        let result = executor.execute(&item, false);
        assert!(result.success, "{}", result.message);
        assert!(!dir.exists());
    }

    /// The most important regression test in this set: a symlink pointing at a large directory
    /// must be refused at execution time, and the real directory it points at (and everything
    /// inside it) must still exist afterwards. If `validate_personal_file` or `remove_entry` ever
    /// started following the symlink instead of statting/removing it directly, this is what would
    /// catch it.
    ///
    /// This does not also assert `personal_file_selectability` refuses the symlink: that
    /// pre-selection check does not re-stat for symlink-ness, because the scanner that feeds it
    /// already excludes symlinks from `DiskEntry`/`LargeFile` results (the "never follow symlinks
    /// while scanning" invariant), so a symlink is never something the UI would offer in the
    /// first place. `Executor::validate_personal_file`, exercised below, is the authoritative,
    /// execution-time re-check that must refuse it regardless.
    #[test]
    fn symlinked_directory_is_refused_and_its_target_survives() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let real_target = root.path().join("real-large-dir");
        fs::create_dir_all(&real_target).unwrap();
        fs::write(real_target.join("payload.bin"), vec![0u8; 8]).unwrap();
        let link = home.join("linked-large-dir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let executor = Executor::new(home);
        assert!(executor.validate_personal_file(&link).is_err());

        let item = CleanupItem {
            id: "large-file:test".into(),
            group: CleanupGroup::User,
            label: "linked-large-dir".into(),
            estimated_bytes: ANALYZE_MINIMUM_SIZE,
            risk: Risk::Explicit,
            action: CleanupAction::RemovePersonalFile {
                path: link.clone(),
                force: false,
            },
        };
        let result = executor.execute(&item, false);
        assert!(!result.success);
        assert!(real_target.exists(), "the symlink target must survive");
        assert!(real_target.join("payload.bin").exists());
    }

    /// The home directory itself, a path containing `..`, a symlinked directory, and a directory
    /// with a symlinked ancestor must all still be refused now that plain directories are
    /// selectable.
    #[test]
    fn unsafe_directory_targets_are_still_refused() {
        let root = tempdir().unwrap();
        let home = root.path().to_path_buf();
        let executor = Executor::new(home.clone());

        assert!(executor.validate_personal_file(&home).is_err());

        let escaping = home.join("Videos/../../etc");
        assert!(executor.validate_personal_file(&escaping).is_err());

        let real_dir = root.path().join("elsewhere-dir");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("f.bin"), vec![0u8; 8]).unwrap();
        let link = home.join("linked-dir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        assert!(executor.validate_personal_file(&link).is_err());
        assert!(real_dir.exists());

        let ancestor_target = root.path().join("ancestor-target");
        fs::create_dir_all(&ancestor_target).unwrap();
        let ancestor_link = home.join("ancestor-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ancestor_target, &ancestor_link).unwrap();
        let nested = ancestor_link.join("nested-dir");
        fs::create_dir_all(&nested).unwrap();
        assert!(executor.validate_personal_file(&nested).is_err());
        assert!(ancestor_target.exists());
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

    /// Every refusal reason that can still reach the renderer after directories became
    /// selectable must show its tag inline on the row, and a selectable row must show no tag at
    /// all. `ListItem`'s content field is crate-private in ratatui, so the row text is inspected
    /// through `{:?}`, which (per ratatui's own `Debug` impls for `Text`/`Line`/`Span`) always
    /// includes the literal rendered string.
    #[test]
    fn non_selectable_rows_carry_an_inline_refusal_tag_and_selectable_rows_do_not() {
        let entry = DiskEntry {
            name: "example".into(),
            path: PathBuf::from("/home/tester/example"),
            size: ANALYZE_MINIMUM_SIZE,
            is_dir: false,
        };
        let file = LargeFile {
            path: PathBuf::from("/home/tester/example.bin"),
            size: ANALYZE_MINIMUM_SIZE,
            modified_unix: None,
            app_data: false,
        };

        let cases: &[(PersonalFileRefusal, String)] = &[
            (PersonalFileRefusal::OutsideHome, "outside home".into()),
            (
                PersonalFileRefusal::ProtectedLocation,
                "protected location".into(),
            ),
            (PersonalFileRefusal::GitRepository, "git repository".into()),
            (
                PersonalFileRefusal::BelowMinimumSize,
                format!("below {}", format_bytes(ANALYZE_MINIMUM_SIZE)),
            ),
        ];

        // `ListItem`'s `Debug` output wraps the row text in framing like `Text::from(...)`, which
        // itself contains parentheses, so a bare `contains('(')` would false-positive on every
        // row. Checking for the exact `(tag)` annotation for each known tag avoids that.
        let selectable_disk_row = format!("{:?}", disk_item(&entry, entry.size, false, Ok(())));
        let selectable_file_row = format!("{:?}", file_item(&file, file.size, false, Ok(())));
        for (_, tag) in cases {
            let annotation = format!("({tag})");
            assert!(
                !selectable_disk_row.contains(&annotation),
                "a selectable directory row must not carry a refusal tag: {selectable_disk_row}"
            );
            assert!(
                !selectable_file_row.contains(&annotation),
                "a selectable file row must not carry a refusal tag: {selectable_file_row}"
            );
        }

        for (refusal, tag) in cases {
            let disk_row = format!("{:?}", disk_item(&entry, entry.size, false, Err(*refusal)));
            assert!(
                disk_row.contains(&format!("({tag})")),
                "expected disk row to contain tag {tag:?}: {disk_row}"
            );
            let file_row = format!("{:?}", file_item(&file, file.size, false, Err(*refusal)));
            assert!(
                file_row.contains(&format!("({tag})")),
                "expected file row to contain tag {tag:?}: {file_row}"
            );
        }
    }

    #[test]
    fn entering_analyze_renders_loading_immediately_and_escape_cancels_scan() {
        let root = tempdir().unwrap();
        let state = AnalyzeState::new(root.path().to_path_buf());
        let cancelled = Arc::clone(&state.locations[0].scan.as_ref().unwrap().cancel);
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
            force: false,
        };

        assert!(cleanup_action_requires_root(&direct));
        assert!(cleanup_action_requires_root(&sequence));
        assert!(!cleanup_action_requires_root(&personal));
    }

    fn entry(name: &str, size: u64) -> DiskEntry {
        DiskEntry {
            name: name.into(),
            path: PathBuf::from(format!("/root/{name}")),
            size,
            is_dir: true,
        }
    }

    #[test]
    fn reorder_keeps_cursor_on_the_same_path_after_a_size_change() {
        let mut entries = BTreeMap::new();
        entries.insert(PathBuf::from("/root/a"), entry("a", 10));
        entries.insert(PathBuf::from("/root/b"), entry("b", 5));
        let previous_sorted = vec![entry("a", 10), entry("b", 5)];

        // "b" was the smaller entry and thus ranked last, but it is still the selected path.
        let selected = Some(Path::new("/root/b"));
        let (sorted, resolved) = reorder_and_reconcile(&entries, &previous_sorted, selected);
        assert_eq!(sorted[1].path, PathBuf::from("/root/b"));
        assert_eq!(resolved, Some(PathBuf::from("/root/b")));

        // Now "b" grows past "a": the display order flips, but the resolved selection must stay
        // on "b" by identity rather than silently tracking whatever is now in the old slot.
        let mut grown = BTreeMap::new();
        grown.insert(PathBuf::from("/root/a"), entry("a", 10));
        grown.insert(PathBuf::from("/root/b"), entry("b", 50));
        let (sorted, resolved) = reorder_and_reconcile(&grown, &sorted, resolved.as_deref());
        assert_eq!(sorted[0].path, PathBuf::from("/root/b"));
        assert_eq!(resolved, Some(PathBuf::from("/root/b")));
    }

    #[test]
    fn enter_targets_the_selected_path_even_after_a_same_tick_reorder() {
        let mut entries = BTreeMap::new();
        entries.insert(PathBuf::from("/root/a"), entry("a", 1));
        entries.insert(PathBuf::from("/root/b"), entry("b", 100));
        entries.insert(PathBuf::from("/root/c"), entry("c", 2));
        let previous_sorted = vec![entry("b", 100), entry("c", 2), entry("a", 1)];

        // The cursor was resting on "c" right before a reorder lands in the same tick.
        let (sorted, resolved) =
            reorder_and_reconcile(&entries, &previous_sorted, Some(Path::new("/root/c")));

        // Regardless of where "c" ends up in the freshly sorted list, `enter_selected` must be
        // able to resolve the exact same directory that was under the cursor.
        assert_eq!(resolved, Some(PathBuf::from("/root/c")));
        assert!(sorted.iter().any(|item| item.path == Path::new("/root/c")));
    }

    /// Regression test for the `TopFiles` cursor bug: before this fix, `reconcile_list_state`
    /// returned early for `TopFiles` on the (false) assumption that background scanning never
    /// touches `large_files`. In fact `drain_location`'s `ScanUpdate::Large` arm pushes into it
    /// live, and `visible_large_files()` re-sorts by size descending on every call, so when a
    /// larger file streamed in after the user had placed the cursor on a smaller one, the row
    /// order shifted under the still-numeric `list_state` index and `d` would silently delete
    /// whatever new file now occupied that slot instead of the one the user was looking at.
    #[test]
    fn topfiles_cursor_survives_a_larger_file_arriving_after_it_is_placed() {
        let mut location = Location::new(PathBuf::from("/root"));
        location.large_files = vec![
            LargeFile {
                path: PathBuf::from("/root/a.bin"),
                size: ANALYZE_MINIMUM_SIZE + 10,
                modified_unix: None,
                app_data: false,
            },
            LargeFile {
                path: PathBuf::from("/root/b.bin"),
                size: ANALYZE_MINIMUM_SIZE + 5,
                modified_unix: None,
                app_data: false,
            },
        ];
        // Sorted desc by size: a at index 0, b at index 1. The user has moved the cursor onto
        // "b", the smaller of the two.
        location.selected_large_file = Some(PathBuf::from("/root/b.bin"));
        location.complete = true;
        location.needs_reorder = false;

        let mut state = AnalyzeState {
            home: PathBuf::from("/root"),
            locations: vec![location],
            mode: AnalyzeMode::TopFiles,
            list_state: ListState::default().with_selected(Some(1)),
            selected_files: BTreeMap::new(),
            pending_delete: BTreeMap::new(),
            confirming_delete: false,
            results: None,
            status: "Ready".into(),
            status_severity: StatusSeverity::Info,
            filter: String::new(),
            filtering: false,
            show_help: false,
            spinner: 0,
            history_store: None,
        };

        // A larger file streams in, exactly like `drain_location` pushing a `ScanUpdate::Large`
        // during a live scan. It now sorts first, pushing "b.bin" from index 1 to index 2.
        state.active_mut().large_files.push(LargeFile {
            path: PathBuf::from("/root/c.bin"),
            size: ANALYZE_MINIMUM_SIZE + 100,
            modified_unix: None,
            app_data: false,
        });

        state.reconcile_list_state();

        assert_eq!(
            state.active().selected_large_file.as_deref(),
            Some(Path::new("/root/b.bin")),
            "the identity-tracked selection must stay on the originally highlighted file"
        );
        assert_eq!(
            state.list_state.selected(),
            Some(2),
            "the numeric cursor must follow \"b.bin\" to its new position"
        );

        // `d` (via `focused_file`) must resolve to the same file that was highlighted before the
        // larger file arrived, not whatever now sits at the stale index.
        let (path, _size) = state.focused_file().expect("b.bin is selectable");
        assert_eq!(path, PathBuf::from("/root/b.bin"));
    }

    fn below_floor_file(root: &Path) -> PathBuf {
        let file = root.join("Downloads/small.bin");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, vec![0u8; 10]).unwrap();
        file
    }

    #[test]
    fn plain_d_still_refuses_a_row_the_size_floor_refuses() {
        // Unforced behavior is unchanged: normal `d` must keep refusing exactly what it refused
        // before this change.
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file.clone())));

        app.handle_key(key(KeyCode::Char('d')));

        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert!(!analyze.confirming_delete);
        assert!(file.exists());
    }

    #[test]
    fn shift_d_force_deletes_a_row_the_size_floor_refuses() {
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file.clone())));

        app.handle_key(key(KeyCode::Char('D')));
        {
            let Screen::Analyze(analyze) = &app.screen else {
                panic!("expected Analyze screen");
            };
            assert!(analyze.confirming_delete);
            assert!(!analyze.pending_delete.is_empty());
            assert!(analyze.pending_delete.values().all(|entry| entry.force));
        }
        assert!(
            file.exists(),
            "the confirmation dialog must still gate removal"
        );

        app.handle_key(key(KeyCode::Enter));
        assert!(!file.exists());
    }

    #[test]
    fn plain_space_selects_normally_without_a_modifier() {
        let root = tempdir().unwrap();
        let file = root.path().join("Downloads/large.bin");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::File::create(&file)
            .unwrap()
            .set_len(ANALYZE_MINIMUM_SIZE)
            .unwrap();
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file)));

        app.handle_key(key(KeyCode::Char(' ')));

        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert_eq!(analyze.selected_files.len(), 1);
        assert!(analyze.selected_files.values().all(|entry| !entry.force));
    }

    #[test]
    fn space_with_shift_modifier_force_selects_a_row_the_normal_rules_refuse() {
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file)));

        // Plain Space (no modifier) must still refuse it.
        app.handle_key(key(KeyCode::Char(' ')));
        {
            let Screen::Analyze(analyze) = &app.screen else {
                panic!("expected Analyze screen");
            };
            assert!(analyze.selected_files.is_empty());
        }

        // Space with the SHIFT modifier force-selects it.
        app.handle_key(shift_key(KeyCode::Char(' ')));
        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert_eq!(analyze.selected_files.len(), 1);
        assert!(analyze.selected_files.values().all(|entry| entry.force));
    }

    #[test]
    fn s_is_a_portable_alias_for_force_select() {
        // `S` must always work, unconditionally, regardless of what a terminal reports for
        // Shift+Space -- this is the fallback for terminals that cannot disambiguate the two.
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file)));

        app.handle_key(key(KeyCode::Char('S')));

        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert_eq!(analyze.selected_files.len(), 1);
        assert!(analyze.selected_files.values().all(|entry| entry.force));
    }

    #[test]
    fn toggle_selection_refusal_sets_warning_severity() {
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut app = App::new(root.path().to_path_buf());
        app.screen = Screen::Analyze(Box::new(ready_analyze(root.path(), file)));

        app.handle_key(key(KeyCode::Char(' ')));

        let Screen::Analyze(analyze) = &app.screen else {
            panic!("expected Analyze screen");
        };
        assert_eq!(analyze.status_severity, StatusSeverity::Warning);
    }

    #[test]
    fn confirm_delete_execution_failure_sets_error_severity() {
        let root = tempdir().unwrap();
        let file = root.path().join("Downloads/large.bin");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::File::create(&file)
            .unwrap()
            .set_len(ANALYZE_MINIMUM_SIZE)
            .unwrap();
        let mut state = ready_analyze(root.path(), file);

        // Point `pending_delete` directly at a denylisted, unforced path, bypassing selection, so
        // the executor refuses it and reports a genuine failure.
        let protected = root.path().join(".config/state.bin");
        fs::create_dir_all(protected.parent().unwrap()).unwrap();
        fs::write(&protected, vec![0u8; 8]).unwrap();
        state.pending_delete.insert(
            protected,
            PendingRemoval {
                size: 8,
                is_dir: false,
                force: false,
            },
        );

        state.confirm_delete();

        assert_eq!(state.status_severity, StatusSeverity::Error);
        assert!(
            state
                .results
                .as_ref()
                .unwrap()
                .iter()
                .any(|result| !result.success)
        );
    }

    #[test]
    fn forced_confirmation_dialog_names_the_overridden_guard() {
        let root = tempdir().unwrap();
        let file = below_floor_file(root.path());
        let mut state = ready_analyze(root.path(), file.clone());
        state.pending_delete.insert(
            file,
            PendingRemoval {
                size: 10,
                is_dir: false,
                force: true,
            },
        );

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_delete_confirmation(frame, &state))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("FORCE removal"));
        assert!(rendered.contains("overriding:"));
        assert!(rendered.contains(&format!("below {}", format_bytes(ANALYZE_MINIMUM_SIZE))));
    }
}
