//! `diskdrift` command-line entry point.

use clap::{Args, Parser, Subcommand, ValueEnum};
use diskdrift_app::config::{self, ConfigError, Effective, Overrides};
use diskdrift_app::core::units::{SizeUnits, parse_size};
use diskdrift_app::diagnostics::doctor::{self, DoctorOptions, DoctorStep};
use diskdrift_app::platform::{self, ListerKind};
use diskdrift_app::text as render;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// The system allocator keeps memory that one thread frees after another
// allocated it (snapshot blocks are decoded on reader threads); mimalloc reuses
// it, which keeps long TUI sessions flat (see docs/benchmarks).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Exit codes (spec 3.3).
const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CANCELLED: u8 = 3;

#[derive(Parser)]
#[command(
    name = "diskdrift",
    version,
    about = "Read-only disk-space investigation: where did my free space go?",
    long_about = "DiskDrift reports OS-level space accounting, large storage consumers and \
                  (with saved scans) what grew between two points in time. It never modifies, \
                  moves or deletes your files.\n\nRun without a command in a terminal to open the full-screen UI."
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Clone)]
struct Global {
    /// Snapshot store location (overrides storage.path in the config).
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,
    /// Config file (default: ~/.diskdrift/config.json).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Size units for human-readable output.
    #[arg(long, global = true, value_enum)]
    units: Option<Units>,
    /// More diagnostic logging on stderr (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Also write diagnostic logs to this file.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Units {
    Decimal,
    Binary,
}

