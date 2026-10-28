//! TUI state and keyboard handling, independent of the terminal so it can be tested.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use diskdrift_app::config::{self, Config, Effective, FIELDS, Field};
use diskdrift_app::core::diff::DiffSort;
use diskdrift_app::core::top::SizeMetric;
use diskdrift_app::core::walk::Progress;
use diskdrift_app::diagnostics::doctor::{DoctorReport, DoctorStep};
use diskdrift_app::diagnostics::usage::UsageReport;
use diskdrift_app::analyze::{AnalyzeStep, DirAnalysis};
use diskdrift_app::core::units::{SizeUnits, format_size};
use diskdrift_app::diagnostics::suspects::MeasureStatus;
use diskdrift_app::investigate::RecoverReport;
use diskdrift_app::core::diff::DiffItem;
use diskdrift_app::snapshots::{self, AutoScan, BrowseResult, DiffBrowseResult, DiffResult, ScanOutcome, SnapshotList, TopResult, comparable, latest_pair};
use diskdrift_app::disks::{self, ConnectAction, DiskChoice, DiskList, Edit, Forget, Volume};
use diskdrift_app::storage::{SnapshotMeta, Status};
use diskdrift_app::prune::{Proposal, Removed};
use diskdrift_app::search::{Current, Hit, History, Lifetime, Query};
use diskdrift_app::storage::Compaction;
use diskdrift_app::careful::{CarefulOutcome, Held};
use diskdrift_app::core::record::Kind;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Top-level screens (spec 3A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Scan,
    Snapshots,
    Diff,
    Top,
    Doctor,
    Recover,
    Usage,
    Help,
    /// One directory level of a snapshot or comparison (opened from history,
    /// a finished scan, the largest-files list or a comparison).
    Browse,
    /// Live analysis of a folder (opened from Recover or Browse).
    Analyze,
    Settings,
    /// Every disk the store knows (spec 3A, item 11).
    Disks,
    /// File search over the saved scans of every disk (spec 3A, item 12).
    Find,
    /// Hold back external disks, scan them read-only, eject (spec 3C).
    Careful,
}

impl Screen {
    pub fn title(self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Scan => "Scan disk",
            Screen::Snapshots => "Snapshot history",
            Screen::Diff => "Compare snapshots",
            Screen::Top => "Largest files",
            Screen::Doctor => "Investigate / Doctor",
            Screen::Recover => "Recovery suggestions",
            Screen::Usage => "Volume usage",
            Screen::Help => "Help",
            Screen::Browse => "Browse",
            Screen::Analyze => "Folder analysis",
            Screen::Settings => "Settings",
            Screen::Disks => "My disks",
            Screen::Find => "Find a file",
            Screen::Careful => "Careful mode",
        }
    }

    pub fn hotkey(self) -> char {
        match self {
            Screen::Dashboard => ' ',
            Screen::Scan => 'S',
            Screen::Snapshots => 'H',
            Screen::Diff => 'D',
            Screen::Top => 'T',
            Screen::Doctor => 'I',
            Screen::Recover => 'R',
            Screen::Usage => 'U',
            Screen::Help => '?',
            Screen::Settings => 'E',
            Screen::Disks => 'K',
            Screen::Find => 'F',
            Screen::Careful => 'C',
            Screen::Browse | Screen::Analyze => ' ',
        }
    }

    fn from_hotkey(c: char) -> Option<Screen> {
        MENU.iter().copied().find(|s| s.hotkey().eq_ignore_ascii_case(&c))
    }
}

/// Main menu, laid out in two columns, row by row.
pub const MENU: [Screen; 12] = [
    Screen::Scan,
    Screen::Snapshots,
    Screen::Disks,
    Screen::Find,
    Screen::Diff,
    Screen::Top,
    Screen::Doctor,
    Screen::Recover,
    Screen::Usage,
    Screen::Settings,
    Screen::Careful,
    Screen::Help,
];

/// State of a background job.
#[derive(Debug, Default)]
pub enum Job<T> {
    #[default]
    Idle,
    Running(String),
    Done(Box<T>),
    Failed(String),
}

impl<T> Job<T> {
    pub fn is_running(&self) -> bool {
        matches!(self, Job::Running(_))
    }

    pub fn done(&self) -> Option<&T> {
        match self {
            Job::Done(v) => Some(v),
            _ => None,
        }
    }

    fn from(r: Result<T, String>) -> Job<T> {
        match r {
            Ok(v) => Job::Done(Box::new(v)),
            Err(e) => Job::Failed(e),
        }
    }
}

/// Work the event loop must start in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    LoadUsage,
    LoadSnapshots,
    LoadDisks,
    /// A volume appeared: decide whether to rescan it.
    DiskConnected(Volume),
    /// `live`: measure every location now instead of using a recent scan.
    RunDoctor { live: bool },
    /// `background`: the automatic scan at start (low priority).
    /// `disk`: the answer to "is this the known disk?", when it was asked.
    StartScan { root: String, name: Option<String>, background: bool, disk: Option<DiskChoice> },
    CancelScan,
    RunDiff { old: u64, new: u64, sort: DiffSort, min_size: u64 },
    RunTop { snapshot: u64, metric: SizeMetric },
    /// `live`: measure now even if a recent scan has the folder.
    RunAnalyze { id: u64, path: PathBuf, live: bool },
    /// One level of a snapshot or comparison; `path` `None` is the scan root.
    RunBrowse { id: u64, source: BrowseSource, path: Option<PathBuf> },
    /// Search the newest scans for `pattern` (run `id`), on one disk when `disk` is set.
    RunFind { id: u64, pattern: String, disk: Option<String>, set: Option<String> },
    /// Search the history of the last search; `always`: also when it found something now.
    RunFindHistory { id: u64, always: bool },
    CancelFind,
    /// Careful mode on (the guard starts) or off (it stops).
    Careful(bool),
    /// Mount the held volume `bsd` read-only, scan it with one thread, eject it.
    CarefulScan { bsd: String, choice: Option<DiskChoice> },
    /// Mount it read-only and visible in Finder.
    CarefulMount { bsd: String },
    CarefulEject { bsd: String },
    CancelCareful,
    /// Remove these snapshots (the user confirmed what removing them does).
    RemoveSnapshots { ids: Vec<u64> },
    /// Compact the store.
    Compact,
    /// Stop the removal (before anything is removed) or the compaction.
    CancelStoreJob,
    /// Skip the location currently being measured.
    SkipDoctorStep,
    CancelAnalyze,
    CancelJobs,
    Quit,
}

/// Results delivered from background jobs.
pub enum Msg {
    Usage(Box<UsageReport>),
    Snapshots(Box<Result<SnapshotList, String>>),
    DoctorStep(DoctorStep),
    Doctor(Box<DoctorReport>),
    ScanDone(Box<Result<ScanOutcome, String>>),
    /// The scan did not start: its disk matches a known one only doubtfully.
    ScanDoubt(String),
    Disks(Box<Result<DiskList, String>>),
    /// The volumes that count as disks, sent when the mounts change (the first
    /// one is the baseline at start).
    Volumes(Vec<Volume>),
    Connected { volume: Volume, action: Box<Result<ConnectAction, String>> },
    Diff(Box<Result<DiffResult, String>>),
    Top(Box<Result<TopResult, String>>),
    /// Suggestions derived from the diagnosis just finished.
    Recover(Box<Result<RecoverReport, String>>),
    AnalyzeStep { id: u64, step: AnalyzeStep },
    Analyze { id: u64, result: Box<Result<DirAnalysis, String>> },
    Browse { id: u64, result: Box<Result<BrowseData, String>> },
    FindCurrent { id: u64, result: Box<Result<Current, String>> },
    /// Pairs of scans read so far by the history search.
    FindProgress { id: u64, done: usize, total: usize },
    FindHistory { id: u64, result: Box<Result<History, String>> },
    /// Careful mode started (or could not).
    CarefulStarted(Result<(), String>),
    /// A volume was held back (with what the store knows about it).
    CarefulHeld(Box<Result<Held, String>>),
    CarefulGone(String),
    CarefulDone { bsd: String, result: Box<Result<CarefulOutcome, String>> },
    /// Mounted for Finder (`Ok(where)`) or ejected (`Ok(None)`).
    CarefulAction { bsd: String, result: Result<Option<PathBuf>, String> },
    /// What the removal or compaction is doing.
    StoreProgress(String),
    Removed(Box<Result<Removed, String>>),
    Compacted(Box<Result<Compaction, String>>),
}

fn scans_text(n: usize) -> String {
    if n == 1 { "1 scan".into() } else { format!("{n} scans") }
}

/// One timestamped line of a background job's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at: String,
    pub text: String,
}

impl LogLine {
    pub fn now(text: String) -> LogLine {
        LogLine { at: chrono::Local::now().format("%H:%M:%S").to_string(), text }
    }
}

/// Log text for a doctor/recover progress step.
pub fn step_text(step: &DoctorStep, u: SizeUnits) -> String {
    match step {
        DoctorStep::Volumes => "reading volumes and APFS containers".into(),
        DoctorStep::Snapshots => "listing local snapshots, checking Full Disk Access".into(),
        DoctorStep::Measuring { label, index, total } => format!("measuring {label} ({}/{total})", index + 1),
        DoctorStep::Measured { label, path, status, allocated_bytes, entries, elapsed } => {
            let size = match status {
                MeasureStatus::Absent => "not present".to_string(),
                MeasureStatus::Inaccessible => "not readable".to_string(),
                MeasureStatus::Complete => format_size(*allocated_bytes, u),
                MeasureStatus::Partial | MeasureStatus::TimedOut => format!("≥ {}", format_size(*allocated_bytes, u)),
                MeasureStatus::Skipped => format!("SKIPPED after {}", format_size(*allocated_bytes, u)),
            };
            format!("  {label}: {size}, {entries} entries, {:.1}s — {}", elapsed.as_secs_f64(), path.display())
        }
        DoctorStep::OpenFiles => "checking deleted-but-open files (lsof)".into(),
        DoctorStep::FromScan { source, locations } => {
            format!("using sizes of {locations} locations from {source}; F5 measures them all now instead")
        }
    }
}

/// The location a step starts measuring (what `n` would skip).
fn current_location(step: &DoctorStep) -> Option<String> {
    match step {
        DoctorStep::Measuring { label, .. } => Some(label.clone()),
        _ => None,
    }
}

/// One level of folder analysis.
pub struct AnalysisView {
    /// Distinguishes runs, so results of a cancelled one are ignored.
    pub id: u64,
    pub path: PathBuf,
    pub result: Job<DirAnalysis>,
    pub sel: usize,
    pub log: Vec<LogLine>,
    pub progress: Arc<Progress>,
    pub started: Instant,
    /// Why the last key did nothing; the next key clears it.
    pub note: Option<String>,
}

impl AnalysisView {
    fn new(id: u64, path: PathBuf) -> AnalysisView {
        AnalysisView {
            id,
            log: vec![LogLine::now(format!("analysing {}", path.display()))],
            path,
            result: Job::Running("measuring…".into()),
            sel: 0,
            progress: Arc::default(),
            started: Instant::now(),
            note: None,
        }
    }
}

pub struct RecoverUi {
    pub sel: usize,
}

/// The config file being edited on the Settings screen.
pub struct SettingsUi {
    pub sel: usize,
    /// The file's contents (defaults when there is none yet).
    pub file: Result<Config, String>,
    /// Text being typed for a text or number field.
    pub edit: Option<String>,
    /// Outcome of the last save: Ok(message) or Err(reason).
    pub message: Option<Result<String, String>>,
}

/// Folder analyses opened from Recover or Browse.
pub struct AnalysisUi {
    /// Outermost first; the last one is shown.
    pub stack: Vec<AnalysisView>,
    next_id: u64,
    /// Where Esc on the outermost analysis goes.
    pub back: Screen,
}

/// What a browser walks through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseSource {
    Snapshot(u64),
    /// Changes from the older to the newer snapshot.
    Diff { old: u64, new: u64 },
}

pub enum BrowseData {
    Snapshot(Box<BrowseResult>),
    Diff(Box<DiffBrowseResult>),
}

impl BrowseData {
    pub fn len(&self) -> usize {
        match self {
            BrowseData::Snapshot(r) => r.listing.items.len(),
            BrowseData::Diff(r) => r.listing.items.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Path, name and whether it is a directory, of the child at `i`.
    fn child(&self, i: usize) -> Option<(PathBuf, String, bool)> {
        let (path, name, dir) = match self {
            BrowseData::Snapshot(r) => r.listing.items.get(i).map(|c| (c.path.0.clone(), c.name(), c.is_dir()))?,
            BrowseData::Diff(r) => r.listing.items.get(i).map(|c| (c.path.0.clone(), c.name(), c.is_dir()))?,
        };
        Some((native(&path), name, dir))
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(match self {
            BrowseData::Snapshot(r) => &r.snapshot.root,
            BrowseData::Diff(r) => &r.new.root,
        })
    }
}

fn native(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Opening `path` from a list: a directory is opened, a file shows its parent
/// with the file selected.
fn open_target(path: &[u8], is_dir: bool) -> (PathBuf, Option<String>) {
    let p = native(path);
    match (is_dir, p.parent(), p.file_name()) {
        (false, Some(parent), Some(name)) => (parent.to_path_buf(), Some(name.to_string_lossy().into_owned())),
        _ => (p, None),
    }
}

/// One directory level in a browser.
pub struct BrowseLevel {
    pub id: u64,
    /// `None`: the scan root.
    pub path: Option<PathBuf>,
    pub result: Job<BrowseData>,
    pub sel: usize,
    /// Child to put the cursor on once the level is loaded.
    select: Option<String>,
    pub started: Instant,
}

pub struct BrowseUi {
    pub source: BrowseSource,
    /// Levels opened so far, outermost first; the last one is shown.
    pub stack: Vec<BrowseLevel>,
    /// Where Esc at the scan root goes.
    pub back: Screen,
    root: Option<PathBuf>,
    /// The level shown before going up past the opening level; it stays on
    /// screen (dimmed) until its parent is loaded.
    previous: Option<BrowseLevel>,
    /// Why the last key did nothing ("a file: nothing to open"); the next key clears it.
    pub note: Option<String>,
}

impl BrowseUi {
    /// Scan root of the snapshot(s) browsed, once known.
    pub fn root(&self) -> Option<&PathBuf> {
        self.root.as_ref()
    }

    /// What to keep on screen while the level shown is loading: the level it
    /// was opened from, so entering a folder does not blank the screen.
    pub fn backdrop(&self) -> Option<&BrowseLevel> {
        let n = self.stack.len();
        if !self.stack.last()?.result.is_running() {
            return None;
        }
        let parent = n.checked_sub(2).map(|i| &self.stack[i]);
        parent.or(self.previous.as_ref()).filter(|l| l.result.done().is_some())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanField {
    Root,
    Name,
}

pub enum ScanStage {
    /// `error`: why the typed root cannot be scanned, shown under it.
    Setup { root: String, name: String, field: ScanField, error: Option<String> },
    Confirm { root: String, name: String },
    /// The disk looks like a copy of a known one, or is recognised only
    /// weakly: is it that disk? (asked every time, ADR 0002).
    AskDisk { root: String, name: String, message: String },
    Running {
        progress: Arc<Progress>,
        started: Instant,
        root: String,
        /// The automatic background scan (stops with `x`, quitting does not ask).
        auto: bool,
        /// Entries of the previous scan of the root, for a progress estimate.
        expected: Option<u64>,
    },
    Finished(Result<Box<ScanOutcome>, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFocus {
    /// Picking the earlier scan from the list (step 1 of 2).
    PickOld,
    /// Picking the later scan (step 2 of 2).
    PickNew,
    /// The "comparing #a → #b" line above the results; Enter changes the scans.
    Header,
    Results,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffTab {
    GrowingDirs,
    Files,
    Shrinking,
    Summary,
}

pub const DIFF_TABS: [DiffTab; 4] = [DiffTab::Summary, DiffTab::GrowingDirs, DiffTab::Files, DiffTab::Shrinking];
/// Most items each Compare list holds.
pub const DIFF_LIMIT: usize = 200;
pub const MIN_SIZES: [u64; 4] = [0, 1_000_000, 100_000_000, 1_000_000_000];
pub const SORTS: [DiffSort; 5] = [DiffSort::Growth, DiffSort::Size, DiffSort::Modified, DiffSort::Created, DiffSort::Path];

pub struct DiffUi {
    /// Indices into the usable snapshots (oldest first).
    pub old: usize,
    pub new: usize,
    pub focus: DiffFocus,
    /// Row under the cursor while picking.
    pub cursor: usize,
    /// The user picked the scans; otherwise Compare follows the newest pair.
    pub chosen: bool,
    pub tab: DiffTab,
    pub sort: DiffSort,
    pub min_size: u64,
    pub result: Job<DiffResult>,
    /// Cursor in the list tabs.
    pub sel: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopView {
    Dirs,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopFocus {
    List,
    /// The "largest in #4 …" line; Enter picks another scan.
    Header,
    /// Picking the scan from a list.
    Pick,
}

pub struct TopUi {
    /// Index into the usable snapshots list.
    pub snapshot: usize,
    pub focus: TopFocus,
    /// Row under the cursor while picking.
    pub cursor: usize,
    /// The user picked the scan; otherwise the newest one is shown.
    pub chosen: bool,
    pub view: TopView,
    pub metric: SizeMetric,
    pub result: Job<TopResult>,
    pub sel: usize,
}

/// A text being typed on My disks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskField {
    Label,
    Describe,
    AddToSet,
    RemoveFromSet,
}

impl DiskField {
    pub fn prompt(self) -> &'static str {
        match self {
            DiskField::Label => "Label (empty removes it)",
            DiskField::Describe => "Description (empty removes it)",
            DiskField::AddToSet => "Add to set",
            DiskField::RemoveFromSet => "Remove from set",
        }
    }
}

pub struct DisksUi {
    pub list: Job<DiskList>,
    pub sel: usize,
    pub edit: Option<(DiskField, String)>,
    /// Asking whether to forget a disk (what it would remove).
    pub forget: Option<Forget>,
    /// The result of the last action, or why it did nothing.
    pub note: Option<Result<String, String>>,
}

/// The Find screen.
pub struct FindUi {
    pub input: String,
    /// The cursor is in the search field (else on the results).
    pub editing: bool,
    /// The query of the results shown.
    pub query: Option<Query>,
    pub current: Job<Current>,
    pub history: Job<History>,
    /// Pairs of scans read by the running history search, of all.
    pub progress: Option<(usize, usize)>,
    /// Distinguishes runs, so results of an older search are ignored.
    pub id: u64,
    /// Cursor over the current matches, then the history entries.
    pub sel: usize,
    /// Why the last key did nothing; the next key clears it.
    pub note: Option<String>,
}

/// Careful mode (spec 3C).
#[derive(Default)]
pub struct CarefulUi {
    /// On: the guard runs (`Some(false)` while it starts).
    pub on: Option<bool>,
    /// Volumes held back, in the order they came.
    pub held: Vec<Held>,
    pub sel: usize,
    /// The careful scan running: volume, progress, start.
    pub running: Option<(String, Arc<Progress>, Instant)>,
    /// Asking whether the doubtful volume `bsd` is the known disk.
    pub ask: Option<String>,
    pub log: Vec<LogLine>,
}

impl CarefulUi {
    pub fn is_on(&self) -> bool {
        self.on.is_some()
    }

    fn log(&mut self, text: String) {
        self.log.push(LogLine::now(text));
    }
}

/// A row of the Find results.
pub enum FindRow<'a> {
    Now(&'a Hit),
    Gone(&'a Lifetime),
}

/// Results the TUI keeps per list (all are counted).
pub const FIND_LIMIT: usize = 500;

/// History (and Compare, Largest files, the scan form, Find) shows one disk,
/// or the disks of one set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFilter {
    /// The disks shown (one, unless `set`).
    pub ids: Vec<String>,
    pub name: String,
    pub set: Option<String>,
}

impl DiskFilter {
    pub fn disk(d: &diskdrift_app::storage::Disk) -> DiskFilter {
        DiskFilter { ids: vec![d.id.clone()], name: d.display_name().to_string(), set: None }
    }

    pub fn shows(&self, disk: Option<&str>) -> bool {
        disk.is_some_and(|d| self.ids.iter().any(|i| i == d))
    }

    /// "disk backupy-2014", "set stare-dyski".
    pub fn label(&self) -> String {
        match &self.set {
            Some(s) => format!("set {s}"),
            None => format!("disk {}", self.name),
        }
    }
}

pub struct App {
    pub settings: Effective,
    pub screen: Screen,
    pub menu_index: usize,
    pub usage: Job<UsageReport>,
    pub doctor: Job<DoctorReport>,
    pub recover: Job<RecoverReport>,
    pub recover_ui: RecoverUi,
    pub analysis: AnalysisUi,
    pub settings_ui: SettingsUi,
    /// What the automatic background scan did, for the dashboard.
    pub auto_status: Option<String>,
    /// When the volume figures were read ("19:40").
    pub usage_at: Option<String>,
    /// The root of the last scan you started: a new scan offers it again.
    last_root: Option<String>,
    auto_checked: bool,
    /// Log of the measurement Doctor and Recover share.
    pub doctor_log: Vec<LogLine>,
    pub doctor_current: Option<String>,
    pub doctor_started: Instant,
    pub snapshots: Job<SnapshotList>,
    pub disks: DisksUi,
    pub find: FindUi,
    pub careful: CarefulUi,
    pub disk_filter: Option<DiskFilter>,
    /// Root and name of the scan last started, to start it again with an answer.
    scan_request: Option<(String, String)>,
    /// The volumes mounted now that count as disks (`None` until the first look).
    pub volumes: Option<Vec<Volume>>,
    /// The latest disk event ("backupy-2014 connected…"), for the dashboard.
    pub disk_event: Option<LogLine>,
    disk_event_at: Option<Instant>,
    /// Rescans of connected disks waiting for the running scan: (disk name, root).
    pending_connect: std::collections::VecDeque<(String, String)>,
    /// The background scan running is the rescan of this connected disk.
    connect_scan: Option<String>,
    pub snap_sel: usize,
    /// Why the last action in History did nothing (cleared when the cursor moves).
    pub snap_note: Option<String>,
    /// Scans marked in History for removal.
    pub snap_marks: std::collections::BTreeSet<u64>,
    /// Asking whether to remove these scans (what removing them does).
    pub snap_remove: Option<Proposal>,
    /// A removal or compaction is running: what it is doing.
    pub store_job: Option<String>,
    pub scan: ScanStage,
    pub diff: DiffUi,
    pub top: TopUi,
    pub browse: Option<BrowseUi>,
    next_browse_id: u64,
    /// Vertical scroll of the current text/list area.
    pub scroll: u16,
    /// Asking "a scan is running: cancel it and quit?".
    pub confirm_quit: bool,
    /// Asking "cancel the running scan?".
    pub confirm_cancel: bool,
    /// Asking "quit DiskDrift?" after Esc on the dashboard (Enter quits).
    pub confirm_exit: bool,
    pub should_quit: bool,
}

fn initial_scan(settings: &Effective) -> ScanStage {
    ScanStage::Setup {
        root: settings.default_root.value.display().to_string(),
        name: String::new(),
        field: ScanField::Root,
        error: None,
    }
}

impl App {
    pub fn new(settings: Effective) -> App {
        App {
            scan: initial_scan(&settings),
            settings,
            screen: Screen::Dashboard,
            menu_index: 0,
            usage: Job::Idle,
            doctor: Job::Idle,
            recover: Job::Idle,
            recover_ui: RecoverUi { sel: 0 },
            analysis: AnalysisUi { stack: Vec::new(), next_id: 1, back: Screen::Recover },
            auto_status: None,
            usage_at: None,
            last_root: None,
            auto_checked: false,
            settings_ui: SettingsUi { sel: 0, file: Ok(Config::default()), edit: None, message: None },
            doctor_log: Vec::new(),
            doctor_current: None,
            doctor_started: Instant::now(),
            snapshots: Job::Idle,
            disks: DisksUi { list: Job::Idle, sel: 0, edit: None, forget: None, note: None },
            careful: CarefulUi::default(),
            find: FindUi { input: String::new(), editing: true, query: None, current: Job::Idle, history: Job::Idle, progress: None, id: 0, sel: 0, note: None },
            disk_filter: None,
            scan_request: None,
            volumes: None,
            disk_event: None,
            disk_event_at: None,
            pending_connect: Default::default(),
            connect_scan: None,
            snap_sel: 0,
            snap_marks: Default::default(),
            snap_remove: None,
            store_job: None,
            snap_note: None,
            diff: DiffUi {
                old: 0,
                new: 0,
                focus: DiffFocus::Results,
                cursor: 0,
                chosen: false,
                tab: DiffTab::Summary,
                sort: DiffSort::Growth,
                min_size: 0,
                result: Job::Idle,
                sel: 0,
            },
            top: TopUi { snapshot: 0, focus: TopFocus::List, cursor: 0, chosen: false, view: TopView::Files, metric: SizeMetric::Allocated, result: Job::Idle, sel: 0 },
            browse: None,
            next_browse_id: 1,
            scroll: 0,
            confirm_quit: false,
            confirm_cancel: false,
            confirm_exit: false,
            should_quit: false,
        }
    }

    /// Commands to run at startup.
    pub fn startup(&mut self) -> Vec<Command> {
        self.usage = Job::Running("reading volumes…".into());
        self.snapshots = Job::Running("loading snapshots…".into());
        self.disks.list = Job::Running("reading disks…".into());
        vec![Command::LoadUsage, Command::LoadSnapshots, Command::LoadDisks]
    }

    pub fn scan_running(&self) -> bool {
        matches!(self.scan, ScanStage::Running { .. })
    }

    /// A scan the user started is running (quitting asks first).
    fn busy(&self) -> bool {
        matches!(self.scan, ScanStage::Running { auto: false, .. })
    }

    pub fn auto_scan_running(&self) -> bool {
        matches!(self.scan, ScanStage::Running { auto: true, .. })
    }

    /// The snapshots History lists, oldest first: all, or the filtered disk's.
    pub fn history(&self) -> Vec<&diskdrift_app::snapshots::SnapshotRow> {
        match &self.snapshots {
            Job::Done(l) => l
                .snapshots
                .iter()
                .filter(|r| self.disk_filter.as_ref().is_none_or(|f| f.shows(r.meta.disk.as_deref())))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Snapshots with data (COMPLETED or PARTIAL), oldest first, of the
    /// filtered disk when History is filtered.
    pub fn usable(&self) -> Vec<&diskdrift_app::snapshots::SnapshotRow> {
        self.history().into_iter().filter(|r| r.meta.status.is_usable()).collect()
    }

    pub fn text_input_active(&self) -> bool {
        (self.screen == Screen::Scan && matches!(self.scan, ScanStage::Setup { .. }))
            || (self.screen == Screen::Settings && self.settings_ui.edit.is_some())
            || (self.screen == Screen::Disks && self.disks.edit.is_some())
            || (self.screen == Screen::Find && self.find.editing)
    }

    pub fn open(&mut self, screen: Screen) -> Vec<Command> {
        self.screen = screen;
        self.scroll = 0;
        if let Some(i) = MENU.iter().position(|s| *s == screen) {
            self.menu_index = i;
        }
        match screen {
            Screen::Doctor if matches!(self.doctor, Job::Idle | Job::Failed(_)) => self.run_doctor(false),
            Screen::Recover if matches!(self.recover, Job::Idle | Job::Failed(_)) && !self.doctor.is_running() => self.run_doctor(false),
            Screen::Usage if matches!(self.usage, Job::Idle | Job::Failed(_)) => {
                self.usage = Job::Running("reading volumes…".into());
                vec![Command::LoadUsage]
            }
            Screen::Settings => {
                let ui = &mut self.settings_ui;
                ui.file = config::load_file(&self.settings.config_path.value).map_err(|e| e.to_string());
                ui.edit = None;
                ui.message = None;
                Vec::new()
            }
            Screen::Snapshots if !self.snapshots.is_running() => {
                self.snapshots = Job::Running("loading snapshots…".into());
                vec![Command::LoadSnapshots]
            }
            Screen::Diff => self.diff_latest(),
            Screen::Top => self.top_latest(),
            Screen::Disks => {
                self.disks.forget = None;
                self.disks.note = None;
                self.load_disks()
            }
            Screen::Find => {
                self.find.editing = true;
                self.find.note = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn load_disks(&mut self) -> Vec<Command> {
        if self.disks.list.is_running() {
            return Vec::new();
        }
        self.disks.list = Job::Running("reading disks…".into());
        vec![Command::LoadDisks]
    }

    /// One measurement feeds both Doctor (the diagnosis) and Recover (the
    /// suggestions derived from it).
    fn run_doctor(&mut self, live: bool) -> Vec<Command> {
        self.doctor = Job::Running("starting…".into());
        self.recover = Job::Running("measuring candidate locations…".into());
        self.recover_ui.sel = 0;
        self.doctor_log = vec![LogLine::now("measuring (shared by Doctor and Recovery suggestions)".into())];
        self.doctor_started = Instant::now();
        vec![Command::RunDoctor { live }]
    }

    /// `n` while measuring: skip the location being measured.
    fn skip_doctor_step(&mut self) -> Vec<Command> {
        match self.doctor_current.take() {
            Some(label) => {
                self.doctor_log.push(LogLine::now(format!("skipping {label} (n pressed)")));
                vec![Command::SkipDoctorStep]
            }
            None => Vec::new(),
        }
    }

    /// Opens an analysis of `path`: on top of the analyses shown, or as the
    /// first one, returning to the current screen on Esc.
    fn push_analysis(&mut self, path: PathBuf) -> Vec<Command> {
        self.push_analysis_with(path, false)
    }

    fn push_analysis_with(&mut self, path: PathBuf, live: bool) -> Vec<Command> {
        let a = &mut self.analysis;
        if self.screen != Screen::Analyze {
            a.stack.clear();
            a.back = self.screen;
            self.screen = Screen::Analyze;
        }
        let id = a.next_id;
        a.next_id += 1;
        a.stack.push(AnalysisView::new(id, path.clone()));
        vec![Command::CancelAnalyze, Command::RunAnalyze { id, path, live }]
    }

    /// Progress counters of the analysis on screen (shared with the worker).
    pub fn analysis_progress(&self) -> Option<Arc<Progress>> {
        self.analysis.stack.last().map(|v| v.progress.clone())
    }

    /// Something on screen changes without new messages (live counters, timers).
    pub fn needs_ticks(&self) -> bool {
        self.scan_running()
            || (self.screen == Screen::Browse && self.browse.as_ref().and_then(|b| b.stack.last()).is_some_and(|l| l.result.is_running()))
            || (matches!(self.screen, Screen::Recover | Screen::Doctor) && self.doctor.is_running())
            || (self.screen == Screen::Analyze && self.analysis.stack.last().is_some_and(|v| v.result.is_running()))
            || (self.screen == Screen::Find && (self.find.current.is_running() || self.find.history.is_running()))
            || self.careful.running.is_some()
    }

    /// Opens a browser of `source` at `path` (`None`: the scan root).
    pub fn open_browse(&mut self, source: BrowseSource, path: Option<PathBuf>, select: Option<String>) -> Vec<Command> {
        let back = match &self.browse {
            Some(b) if self.screen == Screen::Browse => b.back,
            _ => self.screen,
        };
        let id = match source {
            BrowseSource::Snapshot(id) | BrowseSource::Diff { new: id, .. } => id,
        };
        let root = self.snapshots.done().and_then(|l| l.snapshots.iter().find(|r| r.meta.id == id)).map(|r| PathBuf::from(&r.meta.root));
        self.browse = Some(BrowseUi { source, stack: Vec::new(), back, root, previous: None, note: None });
        self.screen = Screen::Browse;
        self.push_level(path, select)
    }

    fn push_level(&mut self, path: Option<PathBuf>, select: Option<String>) -> Vec<Command> {
        let Some(b) = self.browse.as_mut() else { return Vec::new() };
        let id = self.next_browse_id;
        self.next_browse_id += 1;
        let what = path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "the scan root".into());
        b.stack.push(BrowseLevel { id, path: path.clone(), result: Job::Running(format!("reading {what}…")), sel: 0, select, started: Instant::now() });
        vec![Command::RunBrowse { id, source: b.source, path }]
    }

    /// Leaves the browser for the screen it was opened from.
    fn close_browse(&mut self) {
        if let Some(b) = self.browse.take() {
            self.screen = b.back;
        }
    }

    fn browse_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let page = |k: KeyCode| if matches!(k, KeyCode::PageUp | KeyCode::PageDown) { 10 } else { 1 };
        let b = self.browse.as_mut()?;
        let Some(level) = b.stack.last_mut() else {
            self.close_browse();
            return Some(Vec::new());
        };
        let n = level.result.done().map(BrowseData::len).unwrap_or(0);
        b.note = None;
        match key.code {
            KeyCode::Up | KeyCode::PageUp => level.sel = level.sel.saturating_sub(page(key.code)),
            KeyCode::Down | KeyCode::PageDown => level.sel = (level.sel + page(key.code)).min(n.saturating_sub(1)),
            KeyCode::Home => level.sel = 0,
            KeyCode::End => level.sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => {
                return Some(match level.result.done().and_then(|r| r.child(level.sel)) {
                    Some((path, _, true)) => self.push_level(Some(path), None),
                    Some((_, name, false)) => {
                        b.note = Some(format!("{name} is a file: nothing to open. a analyses the folder it is in."));
                        Vec::new()
                    }
                    None => Vec::new(),
                });
            }
            // The same folder in the other view: what changed here, or the scan itself.
            KeyCode::Char('c') => {
                let (source, path) = (b.source, level.path.clone());
                let select = level.result.done().and_then(|r| r.child(level.sel)).map(|(_, name, _)| name);
                return Some(match source {
                    BrowseSource::Diff { new, .. } => self.open_browse(BrowseSource::Snapshot(new), path, select),
                    BrowseSource::Snapshot(id) => {
                        let meta = self.usable().iter().find(|r| r.meta.id == id).map(|r| r.meta.clone());
                        match meta.and_then(|m| self.previous_comparable(&m)) {
                            Some(old) => self.open_browse(BrowseSource::Diff { old: old.id, new: id }, path, select),
                            None => {
                                if let Some(b) = self.browse.as_mut() {
                                    b.note = Some(format!("No earlier scan of the same folder to compare #{id} with."));
                                }
                                Vec::new()
                            }
                        }
                    }
                });
            }
            KeyCode::Char('a') => {
                // The selected folder as it is now; on a file, the folder shown.
                let target = match level.result.done().and_then(|r| r.child(level.sel)) {
                    Some((path, _, true)) => Some(path),
                    _ => level.path.clone().or_else(|| b.root.clone()),
                };
                // A scan of a disk that is not connected: say so instead of failing.
                let id = match b.source {
                    BrowseSource::Snapshot(id) | BrowseSource::Diff { new: id, .. } => id,
                };
                if let Some(p) = target.as_ref().filter(|p| !p.exists())
                    && !self.disk_connected_now(id)
                {
                    let disk = self.snapshots.done().and_then(|l| l.snapshots.iter().find(|r| r.meta.id == id)).and_then(|r| r.disk_name.clone());
                    let note = format!("{} is not connected: connect it to analyse {} as it is now.", disk.as_deref().unwrap_or("Its disk"), p.display());
                    if let Some(b) = self.browse.as_mut() {
                        b.note = Some(note);
                    }
                    return Some(Vec::new());
                }
                return Some(match target {
                    Some(p) => self.push_analysis(p),
                    None => Vec::new(),
                });
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                // At the level the browser was opened on: go on up to the scan root.
                let up = match (&level.path, &b.root) {
                    (Some(p), Some(root)) if p != root => {
                        p.parent().map(|parent| (parent.to_path_buf(), p.file_name().map(|n| n.to_string_lossy().into_owned())))
                    }
                    _ => None,
                };
                if b.stack.len() > 1 {
                    b.stack.pop();
                    return Some(Vec::new());
                }
                return Some(match up {
                    Some((parent, name)) => {
                        b.previous = b.stack.pop();
                        b.stack.clear();
                        self.push_level(Some(parent), name)
                    }
                    None => {
                        self.close_browse();
                        Vec::new()
                    }
                });
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// The comparison's entries in the current list tab, as shown.
    pub fn diff_items(&self) -> Vec<DiffItem> {
        let Some(r) = self.diff.result.done() else { return Vec::new() };
        match self.diff.tab {
            DiffTab::GrowingDirs => r.report.growing_dirs.clone(),
            DiffTab::Files => r.report.files.clone(),
            DiffTab::Shrinking => {
                let mut v = r.report.shrinking_dirs.clone();
                v.extend(r.report.shrinking_files.iter().cloned());
                v.sort_by_key(|i| i.delta_allocated);
                v
            }
            DiffTab::Summary => Vec::new(),
        }
    }

    fn analysis_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let page = |k: KeyCode| if matches!(k, KeyCode::PageUp | KeyCode::PageDown) { 10 } else { 1 };
        let Some(view) = self.analysis.stack.last_mut() else {
            self.screen = self.analysis.back;
            return Some(Vec::new());
        };
        {
            let n = view.result.done().map(|r| r.items.len()).unwrap_or(0);
            view.note = None;
            match key.code {
                KeyCode::Up | KeyCode::PageUp => view.sel = view.sel.saturating_sub(page(key.code)),
                KeyCode::Down | KeyCode::PageDown => view.sel = (view.sel + page(key.code)).min(n.saturating_sub(1)),
                KeyCode::Home => view.sel = 0,
                KeyCode::End => view.sel = n.saturating_sub(1),
                KeyCode::Enter | KeyCode::Right => {
                    let item = view.result.done().and_then(|r| r.items.get(view.sel));
                    if let Some(i) = item.filter(|i| !i.is_dir) {
                        view.note = Some(format!("{} is a file: nothing to open.", i.name));
                    }
                    let target = item.filter(|i| i.is_dir).map(|i| PathBuf::from(&i.path));
                    return Some(match target {
                        Some(p) => self.push_analysis(p),
                        None => Vec::new(),
                    });
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                    let running = view.result.is_running();
                    self.analysis.stack.pop();
                    if self.analysis.stack.is_empty() {
                        self.screen = self.analysis.back;
                    }
                    return Some(if running { vec![Command::CancelAnalyze] } else { Vec::new() });
                }
                KeyCode::F(5) => {
                    let path = view.path.clone();
                    self.analysis.stack.pop();
                    return Some(self.push_analysis_with(path, true));
                }
                _ => return None,
            }
        }
        Some(Vec::new())
    }

    fn recover_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let page = |k: KeyCode| if matches!(k, KeyCode::PageUp | KeyCode::PageDown) { 10 } else { 1 };
        if key.code == KeyCode::F(5) && !self.doctor.is_running() {
            return Some(self.run_doctor(true));
        }
        if key.code == KeyCode::Char('n') && self.doctor.is_running() {
            return Some(self.skip_doctor_step());
        }
        if key.code == KeyCode::Tab {
            return Some(self.open(Screen::Doctor));
        }
        let n = self.recover.done().map(|r| r.candidates.len()).unwrap_or(0);
        let ui = &mut self.recover_ui;
        match key.code {
            KeyCode::Up | KeyCode::PageUp => ui.sel = ui.sel.saturating_sub(page(key.code)),
            KeyCode::Down | KeyCode::PageDown => ui.sel = (ui.sel + page(key.code)).min(n.saturating_sub(1)),
            KeyCode::Home => ui.sel = 0,
            KeyCode::End => ui.sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('a') => {
                let c = self.recover.done().and_then(|r| r.candidates.get(self.recover_ui.sel));
                // Growth opens on what changed there; `a` analyses the folder as it is now.
                if let Some((c, (old, new))) = c.and_then(|c| Some((c, c.growth?))).filter(|_| key.code != KeyCode::Char('a')) {
                    let path = c.path.as_ref().map(PathBuf::from);
                    return Some(self.open_browse(BrowseSource::Diff { old, new }, path, None));
                }
                let path = c.and_then(|c| c.path.as_ref()).map(PathBuf::from).filter(|p| p.is_dir());
                return Some(match path {
                    Some(p) => self.push_analysis(p),
                    None => Vec::new(),
                });
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn run_top(&mut self) -> Vec<Command> {
        let ids: Vec<u64> = self.usable().iter().map(|r| r.meta.id).collect();
        let Some(&id) = ids.get(self.top.snapshot) else { return Vec::new() };
        self.top.result = Job::Running(format!("reading scan #{id}…"));
        self.top.focus = TopFocus::List;
        self.scroll = 0;
        self.top.sel = 0;
        vec![Command::RunTop { snapshot: id, metric: self.top.metric }]
    }

    fn run_diff(&mut self) -> Vec<Command> {
        let ids: Vec<u64> = self.usable().iter().map(|r| r.meta.id).collect();
        let (Some(&old), Some(&new)) = (ids.get(self.diff.old), ids.get(self.diff.new)) else {
            return Vec::new();
        };
        self.diff.result = Job::Running(format!("comparing #{old} → #{new}…"));
        self.diff.focus = DiffFocus::Results;
        self.scroll = 0;
        self.diff.sel = 0;
        vec![Command::RunDiff { old, new, sort: self.diff.sort, min_size: self.diff.min_size }]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.request_quit();
        }
        if self.confirm_exit {
            self.confirm_exit = false;
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.should_quit = true;
                return vec![Command::CancelJobs, Command::Quit];
            }
            return Vec::new();
        }
        if self.confirm_quit || self.confirm_cancel {
            let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
            let quitting = self.confirm_quit;
            self.confirm_quit = false;
            self.confirm_cancel = false;
            if !yes {
                return Vec::new();
            }
            if quitting {
                self.should_quit = true;
                return vec![Command::CancelJobs, Command::Quit];
            }
            if self.auto_scan_running() {
                self.auto_status = Some("stopping the background scan…".into());
            }
            return vec![Command::CancelScan];
        }
        // On Careful mode, x stops the careful scan (the disk is still ejected).
        if self.screen == Screen::Careful && key.code == KeyCode::Char('x') && self.careful.running.is_some() {
            self.careful.log("stopping the careful scan; the disk is ejected after it".into());
            return vec![Command::CancelCareful];
        }
        // On Find, x stops the history search first.
        if self.screen == Screen::Find && !self.find.editing && key.code == KeyCode::Char('x') && self.find.history.is_running() {
            return self.stop_find_history();
        }
        // x stops a running scan from anywhere, after asking.
        if key.code == KeyCode::Char('x') && self.scan_running() && !self.text_input_active() {
            self.confirm_cancel = true;
            return Vec::new();
        }
        if self.text_input_active() {
            return match self.screen {
                Screen::Settings => self.settings_edit_key(key),
                Screen::Disks => self.disks_edit_key(key),
                Screen::Find => self.find_edit_key(key),
                _ => self.scan_setup_key(key),
            };
        }
        // Ctrl+letter is not the letter: Ctrl+U outside a text field opens nothing.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Vec::new();
        }
        // Screen-specific keys first.
        if let Some(cmds) = self.screen_key(key) {
            return cmds;
        }
        match key.code {
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'q') => self.request_quit(),
            KeyCode::Esc => {
                if self.screen == Screen::Dashboard {
                    self.confirm_exit = true;
                } else {
                    self.screen = Screen::Dashboard;
                    self.scroll = 0;
                }
                Vec::new()
            }
            KeyCode::Char('/') => self.open(Screen::Find),
            KeyCode::Char(c) => match Screen::from_hotkey(c) {
                Some(s) => self.open(s),
                None => Vec::new(),
            },
            KeyCode::Enter if self.screen == Screen::Dashboard => self.open(MENU[self.menu_index]),
            KeyCode::Up if self.screen == Screen::Dashboard => {
                if self.menu_index >= 2 {
                    self.menu_index -= 2;
                }
                Vec::new()
            }
            KeyCode::Down if self.screen == Screen::Dashboard => {
                if self.menu_index + 2 < MENU.len() {
                    self.menu_index += 2;
                }
                Vec::new()
            }
            KeyCode::Left | KeyCode::BackTab if self.screen == Screen::Dashboard => {
                // Left stays in the row; Shift+Tab walks back through the whole menu.
                if self.menu_index % 2 == 1 || (key.code == KeyCode::BackTab && self.menu_index > 0) {
                    self.menu_index -= 1;
                }
                Vec::new()
            }
            KeyCode::Right | KeyCode::Tab if self.screen == Screen::Dashboard => {
                if (self.menu_index.is_multiple_of(2) || key.code == KeyCode::Tab) && self.menu_index + 1 < MENU.len() {
                    self.menu_index += 1;
                }
                Vec::new()
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Vec::new()
            }
            KeyCode::Home => {
                self.scroll = 0;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Keys with a meaning on the current screen; `None` falls through to globals.
    fn screen_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        match self.screen {
            // Measuring again takes a while: only F5 does it, not a stray Enter.
            Screen::Doctor if key.code == KeyCode::F(5) && !self.doctor.is_running() => Some(self.run_doctor(true)),
            Screen::Doctor if key.code == KeyCode::Char('n') && self.doctor.is_running() => Some(self.skip_doctor_step()),
            Screen::Doctor if matches!(key.code, KeyCode::Tab | KeyCode::Enter) => Some(self.open(Screen::Recover)),
            Screen::Recover => self.recover_key(key),
            Screen::Scan => match (&self.scan, key.code) {
                (ScanStage::Confirm { root, name }, KeyCode::Enter) => {
                    let (root, name) = (root.clone(), name.clone());
                    Some(self.start_scan(root, name))
                }
                (ScanStage::AskDisk { root, name, .. }, KeyCode::Char(c @ ('s' | 'S' | 'n' | 'N'))) => {
                    let (root, name) = (root.clone(), name.clone());
                    let choice = if c.eq_ignore_ascii_case(&'s') { DiskChoice::Same } else { DiskChoice::New };
                    Some(self.start_scan_with(root, name, Some(choice)))
                }
                (ScanStage::AskDisk { root, name, .. }, KeyCode::Esc) => {
                    self.scan = ScanStage::Setup { root: root.clone(), name: name.clone(), field: ScanField::Root, error: None };
                    Some(Vec::new())
                }
                (ScanStage::AskDisk { .. }, _) => Some(Vec::new()),
                (ScanStage::Confirm { root, name }, KeyCode::Esc) => {
                    self.scan = ScanStage::Setup { root: root.clone(), name: name.clone(), field: ScanField::Root, error: None };
                    Some(Vec::new())
                }
                (ScanStage::Running { .. }, KeyCode::Esc | KeyCode::Char('c')) => {
                    self.confirm_cancel = true;
                    Some(Vec::new())
                }
                // Back to the menu; the scan goes on in the background.
                (ScanStage::Running { .. }, KeyCode::Left | KeyCode::Backspace) => {
                    self.screen = Screen::Dashboard;
                    Some(Vec::new())
                }
                (ScanStage::Finished(Ok(o)), KeyCode::Enter) if o.snapshot.status.is_usable() => {
                    let id = o.snapshot.id;
                    Some(self.open_browse(BrowseSource::Snapshot(id), None, None))
                }
                (ScanStage::Finished(Ok(o)), KeyCode::Char('c')) => {
                    let new = o.snapshot.clone();
                    match self.previous_comparable(&new).map(|m| m.id) {
                        Some(old) => Some(self.open_browse(BrowseSource::Diff { old, new: new.id }, None, None)),
                        None => Some(Vec::new()),
                    }
                }
                (ScanStage::Finished(r), KeyCode::Enter | KeyCode::Char('n')) => {
                    let error = r.as_ref().err().cloned();
                    self.new_scan_form(error);
                    Some(Vec::new())
                }
                // The result has been seen (it stays in the history): S next time is a new scan.
                (ScanStage::Finished(_), KeyCode::Esc) => {
                    self.new_scan_form(None);
                    self.screen = Screen::Dashboard;
                    Some(Vec::new())
                }
                _ => None,
            },
            Screen::Snapshots => {
                if let Some(cmds) = self.history_store_key(key) {
                    return Some(cmds);
                }
                let n = self.history().len();
                let sel = self.history().get(self.snap_sel).map(|r| r.meta.clone());
                let unusable = |m: &SnapshotMeta| {
                    let what = match m.status {
                        Status::Cancelled => "was cancelled",
                        Status::Failed => "failed",
                        _ => "did not finish",
                    };
                    format!("Scan #{} {what}: it holds no data, nothing to open.", m.id)
                };
                match key.code {
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                        self.snap_note = None;
                        self.snap_sel = match key.code {
                            KeyCode::Up => self.snap_sel.saturating_sub(1),
                            KeyCode::PageUp => self.snap_sel.saturating_sub(10),
                            KeyCode::Down => self.snap_sel + 1,
                            KeyCode::PageDown => self.snap_sel + 10,
                            KeyCode::Home => 0,
                            _ => n,
                        }
                        .min(n.saturating_sub(1));
                    }
                    KeyCode::Enter => match sel {
                        Some(m) if m.status.is_usable() => return Some(self.open_browse(BrowseSource::Snapshot(m.id), None, None)),
                        Some(m) => self.snap_note = Some(unusable(&m)),
                        None => {}
                    },
                    KeyCode::Char('c') => match sel {
                        Some(m) if m.status.is_usable() => {
                            // The selected snapshot against the previous comparable one.
                            match self.previous_comparable(&m).map(|p| p.id) {
                                Some(old) => return Some(self.open_browse(BrowseSource::Diff { old, new: m.id }, None, None)),
                                None => self.snap_note = Some(format!("No earlier scan of {} to compare #{} with.", m.root, m.id)),
                            }
                        }
                        Some(m) => self.snap_note = Some(unusable(&m)),
                        None => {}
                    },
                    KeyCode::Char('*') if self.disk_filter.is_some() => {
                        self.snap_marks.clear();
                        self.set_disk_filter(None);
                        self.snap_note = Some("Showing the scans of every disk.".into());
                    }
                    KeyCode::Char('t') => match sel {
                        Some(m) if m.status.is_usable() => {
                            let pos = self.usable().iter().position(|r| r.meta.id == m.id).unwrap_or(0);
                            self.screen = Screen::Top;
                            self.menu_index = MENU.iter().position(|s| *s == Screen::Top).unwrap();
                            self.top.snapshot = pos;
                            self.top.chosen = true;
                            self.top.focus = TopFocus::List;
                            return Some(self.run_top());
                        }
                        Some(m) => self.snap_note = Some(unusable(&m)),
                        None => {}
                    },
                    _ => return None,
                }
                Some(Vec::new())
            }
            Screen::Diff => self.diff_key(key),
            Screen::Top => self.top_key(key),
            Screen::Browse => self.browse_key(key),
            Screen::Analyze => self.analysis_key(key),
            Screen::Settings => self.settings_key(key),
            Screen::Disks => self.disks_key(key),
            Screen::Find => self.find_key(key),
            Screen::Careful => self.careful_key(key),
            Screen::Usage if key.code == KeyCode::F(5) && !self.usage.is_running() => {
                self.usage = Job::Running("reading volumes…".into());
                Some(vec![Command::LoadUsage])
            }
            _ => None,
        }
    }

    /// The setting under the cursor on the Settings screen.
    pub fn selected_field(&self) -> Field {
        FIELDS[self.settings_ui.sel.min(FIELDS.len() - 1)]
    }

    /// The newest scan before `new` that it can be compared with.
    pub fn previous_comparable(&self, new: &SnapshotMeta) -> Option<SnapshotMeta> {
        let usable = self.usable();
        let metas: Vec<&SnapshotMeta> = usable.iter().map(|r| &r.meta).collect();
        snapshots::previous_comparable(&metas, new).cloned()
    }

    /// Opening Compare: show the newest scan against the previous comparable one
    /// straight away, and follow newer scans until the user picks a pair.
    fn diff_latest(&mut self) -> Vec<Command> {
        let picking = matches!(self.diff.focus, DiffFocus::PickOld | DiffFocus::PickNew);
        if picking || (self.diff.chosen && !matches!(self.diff.result, Job::Idle)) {
            return Vec::new();
        }
        let metas: Vec<&SnapshotMeta> = self.usable().iter().map(|r| &r.meta).collect();
        match latest_pair(&metas) {
            Some(pair) if matches!(self.diff.result, Job::Idle) || pair != (self.diff.old, self.diff.new) => {
                (self.diff.old, self.diff.new) = pair;
                self.diff.focus = DiffFocus::Results;
                self.run_diff()
            }
            _ => Vec::new(),
        }
    }

    /// Whether row `i` of the picker can be chosen in the current step.
    pub fn diff_choosable(&self, i: usize) -> bool {
        let usable = self.usable();
        let Some(m) = usable.get(i).map(|r| &r.meta) else { return false };
        match self.diff.focus {
            DiffFocus::PickNew => usable.get(self.diff.old).is_some_and(|o| comparable(&o.meta, m)),
            _ => usable[i + 1..].iter().any(|r| comparable(m, &r.meta)),
        }
    }

    fn diff_pick(&mut self) {
        self.diff.focus = DiffFocus::PickOld;
        self.diff.cursor = self.diff.old;
    }

    fn diff_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let n = self.usable().len();
        match (self.diff.focus, key.code) {
            (DiffFocus::PickOld | DiffFocus::PickNew, KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown) => {
                let c = self.diff.cursor;
                // Move to the nearest row that can be chosen, skipping greyed ones.
                let next = match key.code {
                    KeyCode::Up => (0..c).rev().find(|&i| self.diff_choosable(i)),
                    KeyCode::Down => (c + 1..n).find(|&i| self.diff_choosable(i)),
                    KeyCode::PageUp => (0..c.saturating_sub(9)).rev().find(|&i| self.diff_choosable(i)).or_else(|| (0..c).find(|&i| self.diff_choosable(i))),
                    KeyCode::PageDown => (c + 10..n).find(|&i| self.diff_choosable(i)).or_else(|| (c + 1..n).rev().find(|&i| self.diff_choosable(i))),
                    KeyCode::Home => (0..n).find(|&i| self.diff_choosable(i)),
                    _ => (0..n).rev().find(|&i| self.diff_choosable(i)),
                };
                if let Some(i) = next {
                    self.diff.cursor = i;
                }
            }
            (DiffFocus::PickOld | DiffFocus::PickNew, KeyCode::Enter | KeyCode::Right) => {
                let c = self.diff.cursor;
                if !self.diff_choosable(c) {
                    return Some(Vec::new());
                }
                if self.diff.focus == DiffFocus::PickNew {
                    self.diff.new = c;
                    self.diff.chosen = true;
                    return Some(self.run_diff());
                }
                self.diff.old = c;
                self.diff.focus = DiffFocus::PickNew;
                // Keep the later scan if it still fits, else offer the newest that does.
                self.diff.cursor = if self.diff_choosable(self.diff.new) {
                    self.diff.new
                } else {
                    (0..n).rev().find(|&i| self.diff_choosable(i)).unwrap_or(c)
                };
            }
            (DiffFocus::PickNew, KeyCode::Esc | KeyCode::Left | KeyCode::Backspace) => {
                self.diff.focus = DiffFocus::PickOld;
                self.diff.cursor = self.diff.old;
            }
            // Back to the comparison on screen, if there is one.
            (DiffFocus::PickOld, KeyCode::Esc | KeyCode::Left | KeyCode::Backspace) if !matches!(self.diff.result, Job::Idle) => {
                self.diff.focus = DiffFocus::Results;
            }
            (DiffFocus::PickOld | DiffFocus::PickNew, _) => return None,
            (_, KeyCode::Char('p') | KeyCode::Char('b')) | (DiffFocus::Header, KeyCode::Enter) if n >= 2 => self.diff_pick(),
            (DiffFocus::Header, KeyCode::Down) => self.diff.focus = DiffFocus::Results,
            (DiffFocus::Header, KeyCode::Up) => {}
            (DiffFocus::Header | DiffFocus::Results, KeyCode::Right | KeyCode::Tab) => {
                let i = DIFF_TABS.iter().position(|t| *t == self.diff.tab).unwrap();
                self.diff.tab = DIFF_TABS[(i + 1) % DIFF_TABS.len()];
                self.diff.sel = 0;
                self.scroll = 0;
            }
            (DiffFocus::Header | DiffFocus::Results, KeyCode::Left | KeyCode::BackTab) => {
                let i = DIFF_TABS.iter().position(|t| *t == self.diff.tab).unwrap();
                self.diff.tab = DIFF_TABS[(i + DIFF_TABS.len() - 1) % DIFF_TABS.len()];
                self.diff.sel = 0;
                self.scroll = 0;
            }
            // ↑ at the top of the results moves up to the scans being compared.
            (DiffFocus::Results, KeyCode::Up) if n >= 2 && (if self.diff.tab == DiffTab::Summary { self.scroll == 0 } else { self.diff.sel == 0 }) => {
                self.diff.focus = DiffFocus::Header;
            }
            (DiffFocus::Results, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End)
                if self.diff.tab != DiffTab::Summary =>
            {
                let n = self.diff_items().len();
                let d = &mut self.diff;
                d.sel = match key.code {
                    KeyCode::Up => d.sel.saturating_sub(1),
                    KeyCode::PageUp => d.sel.saturating_sub(10),
                    KeyCode::Down => d.sel + 1,
                    KeyCode::PageDown => d.sel + 10,
                    KeyCode::Home => 0,
                    _ => n,
                }
                .min(n.saturating_sub(1));
            }
            (DiffFocus::Results, KeyCode::Enter) => {
                let d = &self.diff;
                let Some(r) = d.result.done() else { return Some(Vec::new()) };
                let source = BrowseSource::Diff { old: r.old.id, new: r.new.id };
                let target = if d.tab == DiffTab::Summary {
                    Some((r.path.clone(), None))
                } else {
                    self.diff_items().get(self.diff.sel).map(|i| {
                        let (p, sel) = open_target(&i.path, i.is_dir);
                        (Some(p), sel)
                    })
                };
                return Some(match target {
                    Some((path, select)) => self.open_browse(source, path, select),
                    None => Vec::new(),
                });
            }
            (DiffFocus::Header | DiffFocus::Results, KeyCode::Char('o')) if self.diff.result.done().is_some() => {
                let i = SORTS.iter().position(|s| *s == self.diff.sort).unwrap();
                self.diff.sort = SORTS[(i + 1) % SORTS.len()];
                return Some(self.run_diff());
            }
            (DiffFocus::Header | DiffFocus::Results, KeyCode::Char('f')) if self.diff.result.done().is_some() => {
                let i = MIN_SIZES.iter().position(|s| *s == self.diff.min_size).unwrap_or(0);
                self.diff.min_size = MIN_SIZES[(i + 1) % MIN_SIZES.len()];
                return Some(self.run_diff());
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Opening Largest files: the newest scan, until the user picks one.
    fn top_latest(&mut self) -> Vec<Command> {
        let n = self.usable().len();
        if n == 0 || self.top.focus == TopFocus::Pick || (self.top.chosen && !matches!(self.top.result, Job::Idle)) {
            return Vec::new();
        }
        if matches!(self.top.result, Job::Idle) || self.top.snapshot != n - 1 {
            self.top.snapshot = n - 1;
            return self.run_top();
        }
        Vec::new()
    }

    fn top_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let n = self.usable().len();
        let items = |t: &TopUi| t.result.done().map(|r| if t.view == TopView::Dirs { r.report.dirs.len() } else { r.report.files.len() }).unwrap_or(0);
        let rows = items(&self.top);
        let t = &mut self.top;
        if t.focus == TopFocus::Pick {
            match key.code {
                KeyCode::Up => t.cursor = t.cursor.saturating_sub(1),
                KeyCode::Down => t.cursor = (t.cursor + 1).min(n.saturating_sub(1)),
                KeyCode::PageUp => t.cursor = t.cursor.saturating_sub(10),
                KeyCode::PageDown => t.cursor = (t.cursor + 10).min(n.saturating_sub(1)),
                KeyCode::Home => t.cursor = 0,
                KeyCode::End => t.cursor = n.saturating_sub(1),
                KeyCode::Enter | KeyCode::Right => {
                    t.focus = TopFocus::List;
                    t.chosen = true;
                    if t.cursor != t.snapshot || !matches!(t.result, Job::Done(_)) {
                        t.snapshot = t.cursor;
                        return Some(self.run_top());
                    }
                }
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => t.focus = TopFocus::List,
                _ => return None,
            }
            return Some(Vec::new());
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                t.view = if t.view == TopView::Dirs { TopView::Files } else { TopView::Dirs };
                t.sel = 0;
                self.scroll = 0;
                Some(Vec::new())
            }
            KeyCode::Char('p') | KeyCode::Enter if n > 0 && (key.code != KeyCode::Enter || t.focus == TopFocus::Header) => {
                t.focus = TopFocus::Pick;
                t.cursor = t.snapshot;
                Some(Vec::new())
            }
            KeyCode::Down if t.focus == TopFocus::Header => {
                t.focus = TopFocus::List;
                Some(Vec::new())
            }
            KeyCode::Up if t.focus == TopFocus::Header => Some(Vec::new()),
            // ↑ at the top of the list moves up to the scan line.
            KeyCode::Up if t.sel == 0 => {
                t.focus = TopFocus::Header;
                Some(Vec::new())
            }
            KeyCode::Up | KeyCode::PageUp => {
                t.sel = t.sel.saturating_sub(if key.code == KeyCode::Up { 1 } else { 10 });
                Some(Vec::new())
            }
            KeyCode::Down | KeyCode::PageDown => {
                t.sel = (t.sel + if key.code == KeyCode::Down { 1 } else { 10 }).min(rows.saturating_sub(1));
                Some(Vec::new())
            }
            KeyCode::Home | KeyCode::End => {
                t.sel = if key.code == KeyCode::Home { 0 } else { rows.saturating_sub(1) };
                Some(Vec::new())
            }
            KeyCode::Enter => {
                let Some(r) = t.result.done() else { return Some(Vec::new()) };
                let list = if t.view == TopView::Dirs { &r.report.dirs } else { &r.report.files };
                let target = list.get(t.sel).map(|i| open_target(&i.path, i.kind == diskdrift_app::core::record::Kind::Dir));
                let id = r.snapshot.id;
                Some(match target {
                    Some((path, select)) => self.open_browse(BrowseSource::Snapshot(id), Some(path), select),
                    None => Vec::new(),
                })
            }
            KeyCode::Char('m') => {
                self.top.metric = match self.top.metric {
                    SizeMetric::Allocated => SizeMetric::Logical,
                    SizeMetric::Logical => SizeMetric::Allocated,
                };
                Some(self.run_top())
            }
            _ => None,
        }
    }

    fn settings_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let ui = &mut self.settings_ui;
        let field = FIELDS[ui.sel.min(FIELDS.len() - 1)];
        match key.code {
            // A message is about the setting it was saved for.
            KeyCode::Up => {
                ui.sel = ui.sel.saturating_sub(1);
                ui.message = None;
            }
            KeyCode::Down => {
                ui.sel = (ui.sel + 1).min(FIELDS.len() - 1);
                ui.message = None;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Ok(file) = &ui.file else { return Some(Vec::new()) };
                match field.next(file) {
                    Some(v) => return Some(self.save_setting(field, v)),
                    None => ui.edit = Some(field.get(file)),
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn settings_edit_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let ui = &mut self.settings_ui;
        let Some(text) = ui.edit.as_mut() else { return Vec::new() };
        match key.code {
            KeyCode::Esc => {
                ui.edit = None;
                ui.message = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if text.chars().count() < 4096 => text.push(c),
            KeyCode::Enter => {
                let field = FIELDS[ui.sel.min(FIELDS.len() - 1)];
                // A default root that does not exist would fail every scan: say so now.
                if field.key == "scan.defaultRoot"
                    && let Err(e) = snapshots::check_root(text, &self.settings.home)
                {
                    ui.message = Some(Err(format!("not saved: {e}")));
                    return Vec::new();
                }
                let v = ui.edit.take().unwrap_or_default();
                return self.save_setting(field, v);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Sets one field, writes the config file and applies the result.
    fn save_setting(&mut self, field: Field, value: String) -> Vec<Command> {
        let Ok(file) = &self.settings_ui.file else { return Vec::new() };
        let mut cfg = file.clone();
        let path = self.settings.config_path.value.clone();
        let saved = field
            .set(&mut cfg, &value)
            .and_then(|()| config::save(&path, &cfg).map_err(|e| e.to_string()))
            .and_then(|()| self.settings.reload().map_err(|e| e.to_string()));
        let ui = &mut self.settings_ui;
        match saved {
            Ok(effective) => {
                ui.file = Ok(cfg);
                let note = if field.overridden(&effective) { " A command-line flag overrides it this session." } else { "" };
                ui.message = Some(Ok(format!("Saved: {} = {}.{note}", field.label, field.display(ui.file.as_ref().unwrap()))));
                let store_changed = effective.storage_path.value != self.settings.storage_path.value;
                self.settings = effective;
                if store_changed {
                    self.snapshots = Job::Running("loading snapshots…".into());
                    self.browse = None;
                    self.diff.result = Job::Idle;
                    self.top.result = Job::Idle;
                    return vec![Command::LoadSnapshots];
                }
            }
            Err(e) => ui.message = Some(Err(format!("not saved: {e}"))),
        }
        Vec::new()
    }

    fn scan_setup_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let ScanStage::Setup { root, name, field, error } = &mut self.scan else { return Vec::new() };
        if !matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down) {
            *error = None;
        }
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
                self.scroll = 0;
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                *field = if *field == ScanField::Root { ScanField::Name } else { ScanField::Root };
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let s = if *field == ScanField::Root { root } else { name };
                s.clear();
            }
            KeyCode::Backspace => {
                let s = if *field == ScanField::Root { root } else { name };
                s.pop();
            }
            KeyCode::Char(c) => {
                let s = if *field == ScanField::Root { root } else { name };
                if s.chars().count() < 4096 {
                    s.push(c);
                }
            }
            KeyCode::Enter => {
                // Checked here, not after the confirmation: a typo is fixed in place.
                let r = match snapshots::check_root(root, &self.settings.home) {
                    Ok(p) => p.display().to_string(),
                    Err(e) => {
                        *error = Some(e);
                        *field = ScanField::Root;
                        return Vec::new();
                    }
                };
                let n = name.trim().to_string();
                if self.settings.confirm_before_scan.value {
                    self.scan = ScanStage::Confirm { root: r, name: n };
                } else {
                    return self.start_scan(r, n);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn start_scan(&mut self, root: String, name: String) -> Vec<Command> {
        self.start_scan_with(root, name, None)
    }

    fn start_scan_with(&mut self, root: String, name: String, disk: Option<DiskChoice>) -> Vec<Command> {
        if let Some(job) = &self.store_job {
            let why = format!("The store is busy ({job}); scan when it has finished.");
            self.new_scan_form(Some(why));
            return Vec::new();
        }
        let progress = Arc::new(Progress::default());
        let expected = self.last_scan_of(&root).and_then(|m| m.counts).map(|c| c.entries);
        self.last_root = Some(root.clone());
        self.scan_request = Some((root.clone(), name.clone()));
        self.scan = ScanStage::Running { progress, started: Instant::now(), root: root.clone(), auto: false, expected };
        vec![Command::StartScan { root, name: if name.is_empty() { None } else { Some(name) }, background: false, disk }]
    }

    /// A new scan offers the folder of the last one again (or, with History
    /// filtered to a disk, the folder last scanned on it); a failed one says why.
    fn new_scan_form(&mut self, error: Option<String>) {
        let mut form = initial_scan(&self.settings);
        let disk_root = self.disk_filter.as_ref().and(self.usable().last().map(|r| r.meta.root.clone()));
        if let (ScanStage::Setup { root, error: e, .. }, Some(last)) = (&mut form, disk_root.or_else(|| self.last_root.clone())) {
            *root = last;
            *e = error;
        }
        self.scan = form;
    }

    /// The newest scan with data of `root`, for how long scanning it again takes.
    pub fn last_scan_of(&self, root: &str) -> Option<SnapshotMeta> {
        let usable = self.usable();
        let metas: Vec<&SnapshotMeta> = usable.iter().map(|r| &r.meta).collect();
        snapshots::last_scan_of(&metas, std::path::Path::new(root)).cloned()
    }

    /// Once per session, after the snapshot list first loads: start the
    /// automatic background scan if the rule says so.
    fn maybe_auto_scan(&mut self) -> Vec<Command> {
        if self.auto_checked || self.scan_running() {
            return Vec::new();
        }
        let Some(list) = self.snapshots.done() else { return Vec::new() };
        self.auto_checked = true;
        match snapshots::auto_scan(&self.settings, list, chrono::Local::now().timestamp()) {
            AutoScan::Start { root, name, expected_entries } => {
                let root = root.display().to_string();
                self.auto_status = Some(format!("scanning {root} in the background (low priority)"));
                self.scan = ScanStage::Running {
                    progress: Arc::default(),
                    started: Instant::now(),
                    root: root.clone(),
                    auto: true,
                    expected: expected_entries,
                };
                vec![Command::StartScan { root, name: Some(name), background: true, disk: None }]
            }
            AutoScan::Skip(why) => {
                self.auto_status = Some(format!("no background scan: {why}"));
                Vec::new()
            }
        }
    }

    /// Progress counters of the running scan (shared with the worker).
    pub fn scan_progress(&self) -> Option<Arc<Progress>> {
        match &self.scan {
            ScanStage::Running { progress, .. } => Some(progress.clone()),
            _ => None,
        }
    }

    fn note_disk(&mut self, text: String) {
        self.disk_event = Some(LogLine::now(text));
        self.disk_event_at = Some(Instant::now());
    }

    /// The latest disk event happened in the last minute (the header shows it).
    pub fn disk_event_is_recent(&self) -> bool {
        self.disk_event_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
    }

    /// The disk of snapshot `id` is mounted now. When it cannot be told (no
    /// disk recorded, the disk list not read yet), it counts as connected, so
    /// nothing is blocked on a guess.
    fn disk_connected_now(&self, id: u64) -> bool {
        let disk = self.snapshots.done().and_then(|l| l.snapshots.iter().find(|r| r.meta.id == id)).and_then(|r| r.meta.disk.clone());
        let row = self.disks.list.done().and_then(|l| disk.and_then(|d| l.disks.iter().find(|r| r.known && r.disk.id == d)).cloned());
        row.is_none_or(|r| r.connected)
    }

    /// A volume's name for messages: its label in the store, else its volume name.
    fn volume_name(&self, v: &Volume) -> String {
        self.disks
            .list
            .done()
            .and_then(|l| l.disks.iter().find(|r| r.disk.identity == v.identity && r.known))
            .map(|r| r.disk.display_name().to_string())
            .or_else(|| v.name.clone())
            .unwrap_or_else(|| v.mount_point.display().to_string())
    }

    /// The mounts changed: new volumes are looked at, gone ones noted. The
    /// first list is the baseline (what is mounted at start is not "connected").
    fn volumes_changed(&mut self, now: Vec<Volume>) -> Vec<Command> {
        let before = self.volumes.replace(now.clone());
        let mut cmds = Vec::new();
        if let Some(before) = before {
            for v in before.iter().filter(|v| !now.iter().any(|n| n.identity == v.identity)) {
                let name = self.volume_name(v);
                self.note_disk(format!("{name} disconnected; its scans stay browsable"));
            }
            for v in now.iter().filter(|v| !before.iter().any(|b| b.identity == v.identity)) {
                cmds.push(Command::DiskConnected(v.clone()));
            }
        }
        if !self.disks.list.is_running() {
            self.disks.list = Job::Running("reading disks…".into());
            cmds.push(Command::LoadDisks);
        }
        cmds
    }

    /// What to do about a volume just connected (decided by the app layer).
    fn disk_connected(&mut self, volume: Volume, action: Result<ConnectAction, String>) -> Vec<Command> {
        let ago = |rfc: &str| {
            chrono::DateTime::parse_from_rfc3339(rfc)
                .map(|t| diskdrift_app::analyze::ago(chrono::Local::now().timestamp() - t.timestamp()))
                .unwrap_or_else(|_| rfc.to_string())
        };
        let shown = action.as_ref().ok().and_then(|a| a.disk()).map(|d| d.display_name().to_string()).unwrap_or_else(|| self.volume_name(&volume));
        let at = volume.mount_point.display();
        match action {
            Ok(ConnectAction::Rescan { .. }) if self.careful.is_on() => {
                self.note_disk(format!("{shown} mounted at {at}; careful mode is on, so it is not rescanned"))
            }
            Ok(ConnectAction::Rescan { root, last_scan, .. }) => {
                let root = root.display().to_string();
                if !self.can_scan_in_background() {
                    self.note_disk(format!("{shown} connected (last scanned {}); it is rescanned as soon as the scan screen is free", ago(&last_scan)));
                    self.pending_connect.push_back((shown, root));
                    return Vec::new();
                }
                self.note_disk(format!("{shown} connected (last scanned {}): rescanning it to see what changed (x stops)", ago(&last_scan)));
                return self.start_connect_scan(shown, root);
            }
            Ok(ConnectAction::Recent { last_scan, .. }) => self.note_disk(format!("{shown} connected at {at}; scanned {}, no rescan needed", ago(&last_scan))),
            Ok(ConnectAction::Off { .. }) => self.note_disk(format!("{shown} connected at {at}; rescanning on connect is off (Settings)")),
            Ok(ConnectAction::New { disk: None }) => {
                self.note_disk(format!("new disk {shown} connected at {at}: K to give it a label or make its first scan"))
            }
            Ok(ConnectAction::New { disk: Some(_) }) => self.note_disk(format!("{shown} connected at {at}; it has no scan yet: K, then g, scans it")),
            Ok(ConnectAction::Doubt { message }) => self.note_disk(format!("{message} Scan it with S: DiskDrift asks which disk it is.")),
            Err(e) => self.note_disk(format!("{shown} connected at {at}; cannot read the store: {e}")),
        }
        Vec::new()
    }

    /// Rescans a connected disk like the background scan at start (low
    /// priority, `x` stops it).
    fn start_connect_scan(&mut self, disk: String, root: String) -> Vec<Command> {
        let expected = self.last_scan_of(&root).and_then(|m| m.counts).map(|c| c.entries);
        self.connect_scan = Some(disk);
        self.scan_request = Some((root.clone(), String::new()));
        self.scan = ScanStage::Running { progress: Arc::default(), started: Instant::now(), root: root.clone(), auto: true, expected };
        vec![Command::StartScan { root, name: Some(format!("auto-{}", snapshots::default_name())), background: true, disk: None }]
    }

    /// A background scan may take the scan slot: nothing runs, and the scan
    /// screen is not showing a result, a question or a form being filled in.
    fn can_scan_in_background(&self) -> bool {
        matches!(self.scan, ScanStage::Setup { .. }) && self.screen != Screen::Scan && self.store_job.is_none()
    }

    /// Called by the event loop on every turn: starts a waiting rescan of a
    /// connected disk once the scan slot is free.
    pub fn idle(&mut self) -> Vec<Command> {
        if self.pending_connect.is_empty() || !self.can_scan_in_background() {
            return Vec::new();
        }
        self.next_connect_scan()
    }

    /// The next waiting rescan of a connected disk.
    fn next_connect_scan(&mut self) -> Vec<Command> {
        match self.pending_connect.pop_front() {
            Some((disk, root)) => {
                self.note_disk(format!("{disk}: rescanning it now (x stops)"));
                self.start_connect_scan(disk, root)
            }
            None => Vec::new(),
        }
    }

    /// The previous scan comparable with `m` in the whole store (not only the
    /// disk History is filtered to).
    fn previous_in_store(&self, m: &SnapshotMeta) -> Option<SnapshotMeta> {
        let l = self.snapshots.done()?;
        let metas: Vec<&SnapshotMeta> = l.snapshots.iter().map(|r| &r.meta).filter(|x| x.status.is_usable()).collect();
        snapshots::previous_comparable(&metas, m).cloned()
    }

    /// History's cursor on the newest scan with data it lists.
    fn snap_sel_newest(&mut self) {
        let rows = self.history();
        self.snap_sel = rows.iter().rposition(|r| r.meta.status.is_usable()).unwrap_or(rows.len().saturating_sub(1));
    }

    /// Filters History (and with it Compare, Largest files and the scan form)
    /// to one disk, or shows every disk again.
    pub fn set_disk_filter(&mut self, filter: Option<DiskFilter>) {
        if self.disk_filter == filter {
            return;
        }
        self.disk_filter = filter;
        self.snap_sel_newest();
        // Indices into the usable scans changed meaning.
        self.diff.result = Job::Idle;
        self.diff.chosen = false;
        self.diff.focus = DiffFocus::Results;
        self.top.result = Job::Idle;
        self.top.chosen = false;
        self.top.focus = TopFocus::List;
    }

    /// The disk under the cursor on My disks.
    pub fn selected_disk(&self) -> Option<&disks::DiskRow> {
        self.disks.list.done().and_then(|l| l.disks.get(self.disks.sel))
    }

    pub fn careful_selected(&self) -> Option<&Held> {
        self.careful.held.get(self.careful.sel)
    }

    fn careful_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        let ui = &mut self.careful;
        // The question about a doubtful disk takes the next key.
        if let Some(bsd) = ui.ask.take() {
            let choice = match key.code {
                KeyCode::Char('s') => Some(DiskChoice::Same),
                KeyCode::Char('n') => Some(DiskChoice::New),
                _ => None,
            };
            return Some(match choice {
                Some(c) => self.start_careful_scan(bsd, Some(c)),
                None => {
                    self.careful.log("not scanned; it stays held back".into());
                    Vec::new()
                }
            });
        }
        match key.code {
            KeyCode::Char('o') => {
                if ui.running.is_some() {
                    ui.log("a careful scan is running: wait for it, or x stops it".into());
                    return Some(Vec::new());
                }
                return Some(match ui.on {
                    None => {
                        ui.on = Some(false);
                        ui.log("turning careful mode on…".into());
                        vec![Command::Careful(true)]
                    }
                    Some(_) => {
                        ui.on = None;
                        ui.held.clear();
                        ui.sel = 0;
                        ui.log(diskdrift_app::text::careful::OFF.into());
                        vec![Command::Careful(false)]
                    }
                });
            }
            KeyCode::Up => ui.sel = ui.sel.saturating_sub(1),
            KeyCode::Down if ui.sel + 1 < ui.held.len() => ui.sel += 1,
            KeyCode::Char('g') | KeyCode::Enter => {
                let Some(h) = self.careful_selected().cloned() else {
                    self.careful.log(if self.careful.is_on() { "no disk held back: connect one" } else { "careful mode is off: o turns it on" }.into());
                    return Some(Vec::new());
                };
                if self.careful.running.is_some() || self.scan_running() {
                    self.careful.log("a scan is running: one at a time".into());
                    return Some(Vec::new());
                }
                if let Some(why) = &h.doubt {
                    self.careful.log(format!("{}: {why}. Is it that disk? s the same disk, n another, any other key: not now", h.name()));
                    self.careful.ask = Some(h.bsd().to_string());
                    return Some(Vec::new());
                }
                return Some(self.start_careful_scan(h.bsd().to_string(), None));
            }
            KeyCode::Char(c @ ('m' | 'j')) => {
                let Some(h) = self.careful_selected().cloned() else { return Some(Vec::new()) };
                if self.careful.running.as_ref().is_some_and(|r| r.0 == h.bsd()) {
                    self.careful.log("it is being scanned: x stops the scan, then it is ejected".into());
                    return Some(Vec::new());
                }
                let bsd = h.bsd().to_string();
                return Some(vec![if c == 'm' { Command::CarefulMount { bsd } } else { Command::CarefulEject { bsd } }]);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn start_careful_scan(&mut self, bsd: String, choice: Option<DiskChoice>) -> Vec<Command> {
        let name = self.careful.held.iter().find(|h| h.bsd() == bsd).map(|h| h.name()).unwrap_or_else(|| bsd.clone());
        self.careful.log(format!("{name}: mounting it read-only and scanning with one thread (x stops; it is ejected afterwards)"));
        self.careful.running = Some((bsd.clone(), Arc::default(), Instant::now()));
        vec![Command::CarefulScan { bsd, choice }]
    }

    /// Progress of the careful scan, for the worker.
    pub fn careful_progress(&self) -> Option<Arc<Progress>> {
        self.careful.running.as_ref().map(|r| r.1.clone())
    }

    /// Starts a search for what is typed.
    fn start_find(&mut self) -> Vec<Command> {
        let pattern = self.find.input.trim().to_string();
        if pattern.is_empty() {
            self.find.note = Some("type a part of a name, a glob (*.pdf) or re:regex, then Enter".into());
            return Vec::new();
        }
        let (disk, set) = match &self.disk_filter {
            Some(DiskFilter { set: Some(set), .. }) => (None, Some(set.clone())),
            Some(f) => (f.ids.first().cloned(), None),
            None => (None, None),
        };
        let query = Query { disk: disk.clone(), set: set.clone(), limit: FIND_LIMIT, ..Query::new(&pattern) };
        if let Err(e) = diskdrift_app::search::Matcher::new(&query) {
            self.find.note = Some(e.to_string());
            return Vec::new();
        }
        self.find.id += 1;
        self.find.query = Some(query);
        self.find.current = Job::Running("searching the latest scans…".into());
        self.find.history = Job::Idle;
        self.find.progress = None;
        self.find.sel = 0;
        self.find.editing = false;
        self.find.note = None;
        vec![Command::CancelFind, Command::RunFind { id: self.find.id, pattern, disk, set }]
    }

    fn start_find_history(&mut self, always: bool) -> Vec<Command> {
        if self.find.history.is_running() || self.find.current.done().is_none() {
            return Vec::new();
        }
        self.find.history = Job::Running("searching the history…".into());
        self.find.progress = None;
        vec![Command::RunFindHistory { id: self.find.id, always }]
    }

    fn stop_find_history(&mut self) -> Vec<Command> {
        self.find.note = Some("stopping the history search…".into());
        vec![Command::CancelFind]
    }

    fn find_rows(&self) -> usize {
        self.find.current.done().map(|c| c.hits.len()).unwrap_or(0) + self.find.history.done().map(|h| h.gone.len()).unwrap_or(0)
    }

    /// The result under the cursor.
    pub fn find_selected(&self) -> Option<FindRow<'_>> {
        let now = self.find.current.done().map(|c| c.hits.as_slice()).unwrap_or_default();
        match now.get(self.find.sel) {
            Some(h) => Some(FindRow::Now(h)),
            None => self.find.history.done()?.gone.get(self.find.sel - now.len()).map(FindRow::Gone),
        }
    }

    fn find_edit_key(&mut self, key: KeyEvent) -> Vec<Command> {
        self.find.note = None;
        let text = &mut self.find.input;
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
                self.scroll = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) && text.chars().count() < 512 => text.push(c),
            KeyCode::Enter => return self.start_find(),
            KeyCode::Tab | KeyCode::Down if self.find_rows() > 0 => self.find.editing = false,
            _ => {}
        }
        Vec::new()
    }

    fn find_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        self.find.note = None;
        let rows = self.find_rows();
        match key.code {
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('/') => self.find.editing = true,
            KeyCode::Up if self.find.sel == 0 => self.find.editing = true,
            KeyCode::Up => self.find.sel -= 1,
            KeyCode::Down if self.find.sel + 1 < rows => self.find.sel += 1,
            KeyCode::PageDown => self.find.sel = (self.find.sel + 10).min(rows.saturating_sub(1)),
            KeyCode::PageUp => self.find.sel = self.find.sel.saturating_sub(10),
            KeyCode::Enter => return Some(self.find_open()),
            KeyCode::Char('p') => {
                if self.find.history.done().is_some() {
                    self.find.note = Some("the history is searched already (F5 searches again)".into());
                }
                return Some(self.start_find_history(true));
            }
            KeyCode::F(5) => return Some(self.start_find()),
            // Not a hotkey here: the other screens' letters still work.
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Enter on a result: Browse on its scan at the folder that holds it.
    fn find_open(&mut self) -> Vec<Command> {
        let (scan, path, dir, note) = match self.find_selected() {
            None => return Vec::new(),
            Some(FindRow::Now(h)) => {
                let note = (!h.place.connected).then(|| {
                    format!("{} is not connected: this is scan #{}; connect the disk to get at the file.", h.place.disk_name, h.scan.id)
                });
                (h.scan.id, h.path.0.clone(), h.kind == Kind::Dir, note)
            }
            Some(FindRow::Gone(l)) => {
                let gone = l.gone_in.as_ref().map(|g| format!(" It is not in scan #{}.", g.id)).unwrap_or_default();
                let note = format!("Scan #{}, the last one that has it.{gone}", l.last_seen.id);
                (l.last_seen.id, l.path.0.clone(), l.kind == Kind::Dir, Some(note))
            }
        };
        let (folder, select) = open_target(&path, dir);
        let cmds = self.open_browse(BrowseSource::Snapshot(scan), Some(folder), select);
        if let Some(b) = self.browse.as_mut() {
            b.note = note;
        }
        cmds
    }

    /// After a removal or compaction: History and My disks read the store again.
    fn reload_store_views(&mut self) -> Vec<Command> {
        self.disks.list = Job::Idle;
        let mut cmds = self.load_disks();
        cmds.push(Command::LoadSnapshots);
        cmds
    }

    /// History's keys for removing and compacting: marks, the question, the job.
    fn history_store_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        if let Some(p) = self.snap_remove.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let ids: Vec<u64> = p.snapshots.iter().map(|r| r.meta.id).collect();
                self.store_job = Some(format!("removing {}", scans_text(ids.len())));
                return Some(vec![Command::RemoveSnapshots { ids }]);
            }
            self.snap_note = Some("Nothing was removed.".into());
            return Some(Vec::new());
        }
        if self.store_job.is_some() {
            return match key.code {
                KeyCode::Char('x') => Some(vec![Command::CancelStoreJob]),
                KeyCode::Char(' ') | KeyCode::Char('o') | KeyCode::Char('n') | KeyCode::Char('p') | KeyCode::Char('z') | KeyCode::Delete | KeyCode::Backspace => {
                    self.snap_note = Some("Wait for the store to finish (x stops it).".into());
                    Some(Vec::new())
                }
                _ => None,
            };
        }
        let listed: Vec<SnapshotMeta> = self.history().into_iter().map(|r| r.meta.clone()).collect();
        let sel = listed.get(self.snap_sel).cloned();
        let marked = |app: &mut App, ids: Vec<u64>, what: String| {
            let n = ids.len();
            app.snap_marks.extend(ids);
            app.snap_note = Some(match n {
                0 => format!("No {what} to mark."),
                _ => format!("Marked {} ({what}); {} marked in all. Del removes them, Esc clears the marks.", scans_text(n), app.snap_marks.len()),
            });
        };
        match key.code {
            KeyCode::Char(' ') => {
                let m = sel?;
                if !self.snap_marks.remove(&m.id) {
                    self.snap_marks.insert(m.id);
                }
                self.snap_note = None;
                self.snap_sel = (self.snap_sel + 1).min(listed.len().saturating_sub(1));
            }
            KeyCode::Char('n') => {
                let ids = listed.iter().filter(|m| matches!(m.status, Status::Failed | Status::Cancelled)).map(|m| m.id).collect();
                marked(self, ids, "cancelled and failed scans".into());
            }
            KeyCode::Char('o') => {
                let m = sel?;
                let g = diskdrift_app::prune::scan_group(&m);
                let ids = listed.iter().filter(|x| x.id < m.id && diskdrift_app::prune::scan_group(x) == g).map(|x| x.id).collect();
                marked(self, ids, format!("scans of {} older than #{}", m.root, m.id));
            }
            KeyCode::Char('p') => {
                let rules = self.settings.retention.value;
                if rules.is_off() {
                    self.snap_note = Some("No retention rules yet: set them in Settings (E), Keep: the last scans, one a week, one a month.".into());
                } else {
                    let ids = diskdrift_app::prune::retention_removals(&listed, rules, chrono::Local::now().fixed_offset());
                    marked(self, ids, format!("not kept by the rules: {}", rules.describe()));
                }
            }
            KeyCode::Esc if !self.snap_marks.is_empty() => {
                self.snap_marks.clear();
                self.snap_note = Some("Marks cleared.".into());
            }
            KeyCode::Delete | KeyCode::Backspace => {
                let ids: Vec<u64> = if self.snap_marks.is_empty() {
                    sel.map(|m| m.id).into_iter().collect()
                } else {
                    listed.iter().map(|m| m.id).filter(|id| self.snap_marks.contains(id)).collect()
                };
                if ids.is_empty() {
                    return Some(Vec::new());
                }
                if self.scan_running() {
                    self.snap_note = Some("A scan is running: remove scans when it has finished (x stops it).".into());
                    return Some(Vec::new());
                }
                match diskdrift_app::prune::propose(&self.settings, &diskdrift_app::prune::Selection::Ids(ids), None) {
                    Ok(p) => self.snap_remove = Some(p),
                    Err(e) => self.snap_note = Some(format!("Cannot remove them: {e}")),
                }
            }
            KeyCode::Char('z') => {
                if self.scan_running() {
                    self.snap_note = Some("A scan is running: compact the store when it has finished.".into());
                    return Some(Vec::new());
                }
                self.store_job = Some("compacting the store".into());
                self.snap_note = None;
                return Some(vec![Command::Compact]);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn disks_key(&mut self, key: KeyEvent) -> Option<Vec<Command>> {
        if let Some(plan) = self.disks.forget.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                return Some(match disks::forget(&self.settings, &plan.disk.id, true) {
                    Ok(done) => {
                        self.disks.note = Some(Ok(diskdrift_app::text::forget(&done, self.settings.size_units.value).trim_end().to_string()));
                        if self.disk_filter.as_ref().is_some_and(|f| f.ids.contains(&done.disk.id)) {
                            self.set_disk_filter(None);
                        }
                        self.disks.list = Job::Idle;
                        self.snapshots = Job::Running("loading snapshots…".into());
                        let mut cmds = self.load_disks();
                        cmds.push(Command::LoadSnapshots);
                        cmds
                    }
                    Err(e) => {
                        self.disks.note = Some(Err(format!("not forgotten: {e}")));
                        Vec::new()
                    }
                });
            }
            self.disks.note = Some(Ok("Nothing was removed.".into()));
            return Some(Vec::new());
        }
        let n = self.disks.list.done().map(|l| l.disks.len()).unwrap_or(0);
        let row = self.selected_disk().cloned();
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                self.disks.note = None;
                self.disks.sel = match key.code {
                    KeyCode::Up => self.disks.sel.saturating_sub(1),
                    KeyCode::PageUp => self.disks.sel.saturating_sub(10),
                    KeyCode::Down => self.disks.sel + 1,
                    KeyCode::PageDown => self.disks.sel + 10,
                    KeyCode::Home => 0,
                    _ => n,
                }
                .min(n.saturating_sub(1));
            }
            KeyCode::F(5) => return Some(self.load_disks()),
            // The selected disk's set (again: its next set), like Enter for one disk.
            KeyCode::Char('w') => {
                let r = row?;
                let sets = &r.disk.sets;
                if sets.is_empty() {
                    self.disks.note = Some(Err(format!("{} is in no set: + puts it in one.", r.disk.display_name())));
                    return Some(Vec::new());
                }
                let now = self.disk_filter.as_ref().and_then(|f| f.set.as_ref()).and_then(|s| sets.iter().position(|x| x == s));
                let set = sets[now.map(|i| (i + 1) % sets.len()).unwrap_or(0)].clone();
                let ids: Vec<String> = self.disks.list.done().map(|l| l.disks.iter().filter(|d| d.known && d.disk.sets.contains(&set)).map(|d| d.disk.id.clone()).collect()).unwrap_or_default();
                let n = ids.len();
                self.set_disk_filter(Some(DiskFilter { ids, name: set.clone(), set: Some(set.clone()) }));
                let mut cmds = self.open(Screen::Snapshots);
                self.snap_sel_newest();
                self.snap_note = Some(format!("set {set}: {n} {} (Find searches only them; * shows every disk)", if n == 1 { "disk" } else { "disks" }));
                cmds.retain(|c| *c != Command::LoadDisks);
                return Some(cmds);
            }
            KeyCode::Enter => {
                let r = row?;
                if r.snapshots == 0 {
                    self.disks.note = Some(Err(format!("{} has no scans in the store.", r.disk.display_name())));
                    return Some(Vec::new());
                }
                self.set_disk_filter(Some(DiskFilter::disk(&r.disk)));
                let mut cmds = self.open(Screen::Snapshots);
                self.snap_sel_newest();
                self.snap_note = None;
                cmds.retain(|c| *c != Command::LoadDisks);
                return Some(cmds);
            }
            KeyCode::Char('l') | KeyCode::Char('o') | KeyCode::Char('+') | KeyCode::Char('-') => {
                let r = row?;
                let (field, text) = match key.code {
                    KeyCode::Char('l') => (DiskField::Label, r.disk.label.clone().unwrap_or_default()),
                    KeyCode::Char('o') => (DiskField::Describe, r.disk.description.clone().unwrap_or_default()),
                    KeyCode::Char('+') => (DiskField::AddToSet, String::new()),
                    _ if r.disk.sets.is_empty() => {
                        self.disks.note = Some(Err(format!("{} is in no set.", r.disk.display_name())));
                        return Some(Vec::new());
                    }
                    _ => (DiskField::RemoveFromSet, r.disk.sets.last().cloned().unwrap_or_default()),
                };
                self.disks.note = None;
                self.disks.edit = Some((field, text));
            }
            KeyCode::Char('g') => {
                let r = row?;
                let Some(mount) = &r.mount_point else {
                    self.disks.note = Some(Err(format!("{} is not connected: connect it to scan it.", r.disk.display_name())));
                    return Some(Vec::new());
                };
                // The folder last scanned on the disk, if it is still there; else the whole volume.
                let last = self
                    .snapshots
                    .done()
                    .and_then(|l| l.snapshots.iter().rev().find(|s| s.meta.disk.as_deref() == Some(r.disk.id.as_str()) && s.meta.status.is_usable()))
                    .map(|s| s.meta.root.clone())
                    .filter(|p| std::path::Path::new(p).is_dir());
                let root = last.unwrap_or_else(|| mount.display().to_string());
                self.scan = ScanStage::Confirm { root, name: String::new() };
                self.screen = Screen::Scan;
                self.menu_index = MENU.iter().position(|s| *s == Screen::Scan).unwrap_or(0);
            }
            KeyCode::Char('f') if row.as_ref().is_some_and(|r| !r.known) => {
                self.disks.note = Some(Err("This disk is not in the store yet: there is nothing to forget.".into()));
            }
            KeyCode::Char('f') => {
                let r = row?;
                match disks::forget(&self.settings, &r.disk.id, false) {
                    Ok(plan) => self.disks.forget = Some(plan),
                    Err(e) => self.disks.note = Some(Err(e.to_string())),
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn disks_edit_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some((field, text)) = self.disks.edit.as_mut() else { return Vec::new() };
        match key.code {
            KeyCode::Esc => self.disks.edit = None,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) && text.chars().count() < 512 => text.push(c),
            KeyCode::Enter => {
                let (field, text) = (*field, text.clone());
                // A new volume is named by its mount point: the edit records it first.
                let Some(id) = self.selected_disk().map(|r| match (&r.mount_point, r.known) {
                    (Some(mp), false) => mp.display().to_string(),
                    _ => r.disk.id.clone(),
                }) else {
                    self.disks.edit = None;
                    return Vec::new();
                };
                let value = Some(text.as_str()).filter(|t| !t.trim().is_empty());
                let edit = match field {
                    DiskField::Label => Edit::Label(value),
                    DiskField::Describe => Edit::Describe(value),
                    DiskField::AddToSet => Edit::AddToSet(&text),
                    DiskField::RemoveFromSet => Edit::RemoveFromSet(&text),
                };
                match disks::edit(&self.settings, &id, edit) {
                    Ok(d) => {
                        self.disks.edit = None;
                        self.disks.note = Some(Ok(diskdrift_app::text::disk_entry(&d).trim_end().to_string()));
                        if let Some(f) = self.disk_filter.as_mut().filter(|f| f.set.is_none() && f.ids == [d.id.clone()]) {
                            f.name = d.display_name().to_string();
                        }
                        self.disks.list = Job::Idle;
                        self.snapshots = Job::Running("loading snapshots…".into());
                        let mut cmds = self.load_disks();
                        cmds.push(Command::LoadSnapshots);
                        return cmds;
                    }
                    // The text stays, so it can be corrected.
                    Err(e) => self.disks.note = Some(Err(e.to_string())),
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn request_quit(&mut self) -> Vec<Command> {
        if self.busy() {
            self.confirm_quit = true;
            Vec::new()
        } else {
            self.should_quit = true;
            vec![Command::CancelJobs, Command::Quit]
        }
    }

    pub fn handle_msg(&mut self, msg: Msg) -> Vec<Command> {
        match msg {
            Msg::Usage(r) => {
                self.usage = Job::Done(r);
                self.usage_at = Some(chrono::Local::now().format("%H:%M").to_string());
            }
            Msg::StoreProgress(text) => {
                if self.store_job.is_some() {
                    self.store_job = Some(text);
                }
            }
            Msg::Removed(r) => {
                self.store_job = None;
                self.snap_note = Some(match *r {
                    Ok(done) => {
                        if !done.cancelled {
                            self.snap_marks.clear();
                        }
                        diskdrift_app::text::prune::removed(&done, self.settings.size_units.value).trim_end().to_string()
                    }
                    Err(e) => format!("Not removed: {e}"),
                });
                return self.reload_store_views();
            }
            Msg::Compacted(r) => {
                self.store_job = None;
                self.snap_note = Some(match *r {
                    Ok(c) => diskdrift_app::text::prune::compaction(&c, self.settings.size_units.value).trim_end().to_string(),
                    Err(e) => format!("Compaction stopped: {e}. The store is valid as it is."),
                });
                return self.reload_store_views();
            }
            Msg::Snapshots(r) => {
                let before = self.snapshots.done().map(|l| l.snapshots.len());
                self.snapshots = Job::from(*r);
                // History opens on the newest scan with data, and moves to a new one when it appears.
                let n = self.snapshots.done().map(|l| l.snapshots.len()).unwrap_or(0);
                if before != Some(n) {
                    self.snap_sel_newest();
                }
                let auto = self.maybe_auto_scan();
                if !auto.is_empty() {
                    return auto;
                }
                let n = self.history().len();
                self.snap_sel = self.snap_sel.min(n.saturating_sub(1));
                if self.screen == Screen::Diff {
                    return self.diff_latest();
                }
                if self.screen == Screen::Top {
                    return self.top_latest();
                }
            }
            Msg::DoctorStep(step) => {
                if self.doctor.is_running() {
                    self.doctor_current = current_location(&step);
                    let text = step_text(&step, self.settings.size_units.value);
                    if !matches!(step, DoctorStep::Measured { .. }) {
                        self.doctor = Job::Running(format!("{text}…"));
                    }
                    self.doctor_log.push(LogLine::now(text));
                }
            }
            Msg::Doctor(r) => {
                if self.doctor.is_running() {
                    self.doctor = Job::Done(r);
                    if self.recover.is_running() {
                        self.recover = Job::Running("preparing suggestions (and recent growth from saved scans)…".into());
                    }
                }
            }
            Msg::ScanDone(r) => {
                if self.auto_scan_running() {
                    let secs = match &self.scan {
                        ScanStage::Running { started, .. } => started.elapsed().as_secs_f64(),
                        _ => 0.0,
                    };
                    let at = chrono::Local::now().format("%H:%M");
                    self.auto_status = Some(match (r.as_ref(), self.connect_scan.take()) {
                        (Ok(o), Some(disk)) if o.cancelled => format!("rescan of {disk} stopped at {at}; nothing kept"),
                        (Ok(o), Some(disk)) => {
                            let prev = self.previous_in_store(&o.snapshot);
                            let change = diskdrift_app::text::snapshots::tracked_change(&o.snapshot, prev.as_ref(), self.settings.size_units.value);
                            format!("{disk} rescanned at {at} in {secs:.0}s: {change}. DiskDrift no longer reads {disk}: you can eject it.")
                        }
                        (Err(e), Some(disk)) => format!("rescan of {disk} failed: {e}"),
                        (Ok(o), None) if o.cancelled => format!("background scan stopped at {at}; nothing kept"),
                        (Ok(o), None) => format!("background scan finished at {at} in {secs:.0}s: snapshot #{} ({})", o.snapshot.id, o.snapshot.status),
                        (Err(e), None) => format!("background scan failed: {e}"),
                    });
                    // The scan form stays ready for a scan of your own.
                    self.scan = initial_scan(&self.settings);
                } else {
                    self.scan = ScanStage::Finished(r.map(Box::new));
                }
                self.snapshots = Job::Running("loading snapshots…".into());
                // New data: previous comparisons and top lists are stale.
                self.diff.result = Job::Idle;
                self.top.result = Job::Idle;
                self.disks.list = Job::Running("reading disks…".into());
                return vec![Command::LoadSnapshots, Command::LoadUsage, Command::LoadDisks];
            }
            Msg::ScanDoubt(message) => {
                let (root, name) = self.scan_request.clone().unwrap_or_default();
                if self.auto_scan_running() {
                    self.connect_scan = None;
                    self.auto_status = Some(format!("background scan not started: {message} Scan it with S to answer."));
                    self.scan = initial_scan(&self.settings);
                } else {
                    self.scan = ScanStage::AskDisk { root, name, message };
                }
            }
            Msg::Volumes(now) => return self.volumes_changed(now),
            Msg::Connected { volume, action } => return self.disk_connected(volume, *action),
            Msg::Disks(r) => {
                self.disks.list = Job::from(*r);
                let n = self.disks.list.done().map(|l| l.disks.len()).unwrap_or(0);
                self.disks.sel = self.disks.sel.min(n.saturating_sub(1));
            }
            Msg::Diff(r) => {
                // A comparison overtaken by another choice of scans is dropped.
                let ids: Vec<u64> = self.usable().iter().map(|r| r.meta.id).collect();
                let current = (ids.get(self.diff.old).copied(), ids.get(self.diff.new).copied());
                if !matches!(&*r, Ok(d) if current != (Some(d.old.id), Some(d.new.id))) {
                    self.diff.result = Job::from(*r);
                }
            }
            Msg::Top(r) => {
                let current = self.usable().get(self.top.snapshot).map(|r| r.meta.id);
                if !matches!(&*r, Ok(t) if Some(t.snapshot.id) != current) {
                    self.top.result = Job::from(*r);
                }
            }
            Msg::Recover(r) => {
                self.recover = Job::from(*r);
                self.recover_ui.sel = 0;
            }
            Msg::AnalyzeStep { id, step } => {
                let u = self.settings.size_units.value;
                if let Some(v) = self.analysis.stack.last_mut().filter(|v| v.id == id) {
                    let text = match step {
                        AnalyzeStep::Listed { children, installed_apps } => {
                            format!("{children} items to measure; {installed_apps} installed apps indexed to match owners")
                        }
                        AnalyzeStep::Measured { name, allocated_bytes, entries, elapsed } => {
                            format!("  {name}: {}, {entries} entries, {:.1}s", format_size(allocated_bytes, u), elapsed.as_secs_f64())
                        }
                    };
                    v.log.push(LogLine::now(text));
                }
            }
            Msg::FindCurrent { id, result } if id == self.find.id => {
                self.find.current = Job::from(*result);
                self.find.sel = 0;
                // Nothing now: the history is searched by itself.
                if self.find.current.done().is_some_and(|c| c.total == 0 && c.store_exists) {
                    return self.start_find_history(false);
                }
            }
            Msg::FindProgress { id, done, total } if id == self.find.id => self.find.progress = Some((done, total)),
            Msg::FindHistory { id, result } if id == self.find.id => {
                self.find.history = Job::from(*result);
                self.find.progress = None;
            }
            Msg::FindCurrent { .. } | Msg::FindProgress { .. } | Msg::FindHistory { .. } => {}
            Msg::CarefulStarted(Ok(())) => {
                self.careful.on = Some(true);
                self.careful.log("careful mode on: external disks connected now are held back, not mounted".into());
            }
            Msg::CarefulStarted(Err(e)) => {
                self.careful.on = None;
                self.careful.log(format!("careful mode could not start: {e}"));
            }
            Msg::CarefulHeld(r) => match *r {
                Ok(h) if self.careful.is_on() => {
                    let u = self.settings.size_units.value;
                    let line = diskdrift_app::text::careful::held(&h, u, chrono::Local::now().timestamp());
                    self.careful.log(line.clone());
                    self.note_disk(format!("careful mode: {} held back, not mounted (C to scan it)", h.name()));
                    self.careful.held.retain(|x| x.bsd() != h.bsd());
                    self.careful.held.push(h);
                }
                Ok(_) => {}
                Err(e) => self.careful.log(format!("a disk was held back, but the store cannot be read: {e}")),
            },
            Msg::CarefulGone(bsd) => {
                if let Some(i) = self.careful.held.iter().position(|h| h.bsd() == bsd) {
                    let h = self.careful.held.remove(i);
                    self.careful.log(format!("{} is gone", h.name()));
                    self.careful.sel = self.careful.sel.min(self.careful.held.len().saturating_sub(1));
                }
            }
            Msg::CarefulDone { bsd, result } => {
                self.careful.running = None;
                let u = self.settings.size_units.value;
                match *result {
                    Ok(o) => {
                        for l in diskdrift_app::text::careful::outcome(&o, u).lines().filter(|l| !l.trim().is_empty()) {
                            self.careful.log(l.to_string());
                        }
                        if o.ejected.is_ok() {
                            self.careful.held.retain(|h| h.bsd() != bsd);
                            self.careful.sel = self.careful.sel.min(self.careful.held.len().saturating_sub(1));
                        }
                    }
                    Err(e) => self.careful.log(e),
                }
                // The new scan shows in History and My disks.
                self.snapshots = Job::Running("loading snapshots…".into());
                self.disks.list = Job::Idle;
                let mut cmds = vec![Command::LoadSnapshots];
                cmds.extend(self.load_disks());
                return cmds;
            }
            Msg::CarefulAction { bsd, result } => match result {
                Ok(Some(at)) => self.careful.log(format!("mounted read-only at {} (Finder shows it and cannot write to it); j ejects it", at.display())),
                Ok(None) => {
                    self.careful.log("ejected: safe to unplug".into());
                    self.careful.held.retain(|h| h.bsd() != bsd);
                    self.careful.sel = self.careful.sel.min(self.careful.held.len().saturating_sub(1));
                }
                Err(e) => self.careful.log(e),
            },
            Msg::Browse { id, result } => {
                if let Some(b) = self.browse.as_mut()
                    && let Some(level) = b.stack.iter_mut().find(|l| l.id == id)
                {
                    level.result = Job::from(*result);
                    b.previous = None;
                    if let Some(r) = level.result.done() {
                        b.root.get_or_insert_with(|| r.root());
                        let want = level.select.take();
                        level.sel = (0..r.len()).find(|&i| r.child(i).is_some_and(|c| Some(&c.1) == want.as_ref())).unwrap_or(0);
                    }
                }
            }
            Msg::Analyze { id, result } => {
                if let Some(v) = self.analysis.stack.last_mut().filter(|v| v.id == id) {
                    v.result = Job::from(*result);
                    v.sel = 0;
                }
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use diskdrift_app::config::{Overrides, resolve_with};
    use std::path::Path;

    pub fn settings() -> Effective {
        resolve_with(&Overrides::default(), Path::new("/nonexistent-home"), Path::new("/")).unwrap()
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn hotkeys_are_case_insensitive() {
        for (c, screen) in [('u', Screen::Usage), ('U', Screen::Usage), ('?', Screen::Help), ('h', Screen::Snapshots), ('T', Screen::Top), ('d', Screen::Diff), ('r', Screen::Recover), ('S', Screen::Scan)] {
            let mut app = App::new(settings());
            app.handle_key(key(KeyCode::Char(c)));
            assert_eq!(app.screen, screen, "key {c}");
        }
    }

    #[test]
    fn doctor_hotkey_starts_diagnosis_once() {
        let mut app = App::new(settings());
        assert_eq!(app.handle_key(key(KeyCode::Char('i'))), vec![Command::RunDoctor { live: false }]);
        assert!(app.doctor.is_running());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.handle_key(key(KeyCode::Char('I'))).is_empty(), "already running");
    }

    pub fn find_hit(path: &str, connected: bool) -> Hit {
        use diskdrift_app::search::{Place, ScanRef};
        Hit {
            path: diskdrift_app::core::pathser::NativePath(path.as_bytes().to_vec()),
            kind: Kind::File,
            size: 12_000,
            logical_size: 11_000,
            mtime: 1_700_000_000,
            scan: ScanRef { id: 12, name: "s12".into(), started_at: "2026-10-03T14:20:00+02:00".into() },
            place: Place { disk: Some("d1".into()), disk_name: "backupy-2014".into(), connected, mount_point: None, root: "/Volumes/backupy-2014".into() },
        }
    }

    pub fn find_gone(path: &str) -> Lifetime {
        let h = find_hit(path, false);
        let scan = |id: u64, at: &str| diskdrift_app::search::ScanRef { id, name: format!("s{id}"), started_at: at.into() };
        Lifetime {
            path: h.path,
            kind: Kind::File,
            size: 12_000,
            mtime: 1_700_000_000,
            place: h.place,
            last_seen: scan(12, "2026-10-03T14:20:00+02:00"),
            gone_in: Some(scan(13, "2026-10-10T09:05:00+02:00")),
            appeared_in: Some(scan(4, "2026-09-01T10:00:00+02:00")),
            oldest: scan(1, "2026-08-01T10:00:00+02:00"),
            changes: 0,
            uncertain: None,
            moved_to: None,
        }
    }

    #[test]
    fn find_searches_then_falls_back_to_the_history() {
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!((app.screen, app.find.editing), (Screen::Find, true));
        assert!(app.text_input_active());
        // Letters are typed, not hotkeys.
        typed(&mut app, "Ważna");
        assert_eq!(app.screen, Screen::Find);
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::CancelFind, Command::RunFind { id: 1, pattern: "Ważna".into(), disk: None, set: None }]);
        assert!(!app.find.editing && app.find.current.is_running());
        // Nothing now: the history search starts by itself.
        let cmds = app.handle_msg(Msg::FindCurrent { id: 1, result: Box::new(Ok(Current::for_test(Vec::new(), Vec::new(), 0))) });
        assert_eq!(cmds, vec![Command::RunFindHistory { id: 1, always: false }]);
        app.handle_msg(Msg::FindProgress { id: 1, done: 2, total: 7 });
        assert_eq!(app.find.progress, Some((2, 7)));
        // x stops the history search, not a scan.
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), vec![Command::CancelFind]);
        let h = History { gone: vec![find_gone("/Volumes/backupy-2014/Docs/wazna.txt")], gone_total: 1, pairs: 7, pairs_done: 7, ..Default::default() };
        app.handle_msg(Msg::FindHistory { id: 1, result: Box::new(Ok(h)) });
        assert!(matches!(app.find_selected(), Some(FindRow::Gone(_))));
        // A stale result of an older search is ignored.
        app.handle_msg(Msg::FindCurrent { id: 0, result: Box::new(Ok(Current::for_test(Vec::new(), vec![find_hit("/x", true)], 1))) });
        assert_eq!(app.find_rows(), 1);
        // Enter opens the last scan that has it, at its folder, with the file selected.
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 1, source: BrowseSource::Snapshot(12), path: Some("/Volumes/backupy-2014/Docs".into()) }]);
        let b = app.browse.as_ref().unwrap();
        assert_eq!(b.back, Screen::Find);
        assert!(b.note.as_deref().unwrap().contains("not in scan #13"), "{:?}", b.note);
    }

    #[test]
    fn find_results_keys() {
        let mut app = App::new(settings());
        app.open(Screen::Find);
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "nothing typed");
        assert!(app.find.note.is_some());
        typed(&mut app, "re:(");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "an invalid regex is refused before searching");
        assert!(app.find.note.as_deref().unwrap().contains("invalid"), "{:?}", app.find.note);
        app.handle_key(KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char('u')) });
        typed(&mut app, "plan");
        app.disk_filter = Some(DiskFilter { ids: vec!["d1".into()], name: "backupy-2014".into(), set: None });
        assert_eq!(app.handle_key(key(KeyCode::Enter))[1], Command::RunFind { id: 1, pattern: "plan".into(), disk: Some("d1".into()), set: None });
        let hits = vec![find_hit("/Volumes/backupy-2014/a/plan.doc", false), find_hit("/Volumes/backupy-2014/b/plan.doc", true)];
        assert!(app.handle_msg(Msg::FindCurrent { id: 1, result: Box::new(Ok(Current::for_test(Vec::new(), hits, 2))) }).is_empty(), "found: no history");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.find.sel, 1, "stops at the last row");
        // p searches the history too; letters that are not Find's open their screens.
        assert_eq!(app.handle_key(key(KeyCode::Char('p'))), vec![Command::RunFindHistory { id: 1, always: true }]);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        assert!(app.find.editing, "up from the first row goes to the field");
        app.handle_key(key(KeyCode::Tab));
        assert!(!app.find.editing);
        // A disconnected disk: the scan opens with a note.
        app.find.sel = 0;
        app.handle_key(key(KeyCode::Enter));
        let note = app.browse.as_ref().unwrap().note.clone().unwrap();
        assert!(note.contains("not connected"), "{note}");
        for _ in 0..3 {
            if app.screen == Screen::Browse {
                app.handle_key(key(KeyCode::Esc));
            }
        }
        assert_eq!((app.screen, app.find.editing), (Screen::Find, false), "Esc from Browse returns to the results");
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.screen, Screen::Disks);
    }

    pub fn held(bsd: &str, name: &str, doubt: Option<&str>) -> Held {
        use diskdrift_app::platform::{DeviceInfo, DiskInfo, HeldVolume};
        Held {
            volume: HeldVolume {
                bsd_name: bsd.into(),
                writable: Some(true),
                info: DiskInfo {
                    mount_point: PathBuf::new(),
                    fs_type: "exfat".into(),
                    volume_name: Some(name.into()),
                    volume_uuid: Some("V1".into()),
                    capacity_bytes: Some(64_000_000_000),
                    bsd_name: Some(bsd.into()),
                    device: DeviceInfo { bus: Some("USB".into()), model: Some("Old Stick".into()), ..Default::default() },
                },
            },
            disk: None,
            doubt: doubt.map(String::from),
            last_scan: None,
        }
    }

    #[test]
    fn careful_mode_holds_disks_scans_on_request_and_asks_about_doubtful_ones() {
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('C')));
        assert_eq!(app.screen, Screen::Careful);
        assert!(app.handle_key(key(KeyCode::Char('g'))).is_empty(), "off: nothing to scan");
        assert_eq!(app.handle_key(key(KeyCode::Char('o'))), vec![Command::Careful(true)]);
        app.handle_msg(Msg::CarefulStarted(Ok(())));
        assert!(app.careful.is_on());
        app.handle_msg(Msg::CarefulHeld(Box::new(Ok(held("disk9s1", "OLD", None)))));
        app.handle_msg(Msg::CarefulHeld(Box::new(Ok(held("disk10s1", "COPY", Some("a copy of OLD?"))))));
        assert_eq!(app.careful.held.len(), 2);
        assert!(app.disk_event.as_ref().unwrap().text.contains("COPY held back"));
        // A doubtful disk asks first; any other key leaves it.
        app.handle_key(key(KeyCode::Down));
        assert!(app.handle_key(key(KeyCode::Char('g'))).is_empty());
        assert!(app.careful.ask.is_some());
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), vec![Command::CarefulScan { bsd: "disk10s1".into(), choice: Some(DiskChoice::New) }]);
        assert!(app.needs_ticks());
        // One at a time; x stops it; o cannot turn it off meanwhile.
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Char('g')));
        assert!(app.careful.log.last().unwrap().text.contains("one at a time"));
        assert!(!app.handle_key(key(KeyCode::Char('o'))).contains(&Command::Careful(false)));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), vec![Command::CancelCareful]);
        let cmds = app.handle_msg(Msg::CarefulDone { bsd: "disk10s1".into(), result: Box::new(Err("stopped".into())) });
        assert!(cmds.contains(&Command::LoadSnapshots));
        assert!(app.careful.running.is_none());
        // m and j act on the selected disk; ejecting it removes it.
        assert_eq!(app.handle_key(key(KeyCode::Char('m'))), vec![Command::CarefulMount { bsd: "disk9s1".into() }]);
        assert_eq!(app.handle_key(key(KeyCode::Char('j'))), vec![Command::CarefulEject { bsd: "disk9s1".into() }]);
        app.handle_msg(Msg::CarefulAction { bsd: "disk9s1".into(), result: Ok(None) });
        assert_eq!(app.careful.held.len(), 1);
        app.handle_msg(Msg::CarefulGone("disk10s1".into()));
        assert!(app.careful.held.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Char('o'))), vec![Command::Careful(false)]);
        assert!(!app.careful.is_on());
    }