#[derive(Subcommand)]
enum Command {
    /// Open the full-screen terminal UI.
    Tui,
    /// OS-reported space per volume, grouped by APFS container.
    Usage {
        /// Include hidden system volumes.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Environment facts: APFS topology, permissions, configuration.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Diagnose storage usage.
    Doctor(DoctorArgs),
    /// Scan a directory tree (metadata only) and save it as a snapshot.
    Scan(ScanArgs),
    /// List saved snapshots; remove, prune, compact or verify them.
    Snapshots {
        #[command(subcommand)]
        action: Option<SnapshotAction>,
        /// Only this disk's snapshots (label, disk ID or a prefix of it).
        #[arg(long, value_name = "DISK", global = true)]
        disk: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Find a file in the saved scans of every disk, connected or not; when it
    /// is not there any more, search the history: when was it deleted.
    Find(FindArgs),
    /// Careful mode for old disks, until Ctrl-C: a disk connected now is not
    /// mounted (nothing is written to it); it is scanned read-only with one
    /// thread and ejected.
    Careful(CarefulArgs),
    /// The disks the store knows (connected or not), or change one: label,
    /// describe, sets, merge, forget.
    Disks {
        #[command(subcommand)]
        action: Option<DiskAction>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Largest files and directories of a snapshot.
    Top(TopArgs),
    /// Compare two snapshots: what grew, shrank, appeared or disappeared.
    Diff(DiffArgs),
    /// One directory level of a snapshot, largest first; with --compare, what
    /// changed there since an older snapshot.
    Browse {
        /// Directory inside the snapshot's root (default: the root).
        path: Option<PathBuf>,
        #[arg(long, default_value = "latest")]
        snapshot: String,
        /// Older snapshot to compare with.
        #[arg(long, value_name = "SNAPSHOT")]
        compare: Option<String>,
        /// Show at most this many items.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Read-only, ranked suggestions where space could be recovered.
    Recover {
        /// Time budget for measuring known locations, in seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = 120)]
        budget: u64,
        #[arg(long)]
        json: bool,
    },
    /// What the folders inside a directory are, how big, and which matter.
    Analyze {
        /// Directory to analyse (e.g. "~/Library/Application Support").
        path: String,
        /// Time budget for measuring, in seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = 60)]
        budget: u64,
        /// Show at most this many items.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show or create the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Args)]
struct DoctorArgs {
    /// Quick diagnosis without a full scan (default).
    #[arg(long, conflicts_with = "deep")]
    quick: bool,
    /// Also use saved snapshots: current inventory, latest change, unexplained difference.
    #[arg(long)]
    deep: bool,
    /// Free space you remember having earlier, e.g. 120GB (shown as not verified).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    reference_free: Option<u64>,
    /// Time budget for measuring known locations, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 120)]
    budget: u64,
    /// Worker threads (0 = automatic).
    #[arg(long)]
    threads: Option<usize>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ScanArgs {
    /// Directory to scan (default: scan.defaultRoot from the config, normally /).
    root: Option<OsString>,
    /// Snapshot name (default: the start time).
    #[arg(long)]
    name: Option<String>,
    /// Exclude an absolute path or an entry name (repeatable; adds to the config).
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
    /// Worker threads (0 = automatic).
    #[arg(long)]
    threads: Option<usize>,
    /// When the disk looks like a copy of a known one, or is recognised only
    /// weakly: whether it is that disk.
    #[arg(long, value_enum)]
    disk: Option<DiskArg>,
    /// Scan the careful way (spec 3C): one thread, low priority, the snapshot
    /// marked careful; warns when the folder's volume is not mounted read-only.
    #[arg(long)]
    careful: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CarefulArgs {
    /// Scan every disk that is connected, without asking (a doubtful one is
    /// left held back).
    #[arg(long)]
    scan: bool,
    /// The answer for a disk that looks like a copy of a known one, or is
    /// recognised only weakly.
    #[arg(long, value_enum)]
    disk: Option<DiskArg>,
}

#[derive(Subcommand)]
enum DiskAction {
    /// Set a disk's label, e.g. backupy-2014 (without a label: remove it).
    Label { disk: String, label: Option<String> },
    /// Set a disk's description (without text: remove it).
    Describe { disk: String, text: Option<String> },
    /// Put a disk in a set, or take it out (a disk can be in several).
    Set {
        #[arg(value_enum)]
        op: SetOp,
        disk: String,
        set: String,
    },
    /// Join the history of one disk to another: one disk recognised as two.
    Merge { from: String, into: String },
    /// Remove a disk's snapshots and its entry (files on the disk are never touched).
    Forget {
        disk: String,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SetOp {
    Add,
    Remove,
}

#[derive(Clone, Copy, ValueEnum)]
enum DiskArg {
    /// The disk DiskDrift recognised (doubtfully) is this one.
    Same,
    /// It is another disk.
    New,
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Delete DiskDrift's own snapshots (never touches scanned files). One
    /// snapshot is removed at once; several, --failed or --before list what
    /// would go and ask first.
    Remove {
        /// Snapshots: ID, name, latest or latest~N.
        snapshots: Vec<String>,
        /// Every cancelled and failed scan (they hold no data).
        #[arg(long)]
        failed: bool,
        /// Every scan started before this day.
        #[arg(long, value_name = "YYYY-MM-DD")]
        before: Option<String>,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
    /// Remove the scans the retention rules do not keep, per disk and folder
    /// (the newest is always kept). Rules from the flags, else from the config
    /// (storage.keepLast, keepWeekly, keepMonthly). Lists them and asks first.
    Prune {
        /// Keep the newest N scans.
        #[arg(long, value_name = "N")]
        keep_last: Option<usize>,
        /// Keep the newest scan of each of the last N weeks.
        #[arg(long, value_name = "N")]
        keep_weekly: Option<usize>,
        /// Keep the newest scan of each of the last N months.
        #[arg(long, value_name = "N")]
        keep_monthly: Option<usize>,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
    /// Store every snapshot as writing the scans one by one would have (after
    /// removals), then check every checksum. Ctrl-C stops it; the store stays valid.
    Compact,
    /// Re-read a snapshot and check all checksums.
    Verify { snapshot: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum Metric {
    Allocated,
    Logical,
}

#[derive(Args)]
struct FindArgs {
    /// Part of a name (`wazna`), a glob over the whole name (`*.pdf`), or with
    /// `/` a part of the path (`Docs/wazna`). Case and diacritics are ignored.
    pattern: String,
    /// The pattern is a regular expression (also: a `re:` prefix).
    #[arg(long)]
    regex: bool,
    /// Match case and diacritics exactly.
    #[arg(long)]
    case: bool,
    /// Only files or only folders.
    #[arg(long, value_enum)]
    kind: Option<FindKind>,
    /// Only entries at least this large on disk.
    #[arg(long, value_parser = parse_size, default_value = "0")]
    min_size: u64,
    /// Only this disk (label or short ID).
    #[arg(long, conflicts_with = "set")]
    disk: Option<String>,
    /// Only the disks of this set (`disks set add <disk> <set>`).
    #[arg(long)]
    set: Option<String>,
    /// Show at most this many results per list (all are counted).
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Search the history also when there are current matches (when they
    /// appeared, how often they changed, older copies that are gone).
    #[arg(long, conflicts_with = "no_history")]
    history: bool,
    /// Never search the history.
    #[arg(long)]
    no_history: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum FindKind {
    File,
    Dir,
}

#[derive(Args)]
struct TopArgs {
    /// Snapshot: id, name, latest or latest~N.
    #[arg(long, default_value = "latest")]
    snapshot: String,
    /// Only files.
    #[arg(long, conflicts_with = "directories")]
    files: bool,
    /// Only directories.
    #[arg(long)]
    directories: bool,
    /// Only the deepest large directories (leave out those whose size mostly
    /// repeats a listed directory below them).
    #[arg(long)]
    deepest: bool,
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Only below this path.
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "allocated")]
    metric: Metric,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortBy {
    Growth,
    Size,
    Modified,
    Created,
    Path,
}

#[derive(Args)]
struct DiffArgs {
    /// Older snapshot (id, name, latest~1, ...).
    old: String,
    /// Newer snapshot (default: latest).
    #[arg(default_value = "latest")]
    new: String,
    #[arg(long, value_enum, default_value = "growth")]
    sort: SortBy,
    /// Only below this path.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Only list entries whose size changed by at least this much (e.g. 1GB).
    #[arg(long, value_parser = parse_size, default_value = "0")]
    min_size: u64,
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Only list files.
    #[arg(long, conflicts_with = "directories")]
    files: bool,
    /// Only list directories.
    #[arg(long)]
    directories: bool,
    /// Compare snapshots with different roots, volumes or exclusions anyway.
    #[arg(long)]
    allow_incompatible: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print effective settings and where each comes from.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Create the default config file (never overwrites).
    Init,
}

/// Errors mapped to exit codes.
enum Failure {
    Usage(String),
    Runtime(String),
    Cancelled,
}

impl From<ConfigError> for Failure {
    fn from(e: ConfigError) -> Self {
        Failure::Usage(e.to_string())
    }
}

impl From<diskdrift_app::snapshots::AppError> for Failure {
    fn from(e: diskdrift_app::snapshots::AppError) -> Self {
        if let diskdrift_app::snapshots::AppError::DiskDoubt(why) = &e {
            return Failure::Usage(format!("{why} Scan again with --disk same if it is that disk, or --disk new if it is another one."));
        }
        if e.is_usage() { Failure::Usage(e.to_string()) } else { Failure::Runtime(e.to_string()) }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Runtime(e.to_string())
    }
}

type Outcome = Result<(), Failure>;

fn main() -> ExitCode {
    // Exceeding a file-size limit must surface as an error (EFBIG) that the store
    // handles like a full disk, not kill the process mid-write.
    ignore_file_size_limit();
    let cli = Cli::parse();
    if let Err(e) = init_logging(&cli.global) {
        eprintln!("diskdrift: {e}");
        return ExitCode::from(EXIT_USAGE);
    }
    let result = run(cli);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(m)) => {
            eprintln!("diskdrift: {m}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(Failure::Runtime(m)) => {
            eprintln!("diskdrift: {m}");
            ExitCode::from(EXIT_FAILURE)
        }
        Err(Failure::Cancelled) => {
            eprintln!("diskdrift: cancelled");
            ExitCode::from(EXIT_CANCELLED)
        }
    }
}

#[cfg(unix)]
fn ignore_file_size_limit() {
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
}

#[cfg(not(unix))]
fn ignore_file_size_limit() {}

fn init_logging(g: &Global) -> Result<(), String> {
    use tracing_subscriber::prelude::*;
    let level = match g.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false);
    let file = match &g.log_file {
        Some(p) => {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(|e| format!("cannot open log file {}: {e}", p.display()))?;
            Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)))
        }
        None => None,
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(level))
        .with(stderr)
        .with(file)
        .init();
    Ok(())
}

fn overrides(g: &Global, threads: Option<usize>) -> Overrides {
    Overrides {
        config: g.config.clone(),
        db: g.db.clone(),
        threads,
        size_units: g.units.map(|u| match u {
            Units::Decimal => SizeUnits::Decimal,
            Units::Binary => SizeUnits::Binary,
        }),
    }
}

fn settings(g: &Global, threads: Option<usize>) -> Result<Effective, Failure> {
    Ok(config::resolve(&overrides(g, threads))?)
}

fn print_json<T: serde::Serialize>(v: &T) -> Outcome {
    let mut text = serde_json::to_string_pretty(v).map_err(|e| Failure::Runtime(e.to_string()))?;
    text.push('\n');
    print_text(&text)
}

fn print_text(s: &str) -> Outcome {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()) {
        // A closed pipe (`| head`) is not an error for the user.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => Ok(r?),
    }
}

fn find_cmd(g: &Global, args: FindArgs) -> Outcome {
    use diskdrift_app::search::{self, HistoryMode, Query};
    let s = settings(g, None)?;
    let q = Query {
        pattern: args.pattern,
        regex: args.regex,
        exact: args.case,
        kind: args.kind.map(|k| match k {
            FindKind::File => diskdrift_app::core::record::Kind::File,
            FindKind::Dir => diskdrift_app::core::record::Kind::Dir,
        }),
        min_size: args.min_size,
        disk: args.disk,
        set: args.set,
        limit: args.limit,
    };
    let mode = if args.history {
        HistoryMode::Always
    } else if args.no_history {
        HistoryMode::Never
    } else {
        HistoryMode::Auto
    };
    let cancel = cancel_flag();
    let cancelled = || if cancel.load(Ordering::SeqCst) { Err(Failure::Cancelled) } else { Ok(()) };
    if args.json {
        let r = search::find(&s, &q, mode, &cancel, &|_, _| {})?;
        print_json(&r)?;
        return cancelled();
    }
    let now = now_unix();
    let u = s.size_units.value;
    // The current part first, so the user reads it while the history is searched.
    let cur = search::current_cancellable(&s, &q, None, &cancel)?;
    print_text(&diskdrift_app::text::search::current(&cur, u, now))?;
    let run = match mode {
        HistoryMode::Never => false,
        HistoryMode::Always => true,
        HistoryMode::Auto => cur.total == 0,
    };
    if !run || !cur.store_exists || cancel.load(Ordering::SeqCst) {
        return cancelled();
    }
    if cur.total == 0 {
        print_text(&format!("{}\n", diskdrift_app::text::search::SEARCHING_HISTORY))?;
    } else {
        print_text("\n")?;
    }
    let _ = std::io::stdout().flush();
    let h = search::history(&s, &q, &cur, mode == HistoryMode::Always, &cancel, &|_, _| {})?;
    print_text(&diskdrift_app::text::search::history(&h, cur.total > 0, u, now))?;
    cancelled()
}

fn careful_cmd(g: &Global, args: CarefulArgs) -> Outcome {
    use diskdrift_app::careful;
    use diskdrift_app::disks::DiskChoice;
    use diskdrift_app::platform::{GuardEvent, MountGuard};
    use diskdrift_app::text::careful as text;
    let s = settings(g, None)?;
    let u = s.size_units.value;
    let (tx, rx) = std::sync::mpsc::channel();
    let guard = MountGuard::start(Box::new(move |e| {
        let _ = tx.send(e);
    }))
    .map_err(|e| Failure::Runtime(format!("cannot start careful mode: {e}")))?;
    let cancel = cancel_flag();
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    print_text(&format!("{}\nConnect the disks now. Ctrl-C ends careful mode (a running scan stops and its disk is ejected).\n", text::ON))?;
    let choice = args.disk.map(|d| match d {
        DiskArg::Same => DiskChoice::Same,
        DiskArg::New => DiskChoice::New,
    });
    let ask = |question: &str| -> String {
        eprint!("{question} ");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_lowercase()
    };
    while !cancel.load(Ordering::SeqCst) {
        let event = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(e) => e,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let volume = match event {
            GuardEvent::Held(v) => *v,
            GuardEvent::Gone(bsd) => {
                print_text(&format!("{bsd} is gone.\n"))?;
                continue;
            }
        };
        let held = careful::describe(&s, volume)?;
        print_text(&format!("\n{}\n", text::held(&held, u, now_unix())))?;
        let action = if args.scan {
            if held.doubt.is_some() && choice.is_none() {
                print_text("  Left held back: say with --disk same|new whether it is that disk, or scan it without --scan.\n")?;
                continue;
            }
            "s".to_string()
        } else if interactive {
            ask("  s scan it carefully · m mount read-only for Finder · e eject · Enter leave it held:")
        } else {
            print_text("  Left held back (no terminal to ask on; --scan scans every disk).\n")?;
            continue;
        };
        match action.as_str() {
            "s" => {
                let mut this = choice;
                if held.doubt.is_some() && this.is_none() {
                    this = match ask("  Is it that disk? s the same disk · n another disk:").as_str() {
                        "s" => Some(DiskChoice::Same),
                        "n" => Some(DiskChoice::New),
                        _ => {
                            print_text("  Left held back.\n")?;
                            continue;
                        }
                    };
                }
                print_text("  Mounting it read-only and scanning with one thread…\n")?;
                let _ = std::io::stdout().flush();
                match careful::careful_scan(&s, &guard, &held, this, &cancel, None) {
                    Ok(o) => print_text(&text::outcome(&o, u))?,
                    Err(e) => print_text(&format!("  {e}\n"))?,
                }
            }
            "m" => match careful::mount_for_finder(&guard, &held) {
                Ok(at) => print_text(&format!("  Mounted read-only at {} (Finder shows it and cannot write to it). Eject it in Finder when done.\n", at.display()))?,
                Err(e) => print_text(&format!("  {e}\n"))?,
            },
            "e" => match careful::eject(&guard, &held) {
                Ok(()) => print_text("  Ejected: safe to unplug.\n")?,
                Err(e) => print_text(&format!("  {e}\n"))?,
            },
            _ => print_text("  Left held back, not mounted: it can be unplugged.\n")?,
        }
    }
    #[cfg(target_os = "macos")]
    drop(guard);
    print_text(&format!("{}\n", text::OFF))?;
    Ok(())
}

/// Lists what `sel` removes, asks (when `ask`), removes it.
fn remove_selection(s: &Effective, sel: &diskdrift_app::prune::Selection, disk: Option<&str>, ask: bool, json: bool) -> Outcome {
    use diskdrift_app::prune;
    use diskdrift_app::text::prune as text;
    let u = s.size_units.value;
    let p = prune::propose(s, sel, disk)?;
    if p.snapshots.is_empty() {
        return if json { print_json(&p) } else { print_text(&text::proposal(&p, u)) };
    }
    if ask {
        if json || !std::io::stdin().is_terminal() {
            eprint!("{}", text::proposal(&p, u));
            return Err(Failure::Usage("nothing removed: run again with --yes to confirm".into()));
        }
        eprint!("{}Remove them? [y/N] ", text::proposal(&p, u));
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return print_text("Nothing was removed.\n");
        }
    }
    let ids: Vec<u64> = p.snapshots.iter().map(|r| r.meta.id).collect();
    let r = prune::remove(s, &ids, &cancel_flag())?;
    if json { print_json(&r) } else { print_text(&text::removed(&r, u)) }
}

/// Cancellation flag set by Ctrl-C.
fn cancel_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let f = flag.clone();
    let _ = ctrlc::set_handler(move || {
        if f.swap(true, Ordering::SeqCst) {
            std::process::exit(EXIT_CANCELLED as i32);
        }
    });
    flag
}

fn run(cli: Cli) -> Outcome {
    let g = &cli.global;
    let Some(command) = cli.command else {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            let s = settings(g, None)?;
            return diskdrift_tui::run(s).map_err(|e| Failure::Runtime(e.to_string()));
        }
        // No interactive terminal: print help, never escape sequences.
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        return print_text(&help);
    };
    match command {
        Command::Tui => {
            if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
                return Err(Failure::Usage("the TUI needs an interactive terminal".into()));
            }
            let s = settings(g, None)?;
            diskdrift_tui::run(s).map_err(|e| Failure::Runtime(e.to_string()))
        }
        Command::Usage { all, json } => {
            let s = settings(g, None)?;
            let report = diskdrift_app::usage(all);
            if json { print_json(&report) } else { print_text(&render::usage(&report, s.size_units.value)) }
        }
        Command::Info { json } => {
            let s = settings(g, None)?;
            let report = diskdrift_app::info::info(&s);
            if json { print_json(&report) } else { print_text(&render::info(&report, s.size_units.value)) }
        }
        Command::Doctor(args) => doctor_cmd(g, args),
        Command::Scan(args) => scan_cmd(g, args),
        Command::Recover { budget, json } => recover_cmd(g, budget, json),
        Command::Browse { path, snapshot, compare, limit, json } => {
            let s = settings(g, None)?;
            let u = s.size_units.value;
            match compare {
                Some(old) => {
                    let r = diskdrift_app::snapshots::diff_browse(&s, &old, &snapshot, path.as_deref(), limit)?;
                    if json { print_json(&r) } else { print_text(&render::diff_browse(&r, u)) }
                }
                None => {
                    let r = diskdrift_app::snapshots::browse(&s, &snapshot, path.as_deref(), limit)?;
                    if json { print_json(&r) } else { print_text(&render::browse(&r, u)) }
                }
            }
        }
        Command::Analyze { path, budget, limit, json } => analyze_cmd(g, &path, budget, limit, json),
        Command::Disks { action, json } => disks_cmd(g, action, json),
        Command::Snapshots { action, disk, json } => {
            let s = settings(g, None)?;
            match action {
                None => {
                    let l = diskdrift_app::snapshots::list_disk(&s, disk.as_deref())?;
                    if json { print_json(&l) } else { print_text(&render::snapshot_list(&l, s.size_units.value)) }
                }
                Some(SnapshotAction::Remove { snapshots, failed, before, yes }) => {
                    use diskdrift_app::prune::{Selection, parse_day};
                    let sel = match (snapshots.is_empty(), failed, before) {
                        (false, false, None) => Selection::Refs(snapshots),
                        (true, true, None) => Selection::Failed,
                        (true, false, Some(d)) => Selection::Before(parse_day(&d)?),
                        (true, false, None) => return Err(Failure::Usage("say which snapshots: IDs or names, --failed or --before YYYY-MM-DD".into())),
                        _ => return Err(Failure::Usage("give snapshots, --failed or --before, one of them".into())),
                    };
                    // One snapshot named exactly is removed without asking, as before.
                    let ask = !yes && !matches!(&sel, Selection::Refs(r) if r.len() == 1);
                    remove_selection(&s, &sel, disk.as_deref(), ask, json)
                }
                Some(SnapshotAction::Prune { keep_last, keep_weekly, keep_monthly, yes }) => {
                    let rules = if keep_last.is_some() || keep_weekly.is_some() || keep_monthly.is_some() {
                        diskdrift_app::prune::Retention {
                            keep_last: keep_last.unwrap_or(0),
                            keep_weekly: keep_weekly.unwrap_or(0),
                            keep_monthly: keep_monthly.unwrap_or(0),
                        }
                    } else {
                        s.retention.value
                    };
                    if !json && !rules.is_off() {
                        eprintln!("Retention: keep {} of each disk and folder (and always the newest).", rules.describe());
                    }
                    remove_selection(&s, &diskdrift_app::prune::Selection::Retention(rules), disk.as_deref(), !yes, json)
                }
                Some(SnapshotAction::Compact) => {
                    use diskdrift_app::text::prune as text;
                    let cancel = cancel_flag();
                    let interactive = std::io::stderr().is_terminal() && !json;
                    let c = diskdrift_app::prune::compact(&s, &cancel, &mut |step| {
                        if interactive {
                            eprint!("\r\x1b[2K{}", text::compact_step(&step));
                        }
                    })?;
                    if interactive {
                        eprint!("\r\x1b[2K");
                    }
                    let out = if json { print_json(&c) } else { print_text(&text::compaction(&c, s.size_units.value)) };
                    if c.cancelled { Err(Failure::Cancelled) } else { out }
                }
                Some(SnapshotAction::Verify { snapshot }) => {
                    let (m, n) = diskdrift_app::snapshots::verify(&s, &snapshot)?;
                    if json {
                        print_json(&serde_json::json!({ "snapshot": m.id, "records": n, "ok": true }))
                    } else {
                        print_text(&format!("Snapshot {} '{}': {} records, all checksums valid.\n", m.id, m.name, n))
                    }
                }
            }
        }
        Command::Find(args) => find_cmd(g, args),
        Command::Careful(args) => careful_cmd(g, args),
        Command::Top(args) => {
            let s = settings(g, None)?;
            let opts = diskdrift_app::core::top::TopOptions {
                limit: args.limit,
                metric: match args.metric {
                    Metric::Allocated => diskdrift_app::core::top::SizeMetric::Allocated,
                    Metric::Logical => diskdrift_app::core::top::SizeMetric::Logical,
                },
                prefix: None,
                files: !args.directories,
                dirs: !args.files,
                deepest: args.deepest,
            };
            let r = diskdrift_app::snapshots::top(&s, &args.snapshot, args.path.as_deref(), opts)?;
            if args.json { print_json(&r) } else { print_text(&render::top(&r, s.size_units.value, !args.directories, !args.files)) }
        }
        Command::Diff(args) => {
            let s = settings(g, None)?;
            let opts = diskdrift_app::core::diff::DiffOptions {
                min_size: args.min_size,
                limit: args.limit,
                sort: match args.sort {
                    SortBy::Growth => diskdrift_app::core::diff::DiffSort::Growth,
                    SortBy::Size => diskdrift_app::core::diff::DiffSort::Size,
                    SortBy::Modified => diskdrift_app::core::diff::DiffSort::Modified,
                    SortBy::Created => diskdrift_app::core::diff::DiffSort::Created,
                    SortBy::Path => diskdrift_app::core::diff::DiffSort::Path,
                },
                list_files: !args.directories,
                list_dirs: !args.files,
                ..Default::default()
            };
            let r = diskdrift_app::snapshots::diff(&s, &args.old, &args.new, args.path.as_deref(), args.allow_incompatible, opts)?;
            if args.json { print_json(&r) } else { print_text(&render::diff(&r, s.size_units.value, !args.directories, !args.files)) }
        }
        Command::Config { action } => match action {
            ConfigAction::Show { json } => {
                let s = settings(g, None)?;
                if json { print_json(&s) } else { print_text(&render::config_show(&s)) }
            }
            ConfigAction::Init => {
                let path = match &g.config {
                    Some(p) => config::expand_path(
                        &p.to_string_lossy(),
                        &config::home_dir()?,
                        &std::env::current_dir()?,
                    ),
                    None => config::home_dir()?.join(config::APP_DIR).join("config.json"),
                };
                config::init(&path)?;
                print_text(&format!("Created {}\n", path.display()))
            }
        },
    }
}

fn doctor_progress(interactive: bool) -> impl Fn(DoctorStep) {
    move |step: DoctorStep| {
        if !interactive {
            return;
        }
        let msg = match step {
            DoctorStep::Volumes => "reading volumes…".to_string(),
            DoctorStep::Snapshots => "listing local snapshots…".to_string(),
            DoctorStep::Measuring { label, index, total } => format!("measuring {label} ({}/{total})…", index + 1),
            DoctorStep::OpenFiles => "checking deleted-but-open files…".to_string(),
            DoctorStep::Measured { .. } | DoctorStep::FromScan { .. } => return,
        };
        eprint!("\r\x1b[2K{msg}");
    }
}

fn doctor_cmd(g: &Global, args: DoctorArgs) -> Outcome {
    let _ = args.quick; // quick is the default mode
    let s = settings(g, args.threads)?;
    let cancel = cancel_flag();
    let interactive = std::io::stderr().is_terminal();
    let progress = doctor_progress(interactive);
    let budget = Duration::from_secs(args.budget);
    if args.deep {
        let r = diskdrift_app::investigate::deep(&s, args.reference_free, budget, &cancel, &progress);
        if interactive {
            eprint!("\r\x1b[2K");
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(Failure::Cancelled);
        }
        let r = r?;
        return if args.json { print_json(&r) } else { print_text(&render::deep(&r, s.size_units.value)) };
    }
    let opts = DoctorOptions {
        home: s.home.clone(),
        reference_free: args.reference_free,
        budget,
        threads: s.threads.value,
        skip: None,
        // `doctor --quick` measures now: it must not depend on a store.
        known: None,
        // Only names the latest scan, if a store exists (nothing is created).
        baseline: diskdrift_app::investigate::latest_baseline(&s),
    };
    let lister = platform::lister(ListerKind::Auto);
    let report = doctor::run(&opts, lister.as_ref(), &cancel, &progress);
    if interactive {
        eprint!("\r\x1b[2K");
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(Failure::Cancelled);
    }
    if args.json { print_json(&report) } else { print_text(&render::doctor(&report, s.size_units.value)) }
}

fn recover_cmd(g: &Global, budget: u64, json: bool) -> Outcome {
    let s = settings(g, None)?;
    let cancel = cancel_flag();
    let interactive = std::io::stderr().is_terminal();
    let progress = doctor_progress(interactive);
    let r = diskdrift_app::investigate::recover(&s, Duration::from_secs(budget), &cancel, None, &progress);
    if interactive {
        eprint!("\r\x1b[2K");
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(Failure::Cancelled);
    }
    let r = r?;
    if json { print_json(&r) } else { print_text(&render::recover(&r, s.size_units.value)) }
}

fn analyze_cmd(g: &Global, path: &str, budget: u64, limit: usize, json: bool) -> Outcome {
    use diskdrift_app::analyze::{self, AnalyzeStep};
    let s = settings(g, None)?;
    let cancel = cancel_flag();
    let interactive = std::io::stderr().is_terminal();
    let dir = analyze::resolve_path(&s, path);
    let step = |st: AnalyzeStep| {
        if interactive && let AnalyzeStep::Measured { name, .. } = st {
            eprint!("\r\x1b[2Kmeasured {name}…");
        }
    };
    let r = analyze::analyze(&s, &dir, limit, Duration::from_secs(budget), &cancel, None, &step);
    if interactive {
        eprint!("\r\x1b[2K");
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(Failure::Cancelled);
    }
    let r = r?;
    if json { print_json(&r) } else { print_text(&render::analyze(&r, s.size_units.value)) }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn disks_cmd(g: &Global, action: Option<DiskAction>, json: bool) -> Outcome {
    use diskdrift_app::disks::{self, Edit};
    let s = settings(g, None)?;
    let u = s.size_units.value;
    let edited = |d: diskdrift_app::storage::Disk| if json { print_json(&d) } else { print_text(&render::disk_entry(&d)) };
    match action {
        None => {
            let l = disks::list(&s)?;
            if json { print_json(&l) } else { print_text(&render::disk_list(&l, u, now_unix())) }
        }
        Some(DiskAction::Label { disk, label }) => edited(disks::edit(&s, &disk, Edit::Label(label.as_deref()))?),
        Some(DiskAction::Describe { disk, text }) => edited(disks::edit(&s, &disk, Edit::Describe(text.as_deref()))?),
        Some(DiskAction::Set { op: SetOp::Add, disk, set }) => edited(disks::edit(&s, &disk, Edit::AddToSet(&set))?),
        Some(DiskAction::Set { op: SetOp::Remove, disk, set }) => edited(disks::edit(&s, &disk, Edit::RemoveFromSet(&set))?),
        Some(DiskAction::Merge { from, into }) => {
            let (d, moved) = disks::merge(&s, &from, &into)?;
            if json {
                print_json(&serde_json::json!({ "disk": d, "moved": moved }))
            } else {
                print_text(&format!("Joined {} snapshot(s) to disk {} ({}).\n", moved.len(), d.display_name(), d.id))
            }
        }
        Some(DiskAction::Forget { disk, yes }) => {
            let plan = disks::forget(&s, &disk, false)?;
            if !yes {
                if json || !std::io::stdin().is_terminal() {
                    eprint!("{}", render::forget(&plan, u));
                    return Err(Failure::Usage("not forgotten: run again with --yes to confirm".into()));
                }
                eprint!("{}Forget it? [y/N] ", render::forget(&plan, u));
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    return print_text("Nothing was removed.\n");
                }
            }
            let done = disks::forget(&s, &plan.disk.id, true)?;
            if json { print_json(&done) } else { print_text(&render::forget(&done, u)) }
        }
    }
}

fn scan_cmd(g: &Global, args: ScanArgs) -> Outcome {
    use diskdrift_app::core::units::format_size;
    let s = settings(g, args.threads)?;
    if let Some(root) = args.root.as_deref() {
        platform::native_root(root).map_err(|e| Failure::Usage(format!("invalid scan root: {e}")))?;
    }
    let root = args.root.as_ref().map(PathBuf::from);
    let mut req = diskdrift_app::snapshots::ScanRequest::from_settings(&s, root, args.name.clone(), &args.exclude);
    req.disk_choice = args.disk.map(|d| match d {
        DiskArg::Same => diskdrift_app::disks::DiskChoice::Same,
        DiskArg::New => diskdrift_app::disks::DiskChoice::New,
    });
    req.careful = args.careful;
    if args.careful
        && let Some(w) = diskdrift_app::careful::read_only_warning(&req.root)
    {
        eprintln!("warning: {w}");
    }
    let cancel = cancel_flag();
    let progress = Arc::new(diskdrift_app::core::walk::Progress::default());
    let interactive = std::io::stderr().is_terminal();
    let done = Arc::new(AtomicBool::new(false));
    let reporter = {
        let (progress, done, units) = (progress.clone(), done.clone(), s.size_units.value);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                if !interactive {
                    continue;
                }
                let n = progress.entries.load(Ordering::Relaxed);
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let cur = progress.current.lock().map(|c| c.clone()).unwrap_or_default();
                let cur: String = if cur.chars().count() > 60 { format!("…{}", cur.chars().rev().take(59).collect::<Vec<_>>().into_iter().rev().collect::<String>()) } else { cur };
                eprint!(
                    "\r\x1b[2K{n} entries · {} · {:.0}k/s · {:.0}s · {cur}",
                    format_size(progress.bytes_alloc.load(Ordering::Relaxed), units),
                    n as f64 / secs / 1000.0,
                    secs
                );
            }
            if interactive {
                eprint!("\r\x1b[2K");
            }
        })
    };
    if interactive {
        eprintln!(
            "Scanning {} → snapshot store {} (Ctrl-C cancels)",
            req.root.display(),
            s.storage_path.value.display()
        );
    }
    let result = diskdrift_app::snapshots::scan(&s, &req, &cancel, Some(&progress));
    done.store(true, Ordering::SeqCst);
    let _ = reporter.join();
    let outcome = result?;
    if args.json {
        print_json(&outcome)?;
    } else {
        print_text(&render::scan_outcome(&outcome, s.size_units.value))?;
    }
    if outcome.cancelled { Err(Failure::Cancelled) } else { Ok(()) }
}