    #[test]
    fn a_known_disk_is_not_rescanned_on_connect_in_careful_mode() {
        let mut app = App::new(settings());
        app.careful.on = Some(true);
        let v = Volume {
            identity: diskdrift_app::storage::Identity::new(diskdrift_app::storage::Strength::Uuid, "U"),
            mount_point: "/Volumes/OLD".into(),
            name: Some("OLD".into()),
            fs_type: "hfs".into(),
            capacity_bytes: None,
        };
        let disk = diskdrift_app::storage::Disk {
            id: "d1".into(),
            identity: v.identity.clone(),
            label: Some("old".into()),
            description: None,
            sets: Vec::new(),
            volume: Default::default(),
            device: None,
            first_seen: String::new(),
            last_seen: String::new(),
            last_mount_point: None,
        };
        let action = ConnectAction::Rescan { disk, root: "/Volumes/OLD".into(), last_scan: "2026-01-01T00:00:00+01:00".into() };
        let cmds = app.handle_msg(Msg::Connected { volume: v, action: Box::new(Ok(action)) });
        assert!(cmds.is_empty(), "{cmds:?}");
        assert!(app.disk_event.as_ref().unwrap().text.contains("careful mode is on"));
    }

    #[test]
    fn menu_grid_navigation() {
        let mut app = App::new(settings());
        assert_eq!(MENU[app.menu_index], Screen::Scan);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(MENU[app.menu_index], Screen::Snapshots);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(MENU[app.menu_index], Screen::Snapshots, "stays in the right column");
        // Scan | History, My disks | Find, Compare | Largest, Doctor | Recover, Usage | Settings, Careful | Help.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(MENU[app.menu_index], Screen::Find);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(MENU[app.menu_index], Screen::Recover);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(MENU[app.menu_index], Screen::Settings);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(MENU[app.menu_index], Screen::Help);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(MENU[app.menu_index], Screen::Help, "no row below");
        app.handle_key(key(KeyCode::Left));
        assert_eq!(MENU[app.menu_index], Screen::Careful);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(MENU[app.menu_index], Screen::Doctor);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(MENU[app.menu_index], Screen::Top);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Recover);
    }

    #[test]
    fn esc_on_dashboard_asks_to_quit_and_q_quits() {
        let mut app = App::new(settings());
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.confirm_exit && !app.should_quit);
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty(), "Esc again stays");
        assert!(!app.confirm_exit && !app.should_quit);
        assert_eq!(app.screen, Screen::Dashboard);
        // Enter quits, stopping whatever runs, a scan included.
        app.scan = ScanStage::Running { progress: Arc::default(), started: Instant::now(), root: "/".into(), auto: false, expected: None };
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Command::CancelJobs, Command::Quit]);
        assert!(app.should_quit);
        let mut app = App::new(settings());
        app.handle_key(KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char('u')) });
        assert_eq!(app.screen, Screen::Dashboard, "Ctrl+U is not the U hotkey");
        assert!(app.handle_key(key(KeyCode::Char('Q'))).contains(&Command::Quit));
        assert!(app.should_quit);
    }

    #[test]
    fn scan_form_takes_text_and_confirms() {
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.screen, Screen::Scan);
        assert!(app.text_input_active());
        // Clear the default root and type a new one; letters must not trigger hotkeys.
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Backspace));
        }
        typed(&mut app, "/nonexistent-diskdrift-q");
        assert_eq!(app.screen, Screen::Scan, "q while typing is text, not quit");
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Tab));
        typed(&mut app, "base");
        // A folder that does not exist is caught in the form, before any confirmation.
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.scan, ScanStage::Setup { field: ScanField::Root, error: Some(e), .. } if e.contains("does not exist")));
        app.handle_key(KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char('u')) });
        assert!(matches!(&app.scan, ScanStage::Setup { root, error: None, .. } if root.is_empty()), "Ctrl+U clears; typing clears the error");
        typed(&mut app, "/");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.scan, ScanStage::Confirm { root, name } if root == "/" && name == "base"));
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::StartScan { root: "/".into(), name: Some("base".into()), background: false, disk: None }]);
        assert!(app.scan_running());
    }

    #[test]
    fn quitting_or_cancelling_a_running_scan_asks_first() {
        let mut app = App::new(settings());
        app.screen = Screen::Scan;
        app.scan = ScanStage::Running { progress: Arc::default(), started: Instant::now(), root: "/".into(), auto: false, expected: None };
        assert!(app.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert!(app.confirm_quit && !app.should_quit);
        app.handle_key(key(KeyCode::Char('n')));
        assert!(!app.confirm_quit && !app.should_quit);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.confirm_cancel);
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), vec![Command::CancelScan]);
        // ← leaves the scan running in the background; x (anywhere) asks to stop it.
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.scan_running());
        assert!(app.handle_key(key(KeyCode::Char('x'))).is_empty());
        assert!(app.confirm_cancel);
        assert!(app.handle_key(key(KeyCode::Char('n'))).is_empty(), "no keeps it running");
        assert!(app.scan_running());
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), vec![Command::CancelJobs, Command::Quit]);
        assert!(app.should_quit);
    }

    #[test]
    fn quitting_while_doctor_runs_does_not_block() {
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('i')));
        assert!(app.handle_key(key(KeyCode::Char('q'))).contains(&Command::Quit));
    }

    pub fn recover_report(dir: &Path) -> RecoverReport {
        let c = |rank: usize, path: Option<String>| diskdrift_app::investigate::Candidate {
            rank,
            label: format!("candidate {rank}"),
            path,
            observed_bytes: Some(1_000_000_000),
            reclaimable: "up to 1 GB".into(),
            risk: "medium".into(),
            confidence: "observed".into(),
            how: "look inside".into(),
            skipped: false,
            growth: None,
            within: None,
        };
        RecoverReport {
            candidates: vec![c(1, Some(dir.display().to_string())), c(2, None)],
            notes: Vec::new(),
            files_modified: false,
        }
    }

    pub fn analysis(dir: &Path) -> DirAnalysis {
        let item = |name: &str, is_dir: bool| diskdrift_app::analyze::AnalyzeItem {
            name: name.into(),
            path: dir.join(name).display().to_string(),
            is_dir,
            status: MeasureStatus::Complete,
            allocated_bytes: 5_000,
            logical_bytes: 5_000,
            entries: 3,
            newest_mtime: Some(1_700_000_000),
            assessment: diskdrift_app::diagnostics::classify::Assessment {
                what: "cache".into(),
                risk: Some(diskdrift_app::diagnostics::suspects::Risk::Low),
                advice: Some("Regenerated.".into()),
                owner: None,
                no_installed_owner: true,
            },
        };
        DirAnalysis {
            path: dir.display().to_string(),
            context: None,
            status: MeasureStatus::Complete,
            allocated_bytes: 10_000,
            logical_bytes: 10_000,
            entries: 7,
            items: vec![item("sub", true), item("file.dmg", false)],
            more_items: 0,
            more_allocated_bytes: 0,
            by_verdict: Vec::new(),
            installed_apps: 3,
            analyzed_at: 1_760_000_000,
            elapsed_ms: 12,
            sizes_from: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn growth_suggestions_open_the_changes_and_doctor_enter_is_not_a_rerun() {
        let dir = std::env::temp_dir();
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('r')));
        let mut report = recover_report(&dir);
        report.candidates[0].growth = Some((2, 4));
        app.handle_msg(Msg::Recover(Box::new(Ok(report))));
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 1, source: BrowseSource::Diff { old: 2, new: 4 }, path: Some(dir.clone()) }]);
        assert_eq!(app.screen, Screen::Browse);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Recover);
        let cmds = app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(cmds, vec![Command::CancelAnalyze, Command::RunAnalyze { id: 1, path: dir, live: false }], "a analyses it as it is now");
        // Doctor: Enter goes to the suggestions; only F5 measures again.
        app.doctor = Job::Failed("measured before".into());
        app.screen = Screen::Doctor;
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.screen, Screen::Recover);
        app.screen = Screen::Doctor;
        assert_eq!(app.handle_key(key(KeyCode::F(5))), vec![Command::RunDoctor { live: true }]);
    }

    #[test]
    fn recover_logs_steps_and_opens_folder_analyses() {
        let dir = std::env::temp_dir();
        let mut app = App::new(settings());
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), vec![Command::RunDoctor { live: false }], "one measurement for Doctor and Recover");
        assert!(app.doctor.is_running() && app.recover.is_running());
        app.handle_msg(Msg::DoctorStep(DoctorStep::Measuring { label: "Caches".into(), index: 0, total: 3 }));
        assert_eq!(app.doctor_log.len(), 2, "start line plus the step");
        assert!(app.doctor_log[1].text.contains("measuring Caches (1/3)"));
        assert_eq!(app.doctor_log[1].at.len(), 8, "HH:MM:SS");
        app.handle_msg(Msg::Recover(Box::new(Ok(recover_report(&dir)))));

        // Cursor moves over candidates; Enter on one without a folder does nothing.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.recover_ui.sel, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.recover_ui.sel, 1, "stops at the last candidate");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        app.handle_key(key(KeyCode::Up));
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::CancelAnalyze, Command::RunAnalyze { id: 1, path: dir.clone(), live: false }]);
        assert_eq!(app.analysis.stack.len(), 1);

        // A result of another run is ignored; the matching one is shown.
        app.handle_msg(Msg::Analyze { id: 99, result: Box::new(Ok(analysis(&dir))) });
        assert!(app.analysis.stack[0].result.is_running());
        app.handle_msg(Msg::AnalyzeStep {
            id: 1,
            step: AnalyzeStep::Measured { name: "sub".into(), allocated_bytes: 5_000, entries: 3, elapsed: std::time::Duration::from_millis(5) },
        });
        assert!(app.analysis.stack[0].log.last().unwrap().text.contains("sub: 5.0 kB"));
        app.handle_msg(Msg::Analyze { id: 1, result: Box::new(Ok(analysis(&dir))) });
        assert!(app.analysis.stack[0].result.done().is_some());

        // Enter opens a subfolder; a file does not; Esc goes back one level at a time.
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::CancelAnalyze, Command::RunAnalyze { id: 2, path: dir.join("sub"), live: false }]);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), vec![Command::CancelAnalyze], "leaving a running analysis cancels it");
        app.handle_key(key(KeyCode::Down));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "file.dmg is not a folder");
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.analysis.stack.is_empty());
        assert_eq!(app.screen, Screen::Recover);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn doctor_and_recover_share_one_measurement() {
        let mut app = App::new(settings());
        assert_eq!(app.handle_key(key(KeyCode::Char('i'))), vec![Command::RunDoctor { live: false }]);
        assert!(app.handle_key(key(KeyCode::Char('r'))).is_empty(), "Recover waits for the running measurement");
        assert!(app.recover.is_running());
        assert!(app.handle_key(key(KeyCode::Tab)).is_empty());
        assert_eq!(app.screen, Screen::Doctor, "Tab switches between diagnosis and suggestions");
        app.handle_msg(Msg::Recover(Box::new(Ok(recover_report(Path::new("/tmp"))))));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Recover);
        assert!(app.recover.done().is_some());
    }

    #[test]
    fn n_skips_the_location_being_measured() {
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.handle_key(key(KeyCode::Char('n'))).is_empty(), "nothing being measured yet");
        app.handle_msg(Msg::DoctorStep(DoctorStep::Measuring { label: "Application Support".into(), index: 11, total: 32 }));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), vec![Command::SkipDoctorStep]);
        assert!(app.doctor_log.last().unwrap().text.contains("skipping Application Support"));
        assert!(app.handle_key(key(KeyCode::Char('n'))).is_empty(), "one skip per location");
        assert_eq!(app.screen, Screen::Recover);

        app.handle_key(key(KeyCode::Char('i')));
        app.handle_msg(Msg::DoctorStep(DoctorStep::Measuring { label: "Caches".into(), index: 0, total: 32 }));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), vec![Command::SkipDoctorStep]);
    }

    pub fn browse_listing(root: &str, path: &str, names: &[(&str, bool)]) -> BrowseData {
        use diskdrift_app::core::browse::{Child, Listing};
        let child = |p: String, dir: bool| Child {
            path: p.into_bytes().into(),
            kind: if dir { diskdrift_app::core::record::Kind::Dir } else { diskdrift_app::core::record::Kind::File },
            allocated_bytes: 1000,
            logical_bytes: 1000,
            entries: 1,
            newest_mtime: 1_761_000_000,
            incomplete: false,
        };
        let snapshot = diskdrift_app::storage::SnapshotMeta {
            format_version: 1,
            id: 7,
            name: "base".into(),
            root: root.into(),
            disk: None,
            root_in_volume: None,
            status: diskdrift_app::storage::Status::Completed,
            started_at: "2026-10-21T10:00:00+02:00".into(),
            finished_at: None,
            volume: Default::default(),
            options: Default::default(),
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: "0".into(),
        };
        BrowseData::Snapshot(Box::new(BrowseResult {
            snapshot,
            path: path.into(),
            listing: Listing {
                dir: Some(Child { allocated_bytes: 1000 * names.len() as u64, ..child(path.into(), true) }),
                items: names.iter().map(|(n, d)| child(format!("{path}/{n}"), *d)).collect(),
                more_items: 0,
                more_allocated_bytes: 0,
            },
        }))
    }

    #[test]
    fn browse_drills_down_and_back_up_to_the_root() {
        let mut app = App::new(settings());
        app.screen = Screen::Top;
        let cmds = app.open_browse(BrowseSource::Snapshot(7), Some("/r/a".into()), Some("y".into()));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 1, source: BrowseSource::Snapshot(7), path: Some("/r/a".into()) }]);
        assert_eq!(app.screen, Screen::Browse);
        app.handle_msg(Msg::Browse { id: 1, result: Box::new(Ok(browse_listing("/r", "/r/a", &[("x", true), ("y", false)]))) });
        assert_eq!(app.browse.as_ref().unwrap().stack[0].sel, 1, "cursor on the file it was opened for");

        // A file does not open; a folder does, as a new level.
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        app.handle_key(key(KeyCode::Up));
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 2, source: BrowseSource::Snapshot(7), path: Some("/r/a/x".into()) }]);
        app.handle_msg(Msg::Browse { id: 2, result: Box::new(Ok(browse_listing("/r", "/r/a/x", &[]))) });

        // Esc pops the level back (no reload), then goes above where it was opened.
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(app.browse.as_ref().unwrap().stack.len(), 1);
        let cmds = app.handle_key(key(KeyCode::Left));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 3, source: BrowseSource::Snapshot(7), path: Some("/r".into()) }]);
        app.handle_msg(Msg::Browse { id: 3, result: Box::new(Ok(browse_listing("/r", "/r", &[("0", true), ("a", true)]))) });
        assert_eq!(app.browse.as_ref().unwrap().stack[0].sel, 1, "cursor on the folder we came from");
        app.handle_msg(Msg::Browse { id: 2, result: Box::new(Ok(browse_listing("/r", "/r/a/x", &[]))) });

        // At the root Esc returns to where the browser was opened from.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Top);
        assert!(app.browse.is_none());
    }

    #[test]
    fn a_in_browse_analyses_the_selected_folder_and_esc_returns() {
        let mut app = App::new(settings());
        app.screen = Screen::Snapshots;
        app.open_browse(BrowseSource::Snapshot(7), None, None);
        app.handle_msg(Msg::Browse { id: 1, result: Box::new(Ok(browse_listing("/r", "/r", &[("Library", true), ("big.iso", false)]))) });
        let cmds = app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(cmds, vec![Command::CancelAnalyze, Command::RunAnalyze { id: 1, path: "/r/Library".into(), live: false }]);
        assert_eq!(app.screen, Screen::Analyze);
        app.handle_msg(Msg::Analyze { id: 1, result: Box::new(Ok(analysis(Path::new("/r/Library")))) });
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Browse, "back to the browser, its state kept");
        assert_eq!(app.browse.as_ref().unwrap().stack.len(), 1);

        // On a file, the folder shown is analysed.
        app.handle_key(key(KeyCode::Down));
        let cmds = app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(cmds, vec![Command::CancelAnalyze, Command::RunAnalyze { id: 2, path: "/r".into(), live: false }]);
        assert_eq!(app.analysis.stack.len(), 1, "a fresh analysis stack");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Snapshots);
    }

    #[test]
    fn compare_opens_on_the_latest_pair_and_picks_scans_from_a_list() {
        let mut app = App::new(settings());
        let meta = |id: u64, root: &str| {
            let mut v = match browse_listing(root, root, &[]) {
                BrowseData::Snapshot(b) => b.snapshot,
                BrowseData::Diff(_) => unreachable!(),
            };
            v.id = id;
            diskdrift_app::snapshots::SnapshotRow { meta: v, size_on_disk: 0, disk_name: None }
        };
        app.snapshots = Job::Done(Box::new(SnapshotList {
            store_path: "/s".into(),
            store_exists: true,
            store_bytes: 0,
            snapshots: vec![meta(1, "/a"), meta(2, "/b"), meta(3, "/a"), meta(4, "/a")],
            disk_count: 1,
            disk: None,
        }));
        let run = |old, new| vec![Command::RunDiff { old, new, sort: DiffSort::Growth, min_size: 0 }];
        // Opening compares the newest scan with the previous one, no Enter needed.
        assert_eq!(app.handle_key(key(KeyCode::Char('d'))), run(3, 4));
        // ↑ at the top of the results reaches the scans line; Enter opens the list.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.diff.focus, DiffFocus::Header);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!((app.diff.focus, app.diff.cursor), (DiffFocus::PickOld, 2));
        assert!(!app.diff_choosable(1), "#2 has no later scan of /b");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.diff.cursor, 0, "greyed #2 is skipped");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!((app.diff.focus, app.diff.cursor), (DiffFocus::PickNew, 3), "the later scan stays");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.diff.cursor, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.diff.cursor, 2, "nothing earlier fits");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!((app.diff.focus, app.diff.cursor), (DiffFocus::PickOld, 0), "Esc goes back one step");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), run(1, 4));
        assert_eq!(app.diff.focus, DiffFocus::Results);
        // p opens the list too; Esc returns to the comparison on screen.
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.diff.focus, DiffFocus::PickOld);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!((app.screen, app.diff.focus), (Screen::Diff, DiffFocus::Results));
        // The user's choice is kept when coming back.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.handle_key(key(KeyCode::Char('d'))).is_empty());
        assert_eq!((app.diff.old, app.diff.new), (0, 3));
    }

    fn rows(list: &[(u64, &str, Status)]) -> SnapshotList {
        let snapshots = list
            .iter()
            .map(|&(id, root, status)| {
                let mut v = match browse_listing(root, root, &[]) {
                    BrowseData::Snapshot(b) => b.snapshot,
                    BrowseData::Diff(_) => unreachable!(),
                };
                (v.id, v.status) = (id, status);
                diskdrift_app::snapshots::SnapshotRow { meta: v, size_on_disk: 0, disk_name: None }
            })
            .collect();
        SnapshotList { store_path: "/s".into(), store_exists: true, store_bytes: 0, snapshots, disk_count: 1, disk: None }
    }

    #[test]
    fn browse_switches_to_the_changes_here_and_says_why_a_file_does_not_open() {
        let mut app = App::new(settings());
        app.auto_checked = true;
        app.handle_msg(Msg::Snapshots(Box::new(Ok(rows(&[(1, "/r", Status::Completed), (2, "/r", Status::Completed), (3, "/q", Status::Completed)])))));
        app.screen = Screen::Snapshots;
        let cmds = app.open_browse(BrowseSource::Snapshot(2), Some("/r/a".into()), None);
        let Command::RunBrowse { id, .. } = cmds[0] else { panic!("{cmds:?}") };
        app.handle_msg(Msg::Browse { id, result: Box::new(Ok(browse_listing("/r", "/r/a", &[("dir", true), ("file.txt", false)]))) });
        app.handle_key(key(KeyCode::Down));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.browse.as_ref().unwrap().note.as_deref().unwrap().contains("file.txt is a file"));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.browse.as_ref().unwrap().note, None, "the next key clears the note");
        // c: the same folder, what changed there since the previous scan of it.
        let cmds = app.handle_key(key(KeyCode::Char('c')));
        assert!(
            matches!(&cmds[..], [Command::RunBrowse { source: BrowseSource::Diff { old: 1, new: 2 }, path: Some(p), .. }] if p == Path::new("/r/a")),
            "{cmds:?}"
        );
        // c again: back to the scan; Esc at the top still returns to History.
        let cmds = app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(&cmds[..], [Command::RunBrowse { source: BrowseSource::Snapshot(2), .. }]), "{cmds:?}");
        assert_eq!(app.browse.as_ref().unwrap().back, Screen::Snapshots);
        // A scan with no earlier one of the same folder says so.
        app.open_browse(BrowseSource::Snapshot(3), None, None);
        assert!(app.handle_key(key(KeyCode::Char('c'))).is_empty());
        assert!(app.browse.as_ref().unwrap().note.as_deref().unwrap().contains("No earlier scan"));
    }

    #[test]
    fn usage_reads_again_on_f5_only() {
        let mut app = App::new(settings());
        assert_eq!(app.open(Screen::Usage), vec![Command::LoadUsage]);
        assert!(app.handle_key(key(KeyCode::F(5))).is_empty(), "already reading");
        app.handle_msg(Msg::Usage(Box::new(UsageReport { containers: Vec::new(), other_volumes: Vec::new(), apfs_error: None })));
        assert!(app.usage_at.is_some());
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.handle_key(key(KeyCode::F(5))), vec![Command::LoadUsage]);
    }

    #[test]
    fn a_finished_scan_offers_its_changes_and_a_new_scan_of_the_same_folder() {
        let mut app = App::new(settings());
        app.auto_checked = true;
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.handle_key(key(KeyCode::Enter)).is_empty(), "the default root / starts");
        let both = rows(&[(1, "/", Status::Completed), (2, "/", Status::Completed)]);
        let outcome = ScanOutcome {
            snapshot: both.snapshots[1].meta.clone(),
            cancelled: false,
            store_path: "/s".into(),
            store_bytes: 0,
            rebase: diskdrift_app::storage::Rebase::Nothing,
            rebase_error: None,
            disk: None,
        };
        app.handle_msg(Msg::ScanDone(Box::new(Ok(outcome))));
        app.handle_msg(Msg::Snapshots(Box::new(Ok(both))));
        let cmds = app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(cmds.as_slice(), [Command::RunBrowse { source: BrowseSource::Diff { old: 1, new: 2 }, .. }]), "{cmds:?}");
        // Leaving the result: S next time is a new scan of the same folder.
        app.screen = Screen::Scan;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(matches!(&app.scan, ScanStage::Setup { root, error: None, .. } if root == "/"));
        // A failed scan goes back to the form, saying why.
        app.screen = Screen::Scan;
        app.scan = ScanStage::Finished(Err("boom".into()));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.scan, ScanStage::Setup { root, error: Some(e), .. } if root == "/" && e == "boom"));
    }

    #[test]
    fn history_starts_on_the_newest_usable_scan_and_says_why_nothing_opens() {
        let mut app = App::new(settings());
        app.screen = Screen::Snapshots;
        let list = rows(&[(1, "/a", Status::Completed), (2, "/b", Status::Completed), (3, "/a", Status::Cancelled)]);
        app.handle_msg(Msg::Snapshots(Box::new(Ok(list))));
        assert_eq!(app.snap_sel, 1, "#3 was cancelled: start on #2");
        assert!(app.handle_key(key(KeyCode::Char('c'))).is_empty());
        assert!(app.snap_note.as_deref().unwrap().contains("No earlier scan of /b"));
        app.handle_key(key(KeyCode::Down));
        assert!(app.snap_note.is_none(), "moving clears the note");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.snap_note.as_deref().unwrap().contains("#3 was cancelled"));
        assert!(app.handle_key(key(KeyCode::Char('t'))).is_empty());
        assert_eq!(app.screen, Screen::Snapshots);
    }

    #[test]
    fn largest_files_tabs_on_arrows_and_picks_another_scan_from_a_list() {
        let mut app = App::new(settings());
        app.snapshots = Job::Done(Box::new(rows(&[(1, "/a", Status::Completed), (2, "/a", Status::Partial)])));
        let run = |snapshot| vec![Command::RunTop { snapshot, metric: SizeMetric::Allocated }];
        assert_eq!(app.handle_key(key(KeyCode::Char('t'))), run(2), "opens on the newest scan");
        assert!(app.handle_key(key(KeyCode::Right)).is_empty());
        assert_eq!(app.top.view, TopView::Dirs, "←→ switch files/folders, like the Compare tabs");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.top.focus, TopFocus::Header);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!((app.top.focus, app.top.cursor), (TopFocus::Pick, 1));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), run(1));
        assert_eq!(app.top.focus, TopFocus::List);
        app.handle_key(key(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!((app.screen, app.top.focus), (Screen::Top, TopFocus::List), "Esc leaves the list, not the screen");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.handle_key(key(KeyCode::Char('t'))).is_empty(), "the chosen scan is kept");
    }

    #[test]
    fn history_enter_browses_and_c_compares_with_previous() {
        let mut app = App::new(settings());
        let meta = |id: u64| {
            let mut v = match browse_listing("/r", "/r", &[]) {
                BrowseData::Snapshot(b) => b.snapshot,
                BrowseData::Diff(_) => unreachable!(),
            };
            v.id = id;
            diskdrift_app::snapshots::SnapshotRow { meta: v, size_on_disk: 0, disk_name: None }
        };
        app.snapshots = Job::Done(Box::new(SnapshotList {
            store_path: "/s".into(),
            store_exists: true,
            store_bytes: 0,
            snapshots: vec![meta(1), meta(2)],
            disk_count: 1,
            disk: None,
        }));
        app.screen = Screen::Snapshots;
        assert!(app.handle_key(key(KeyCode::Char('c'))).is_empty(), "no scan before the first");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Command::RunBrowse { id: 1, source: BrowseSource::Snapshot(1), path: None }]);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Snapshots);
        app.handle_key(key(KeyCode::Down));
        let cmds = app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(cmds, vec![Command::RunBrowse { id: 2, source: BrowseSource::Diff { old: 1, new: 2 }, path: None }]);
    }

    #[test]
    fn settings_toggle_edit_and_save_the_config_file() {
        let t = diskdrift_core::testutil::TestDir::new("tui-settings");
        let cfg = t.path().join("config.json");
        let ov = diskdrift_app::config::Overrides { config: Some(cfg.clone()), db: Some(t.path().join("store")), ..Default::default() };
        std::fs::write(&cfg, r#"{"schemaVersion":1}"#).unwrap();
        let mut app = App::new(resolve_with(&ov, t.path(), Path::new("/")).unwrap());
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.screen, Screen::Settings);
        let at = |k: &str| FIELDS.iter().position(|f| f.key == k).unwrap();

        // A choice cycles and is written at once.
        while app.settings_ui.sel < at("output.sizeUnits") {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.settings.size_units.value, SizeUnits::Binary, "applied to this session");
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("binary"));

        // Text is typed (letters are text, not hotkeys) and saved on Enter.
        app.settings_ui.sel = at("scan.exclude");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.text_input_active());
        typed(&mut app, "node_modules,q");
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.should_quit && !app.text_input_active());
        assert_eq!(app.settings.exclude.value, ["node_modules", "q"]);

        // Invalid input is refused and the file is left as it was.
        app.settings_ui.sel = at("scan.threads");
        app.handle_key(key(KeyCode::Enter));
        typed(&mut app, "x");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.settings_ui.message, Some(Err(m)) if m.contains("whole number")));
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("\"threads\": 0,"), "file unchanged");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn background_scan_starts_once_when_the_last_scan_is_old_and_x_stops_it() {
        let mut app = App::new(settings());
        let mut old = match browse_listing("/", "/", &[]) {
            BrowseData::Snapshot(b) => b.snapshot,
            BrowseData::Diff(_) => unreachable!(),
        };
        old.started_at = "2020-01-01T00:00:00+00:00".into();
        let list = || SnapshotList {
            store_path: "/s".into(),
            store_exists: true,
            store_bytes: 0,
            snapshots: vec![diskdrift_app::snapshots::SnapshotRow { meta: old.clone(), size_on_disk: 0, disk_name: None }],
            disk_count: 1,
            disk: None,
        };
        let cmds = app.handle_msg(Msg::Snapshots(Box::new(Ok(list()))));
        assert!(
            matches!(&cmds[..], [Command::StartScan { root, background: true, name: Some(n), disk: None }] if root == "/" && n.starts_with("auto-")),
            "{cmds:?}"
        );
        assert!(app.auto_scan_running() && !app.busy());
        assert!(app.handle_msg(Msg::Snapshots(Box::new(Ok(list())))).is_empty(), "only once per session");

        assert!(app.handle_key(key(KeyCode::Char('x'))).is_empty(), "asks first");
        assert!(app.confirm_cancel);
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), vec![Command::CancelScan]);
        assert!(app.auto_status.as_deref().unwrap().contains("stopping"));
        let mut cancelled = ScanOutcome {
            snapshot: old.clone(),
            cancelled: true,
            store_path: "/s".into(),
            store_bytes: 0,
            rebase: diskdrift_app::storage::Rebase::Nothing,
            rebase_error: None,
            disk: None,
        };
        cancelled.snapshot.status = diskdrift_app::storage::Status::Cancelled;
        app.handle_msg(Msg::ScanDone(Box::new(Ok(cancelled))));
        assert!(app.auto_status.as_deref().unwrap().contains("stopped"));
        assert!(matches!(app.scan, ScanStage::Setup { .. }), "the scan form is ready for a scan of your own");

        // Quitting during a background scan does not ask.
        let mut app = App::new(settings());
        app.handle_msg(Msg::Snapshots(Box::new(Ok(list()))));
        assert!(app.handle_key(key(KeyCode::Char('q'))).contains(&Command::Quit));
    }

    #[test]
    fn no_background_scan_without_an_earlier_scan_or_when_off() {
        let mut app = App::new(settings());
        let empty = SnapshotList { store_path: "/s".into(), store_exists: false, store_bytes: 0, snapshots: Vec::new(), disk_count: 0, disk: None };
        assert!(app.handle_msg(Msg::Snapshots(Box::new(Ok(empty.clone())))).is_empty());
        assert!(app.auto_status.as_deref().unwrap().contains("the first one is up to you"));
        let mut s = settings();
        s.auto_scan.value = false;
        let mut app = App::new(s);
        app.handle_msg(Msg::Snapshots(Box::new(Ok(empty))));
        assert!(app.auto_status.as_deref().unwrap().contains("off"));
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new(settings());
        app.handle_key(KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char('c')) });
        assert!(app.should_quit);
    }

    // My disks (ADR 0002, phase 7). A real store in target/test-tmp: two scans of one tree.

    fn with_store(name: &str) -> (diskdrift_app::core::testutil::TestDir, Effective, PathBuf) {
        let t = diskdrift_app::core::testutil::TestDir::new(name);
        let tree = t.path().join("tree");
        std::fs::create_dir_all(tree.join("a")).unwrap();
        std::fs::write(tree.join("a/x"), b"12345").unwrap();
        std::fs::write(t.path().join("config.json"), r#"{"schemaVersion":1}"#).unwrap();
        let o = Overrides { config: Some(t.path().join("config.json")), db: Some(t.path().join("store")), ..Default::default() };
        let s = resolve_with(&o, t.path(), Path::new("/")).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        for _ in 0..2 {
            let req = snapshots::ScanRequest::from_settings(&s, Some(tree.clone()), None, &[]);
            snapshots::scan(&s, &req, &cancel, None).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        (t, s, tree)
    }

    fn loaded(s: &Effective) -> App {
        let mut app = App::new(s.clone());
        app.handle_msg(Msg::Snapshots(Box::new(snapshots::list(s).map_err(|e| e.to_string()))));
        app
    }

    fn reload(app: &mut App) {
        let s = app.settings.clone();
        app.handle_msg(Msg::Disks(Box::new(disks::list(&s).map_err(|e| e.to_string()))));
        app.handle_msg(Msg::Snapshots(Box::new(snapshots::list(&s).map_err(|e| e.to_string()))));
    }

    #[test]
    fn history_marks_asks_and_removes_scans() {
        let (_t, s, _tree) = with_store("tui-history-remove");
        let mut app = loaded(&s);
        app.screen = Screen::Snapshots;
        assert_eq!((app.history().len(), app.snap_sel), (2, 1));
        // o: the scans of the folder older than the selected one.
        app.handle_key(key(KeyCode::Char('o')));
        let first = app.history()[0].meta.id;
        assert_eq!(app.snap_marks.iter().copied().collect::<Vec<_>>(), vec![first]);
        assert!(app.snap_note.as_deref().is_some_and(|n| n.contains("older than")), "{:?}", app.snap_note);
        // n: none without data here; p: no rules set yet.
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.snap_note.as_deref().is_some_and(|n| n.starts_with("No ")));
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.snap_note.as_deref().is_some_and(|n| n.contains("Settings")));
        // Esc clears the marks first, and stays.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.snap_marks.is_empty() && app.screen == Screen::Snapshots);
        // Space marks and moves down; Del asks with what it frees; another key keeps them.
        app.snap_sel = 0;
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.snap_sel, 1);
        assert!(app.handle_key(key(KeyCode::Delete)).is_empty());
        let p = app.snap_remove.clone().expect("asks");
        assert_eq!(p.snapshots.iter().map(|r| r.meta.id).collect::<Vec<_>>(), vec![first]);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit && app.snap_remove.is_none());
        assert_eq!(app.snap_note.as_deref(), Some("Nothing was removed."));
        // y removes them in the background; the result reloads History and My disks.
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), vec![Command::RemoveSnapshots { ids: vec![first] }]);
        assert!(app.store_job.is_some());
        // While it runs: no other store action, and no scan.
        app.handle_key(key(KeyCode::Char('z')));
        assert!(app.snap_note.as_deref().is_some_and(|n| n.contains("Wait")));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), vec![Command::CancelStoreJob]);
        let r = diskdrift_app::prune::remove(&s, &[first], &std::sync::atomic::AtomicBool::new(false)).map_err(|e| e.to_string());
        let cmds = app.handle_msg(Msg::Removed(Box::new(r)));
        assert!(cmds.contains(&Command::LoadSnapshots) && cmds.contains(&Command::LoadDisks));
        assert!(app.store_job.is_none() && app.snap_marks.is_empty());
        assert!(app.snap_note.as_deref().is_some_and(|n| n.starts_with("Removed 1 scan")), "{:?}", app.snap_note);
        reload(&mut app);
        assert_eq!(app.history().len(), 1);
        // z compacts (nothing to do here).
        assert_eq!(app.handle_key(key(KeyCode::Char('z'))), vec![Command::Compact]);
        let c = diskdrift_app::prune::compact(&s, &std::sync::atomic::AtomicBool::new(false), &mut |_| {}).map_err(|e| e.to_string());
        app.handle_msg(Msg::Compacted(Box::new(c)));
        assert!(app.snap_note.as_deref().is_some_and(|n| n.contains("already compact")), "{:?}", app.snap_note);
    }

    #[test]
    fn my_disks_filters_history_and_edits_the_disk() {
        let (_t, s, _tree) = with_store("tui-my-disks");
        let mut app = loaded(&s);
        assert_eq!(app.handle_key(key(KeyCode::Char('k'))), vec![Command::LoadDisks]);
        assert_eq!(app.screen, Screen::Disks);
        reload(&mut app);
        let id = app.selected_disk().unwrap().disk.id.clone();
        assert_eq!(app.selected_disk().unwrap().snapshots, 2);

        // Enter: History of that disk, Compare follows.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Command::LoadSnapshots]);
        assert_eq!(app.screen, Screen::Snapshots);
        assert_eq!(app.disk_filter.as_ref().and_then(|f| f.ids.first()).map(|i| i.as_str()), Some(id.as_str()));
        reload(&mut app);
        assert_eq!(app.history().len(), 2);
        assert_eq!(app.snap_sel, 1, "on the newest scan");
        app.handle_key(key(KeyCode::Char('*')));
        assert!(app.disk_filter.is_none());
        assert!(app.snap_note.as_deref().is_some_and(|n| n.contains("every disk")));

        // A label, typed; an invalid one is refused and can be corrected.
        app.handle_key(key(KeyCode::Char('k')));
        reload(&mut app);
        app.handle_key(key(KeyCode::Char('l')));
        assert!(app.text_input_active());
        typed(&mut app, "zly:label");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.disks.edit.is_some(), "the text stays for a correction");
        assert!(matches!(&app.disks.note, Some(Err(e)) if e.contains("':'")), "{:?}", app.disks.note);
        app.handle_key(KeyEvent { modifiers: KeyModifiers::CONTROL, ..key(KeyCode::Char('u')) });
        typed(&mut app, "robocze");
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert!(cmds.contains(&Command::LoadDisks) && cmds.contains(&Command::LoadSnapshots));
        assert!(matches!(&app.disks.note, Some(Ok(m)) if m.contains("label robocze")));
        reload(&mut app);
        assert_eq!(app.selected_disk().unwrap().disk.label.as_deref(), Some("robocze"));
        // Sets: + adds, - offers the last one to remove.
        app.handle_key(key(KeyCode::Char('+')));
        typed(&mut app, "archiwum");
        app.handle_key(key(KeyCode::Enter));
        reload(&mut app);
        assert_eq!(app.selected_disk().unwrap().disk.sets, ["archiwum"]);
        app.handle_key(key(KeyCode::Char('-')));
        assert_eq!(app.disks.edit, Some((DiskField::RemoveFromSet, "archiwum".into())));
        app.handle_key(key(KeyCode::Enter));
        reload(&mut app);
        assert!(app.selected_disk().unwrap().disk.sets.is_empty());

        // g: the scan form, confirmed, on the folder last scanned on the disk.
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.screen, Screen::Scan);
        let last = app.history().last().unwrap().meta.root.clone();
        assert!(matches!(&app.scan, ScanStage::Confirm { root, .. } if *root == last));

        // f asks; another key keeps it; y forgets it.
        app.handle_key(key(KeyCode::Char('k')));
        reload(&mut app);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('k')));
        reload(&mut app);
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.disks.forget.as_ref().is_some_and(|p| p.snapshots.len() == 2));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.disks.forget.is_none());
        assert_eq!(snapshots::list(&s).unwrap().snapshots.len(), 2, "nothing removed");
        app.handle_key(key(KeyCode::Char('f')));
        let cmds = app.handle_key(key(KeyCode::Char('y')));
        assert!(cmds.contains(&Command::LoadSnapshots));
        assert!(matches!(&app.disks.note, Some(Ok(m)) if m.starts_with("Forgot disk")), "{:?}", app.disks.note);
        assert!(app.disk_filter.is_none(), "the filter of a forgotten disk goes");
        assert!(snapshots::list(&s).unwrap().snapshots.is_empty());
    }

    #[test]
    fn a_doubtful_disk_is_asked_about_and_the_scan_starts_with_the_answer() {
        let mut app = App::new(settings());
        app.open(Screen::Scan);
        let cmds = app.start_scan("/Volumes/STICK".into(), "x".into());
        assert!(matches!(&cmds[..], [Command::StartScan { disk: None, .. }]));
        app.handle_msg(Msg::ScanDoubt("STICK looks like a copy of backupy-2014.".into()));
        assert!(matches!(&app.scan, ScanStage::AskDisk { root, name, .. } if root == "/Volumes/STICK" && name == "x"));
        assert!(app.handle_key(key(KeyCode::Char('q'))).is_empty(), "other keys wait for the answer");
        let cmds = app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(&cmds[..], [Command::StartScan { disk: Some(DiskChoice::New), root, .. }] if root == "/Volumes/STICK"));
        app.handle_msg(Msg::ScanDoubt("again".into()));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.scan, ScanStage::Setup { root, .. } if root == "/Volumes/STICK"));
    }

    // Connecting a disk (roadmap step 4, spec 3A item 11a).

    fn scan_outcome_for_tests() -> ScanOutcome {
        let list = rows(&[(1, "/", Status::Completed)]);
        ScanOutcome {
            snapshot: list.snapshots[0].meta.clone(),
            cancelled: false,
            store_path: "/s".into(),
            store_bytes: 0,
            rebase: diskdrift_app::storage::Rebase::Nothing,
            rebase_error: None,
            disk: None,
        }
    }

    fn vol(id: &str, mp: &str) -> Volume {
        Volume {
            identity: diskdrift_app::storage::Identity::new(diskdrift_app::storage::Strength::Uuid, id),
            mount_point: mp.into(),
            name: Some(id.into()),
            fs_type: "hfs".into(),
            capacity_bytes: None,
        }
    }

    fn known_disk(label: &str) -> diskdrift_app::storage::Disk {
        diskdrift_app::storage::Disk {
            id: "d1".into(),
            identity: diskdrift_app::storage::Identity::new(diskdrift_app::storage::Strength::Uuid, "B"),
            label: Some(label.into()),
            description: None,
            sets: Vec::new(),
            volume: Default::default(),
            device: None,
            first_seen: "2026-01-01T10:00:00+01:00".into(),
            last_seen: "2026-01-01T10:00:00+01:00".into(),
            last_mount_point: None,
        }
    }

    #[test]
    fn the_mounts_at_start_are_the_baseline_and_changes_are_looked_at() {
        let mut app = App::new(settings());
        let cmds = app.handle_msg(Msg::Volumes(vec![vol("SYS", "/")]));
        assert!(!cmds.iter().any(|c| matches!(c, Command::DiskConnected(_))), "nothing is 'connected' at start");
        assert!(app.disk_event.is_none());
        let cmds = app.handle_msg(Msg::Volumes(vec![vol("SYS", "/"), vol("B", "/Volumes/B")]));
        assert!(cmds.contains(&Command::DiskConnected(vol("B", "/Volumes/B"))), "{cmds:?}");
        app.handle_msg(Msg::Disks(Box::new(Err("x".into()))));
        app.handle_msg(Msg::Volumes(vec![vol("SYS", "/")]));
        assert!(app.disk_event.as_ref().is_some_and(|e| e.text.contains("B disconnected")), "{:?}", app.disk_event);
    }

    #[test]
    fn a_known_disk_is_rescanned_and_then_said_to_be_safe_to_eject() {
        let mut app = App::new(settings());
        app.handle_msg(Msg::Volumes(vec![]));
        let action = Ok(ConnectAction::Rescan { disk: known_disk("backupy-2014"), root: "/Volumes/B/Docs".into(), last_scan: "2026-01-01T10:00:00+01:00".into() });
        let cmds = app.handle_msg(Msg::Connected { volume: vol("B", "/Volumes/B"), action: Box::new(action) });
        assert!(matches!(&cmds[..], [Command::StartScan { root, background: true, disk: None, name: Some(n) }] if root == "/Volumes/B/Docs" && n.starts_with("auto-")), "{cmds:?}");
        assert!(app.auto_scan_running());
        assert!(app.disk_event.as_ref().is_some_and(|e| e.text.contains("backupy-2014 connected") && e.text.contains("x stops")));
        let mut done = scan_outcome_for_tests();
        done.snapshot.root = "/Volumes/B/Docs".into();
        app.handle_msg(Msg::ScanDone(Box::new(Ok(done))));
        let status = app.auto_status.clone().unwrap();
        assert!(status.contains("backupy-2014 rescanned") && status.contains("you can eject it"), "{status}");
        assert!(matches!(app.scan, ScanStage::Setup { .. }), "the scan form is free again");
    }

    #[test]
    fn a_rescan_waits_until_the_scan_screen_is_free() {
        let mut app = App::new(settings());
        app.open(Screen::Scan);
        let action = |l: &str| Ok(ConnectAction::Rescan { disk: known_disk(l), root: format!("/Volumes/{l}").into(), last_scan: "2026-01-01T10:00:00+01:00".into() });
        assert!(app.handle_msg(Msg::Connected { volume: vol("B", "/Volumes/B"), action: Box::new(action("one")) }).is_empty());
        assert!(app.handle_msg(Msg::Connected { volume: vol("C", "/Volumes/C"), action: Box::new(action("two")) }).is_empty());
        assert!(app.disk_event.as_ref().is_some_and(|e| e.text.contains("as soon as the scan screen is free")));
        assert!(app.idle().is_empty(), "the user is on the scan form");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(matches!(&app.idle()[..], [Command::StartScan { root, .. }] if root == "/Volumes/one"));
        assert!(app.idle().is_empty(), "one at a time");
        app.handle_msg(Msg::ScanDone(Box::new(Ok(scan_outcome_for_tests()))));
        assert!(matches!(&app.idle()[..], [Command::StartScan { root, .. }] if root == "/Volumes/two"));
    }

    #[test]
    fn a_new_disk_is_announced_and_never_scanned_on_its_own() {
        let mut app = App::new(settings());
        for action in [Ok(ConnectAction::New { disk: None }), Ok(ConnectAction::Doubt { message: "STICK looks like a copy of X.".into() })] {
            assert!(app.handle_msg(Msg::Connected { volume: vol("S", "/Volumes/STICK"), action: Box::new(action) }).is_empty());
            assert!(!app.scan_running());
        }
        assert!(app.disk_event.as_ref().is_some_and(|e| e.text.contains("Scan it with S")));
        app.handle_msg(Msg::Connected { volume: vol("S", "/Volumes/STICK"), action: Box::new(Ok(ConnectAction::New { disk: None })) });
        assert!(app.disk_event.as_ref().is_some_and(|e| e.text.contains("new disk S connected") && e.text.contains("K to give it a label")));
        assert!(app.idle().is_empty());
    }

    #[test]
    fn w_in_my_disks_filters_to_the_disks_set_and_find_follows() {
        let mut app = App::new(settings());
        let row = |id: &str, label: &str, sets: &[&str]| diskdrift_app::disks::DiskRow {
            disk: diskdrift_app::storage::Disk { id: id.into(), sets: sets.iter().map(|s| s.to_string()).collect(), ..known_disk(label) },
            known: true,
            connected: false,
            mount_point: None,
            snapshots: 1,
            last_scan: None,
            last_size: None,
            last_root: None,
            store_bytes: 0,
        };
        let disks = vec![row("d1", "old-a", &["stare", "2014"]), row("d2", "old-b", &["stare"]), row("d3", "lone", &[])];
        app.disks.list = Job::Done(Box::new(DiskList { store_path: "/s".into(), store_exists: true, disks }));
        app.screen = Screen::Disks;
        app.handle_key(key(KeyCode::Char('w')));
        let f = app.disk_filter.clone().unwrap();
        assert_eq!((f.set.as_deref(), f.ids.clone()), (Some("stare"), vec!["d1".to_string(), "d2".to_string()]));
        assert_eq!(app.screen, Screen::Snapshots);
        assert!(app.history().is_empty() || app.history().iter().all(|r| f.shows(r.meta.disk.as_deref())));
        // Again: its next set.
        app.screen = Screen::Disks;
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(app.disk_filter.as_ref().unwrap().set.as_deref(), Some("2014"));
        // Find searches only the set.
        app.open(Screen::Find);
        typed(&mut app, "plan");
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds[1], Command::RunFind { id: 1, pattern: "plan".into(), disk: None, set: Some("2014".into()) });
        // A disk in no set says so.
        app.screen = Screen::Disks;
        app.disks.sel = 2;
        app.handle_key(key(KeyCode::Char('w')));
        assert!(matches!(&app.disks.note, Some(Err(m)) if m.contains("in no set")), "{:?}", app.disks.note);
    }

    #[test]
    fn analysing_a_scan_of_a_disconnected_disk_says_to_connect_it() {
        let mut app = App::new(settings());
        let mut list = rows(&[(7, "/Volumes/OLD", Status::Completed)]);
        list.snapshots[0].meta.disk = Some("d1".into());
        list.snapshots[0].disk_name = Some("backupy-2014".into());
        app.handle_msg(Msg::Snapshots(Box::new(Ok(list))));
        let row = |connected: bool| diskdrift_app::disks::DiskRow {
            disk: known_disk("backupy-2014"),
            known: true,
            connected,
            mount_point: None,
            snapshots: 1,
            last_scan: None,
            last_size: None,
            last_root: None,
            store_bytes: 0,
        };
        app.disks.list = Job::Done(Box::new(DiskList { store_path: "/s".into(), store_exists: true, disks: vec![row(false)] }));
        app.screen = Screen::Snapshots;
        app.open_browse(BrowseSource::Snapshot(7), None, None);
        app.handle_msg(Msg::Browse { id: 1, result: Box::new(Ok(browse_listing("/Volumes/OLD", "/Volumes/OLD", &[("Docs", true)]))) });
        assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty());
        assert_eq!(app.screen, Screen::Browse);
        let note = app.browse.as_ref().unwrap().note.clone().unwrap();
        assert!(note.contains("backupy-2014 is not connected") && note.contains("/Volumes/OLD/Docs"), "{note}");
        // Connected (or not known to be disconnected): the analysis starts as before.
        app.disks.list = Job::Done(Box::new(DiskList { store_path: "/s".into(), store_exists: true, disks: vec![row(true)] }));
        assert!(matches!(&app.handle_key(key(KeyCode::Char('a')))[..], [Command::CancelAnalyze, Command::RunAnalyze { .. }]));
    }
}
