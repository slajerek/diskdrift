//! Full-screen terminal UI: a thin layer over `diskdrift-app` services.

pub mod app;
pub mod ui;

use app::{App, Command, Msg};
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use diskdrift_app::config::Effective;
use diskdrift_app::diagnostics::doctor::{self, DoctorOptions};
use diskdrift_app::platform::{self, ListerKind};
use diskdrift_app::search::{self, Query};
use diskdrift_app::snapshots;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Restores the terminal when dropped (normal exit, error or unwinding panic).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Runs the TUI until the user quits.
pub fn run(settings: Effective) -> anyhow::Result<()> {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));

    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let result = event_loop(&mut terminal, settings);
    drop(terminal);
    drop(guard);
    let _ = std::panic::take_hook();
    result
}

struct Workers {
    tx: Sender<Msg>,
    /// Cancels the doctor run.
    cancel: Arc<AtomicBool>,
    /// Cancels the running scan.
    scan_cancel: Arc<AtomicBool>,
    /// Cancels the running folder analysis.
    analyze_cancel: Arc<AtomicBool>,
    /// Skip the location the shared doctor/recover measurement is at.
    doctor_skip: Arc<AtomicBool>,
    /// Cancels the running search (newest scans or history).
    find_cancel: Arc<AtomicBool>,
    /// Careful mode: while it is held, external disks are not mounted.
    guard: Option<Arc<diskdrift_app::platform::MountGuard>>,
    careful_cancel: Arc<AtomicBool>,
    /// Stops a removal or compaction.
    store_cancel: Arc<AtomicBool>,
}

fn spawn<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::spawn(f);
}

impl Workers {
    fn start(&mut self, cmd: Command, app: &App) {
        let tx = self.tx.clone();
        // The settings can change at run time (Settings screen).
        let s = app.settings.clone();
        match cmd {
            Command::LoadUsage => spawn(move || {
                let _ = tx.send(Msg::Usage(Box::new(diskdrift_app::usage(false))));
            }),
            Command::LoadSnapshots => spawn(move || {
                let r = snapshots::list(&s).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Snapshots(Box::new(r)));
            }),
            Command::RunDoctor { live } => {
                self.cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.cancel.clone();
                self.doctor_skip = Arc::new(AtomicBool::new(false));
                let skip = self.doctor_skip.clone();
                spawn(move || {
                    let opts = DoctorOptions {
                        home: s.home.clone(),
                        reference_free: None,
                        budget: Duration::from_secs(120),
                        threads: s.threads.value,
                        skip: Some(skip),
                        // A recent scan already has the sizes (at least an hour's worth).
                        known: if live {
                            None
                        } else {
                            let max_age = s.auto_max_age_hours.value.max(1) as i64 * 3600;
                            diskdrift_app::investigate::known_sizes(&s, max_age, chrono::Local::now().timestamp()).ok().flatten().map(Arc::new)
                        },
                        baseline: diskdrift_app::investigate::latest_baseline(&s),
                    };
                    let lister = platform::lister(ListerKind::Auto);
                    let progress_tx = tx.clone();
                    let report = doctor::run(&opts, lister.as_ref(), &cancel, &move |step| {
                        let _ = progress_tx.send(Msg::DoctorStep(step));
                    });
                    // Recovery suggestions come from the same measurement.
                    let _ = tx.send(Msg::Doctor(Box::new(report.clone())));
                    let r = diskdrift_app::investigate::recover_from(&s, &report).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Recover(Box::new(r)));
                });
            }
            Command::LoadDisks => {
                let mounted = app.volumes.clone();
                spawn(move || {
                    let r = match mounted {
                        Some(v) => diskdrift_app::disks::list_with(&s, &v),
                        None => diskdrift_app::disks::list(&s),
                    };
                    let _ = tx.send(Msg::Disks(Box::new(r.map_err(|e| e.to_string()))));
                })
            }
            Command::DiskConnected(volume) => spawn(move || {
                let now = chrono::Local::now().timestamp();
                let action = diskdrift_app::disks::on_connect(&s, &volume, now).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Connected { volume, action: Box::new(action) });
            }),
            Command::StartScan { root, name, background, disk } => {
                self.scan_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.scan_cancel.clone();
                let progress = app.scan_progress().unwrap_or_default();
                spawn(move || {
                    let mut req = snapshots::ScanRequest::from_settings(&s, Some(root.into()), name, &[]);
                    req.background = background;
                    req.disk_choice = disk;
                    let msg = match snapshots::scan(&s, &req, &cancel, Some(progress.as_ref())) {
                        Err(snapshots::AppError::DiskDoubt(m)) => Msg::ScanDoubt(m),
                        r => Msg::ScanDone(Box::new(r.map_err(|e| e.to_string()))),
                    };
                    let _ = tx.send(msg);
                });
            }
            Command::CancelScan => self.scan_cancel.store(true, Ordering::SeqCst),
            Command::RunDiff { old, new, sort, min_size } => spawn(move || {
                let opts = diskdrift_app::core::diff::DiffOptions { sort, min_size, limit: crate::app::DIFF_LIMIT, ..Default::default() };
                let r = snapshots::diff(&s, &old.to_string(), &new.to_string(), None, false, opts).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Diff(Box::new(r)));
            }),
            Command::RunTop { snapshot, metric } => spawn(move || {
                let opts = diskdrift_app::core::top::TopOptions { limit: 200, metric, deepest: true, ..Default::default() };
                let r = snapshots::top(&s, &snapshot.to_string(), None, opts).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Top(Box::new(r)));
            }),
            Command::RunAnalyze { id, path, live } => {
                self.analyze_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.analyze_cancel.clone();
                let progress = app.analysis_progress().unwrap_or_default();
                spawn(move || {
                    let step_tx = tx.clone();
                    let step = move |step| {
                        let _ = step_tx.send(Msg::AnalyzeStep { id, step });
                    };
                    // A recent scan has the sizes already; F5 (live) measures now.
                    let max_age = s.auto_max_age_hours.value.max(1) as i64 * 3600;
                    let from_scan = if live {
                        None
                    } else {
                        diskdrift_app::analyze::analyze_from_scan(&s, &path, 500, max_age, chrono::Local::now().timestamp()).ok().flatten()
                    };
                    let r = match from_scan {
                        Some(r) => Ok(r),
                        None => diskdrift_app::analyze::analyze(&s, &path, 500, Duration::from_secs(120), &cancel, Some(progress.as_ref()), &step)
                            .map_err(|e| e.to_string()),
                    };
                    let _ = tx.send(Msg::Analyze { id, result: Box::new(r) });
                });
            }
            Command::RunBrowse { id, source, path } => spawn(move || {
                const LIMIT: usize = 1000;
                let r = match source {
                    app::BrowseSource::Snapshot(snap) => {
                        snapshots::browse(&s, &snap.to_string(), path.as_deref(), LIMIT).map(|r| app::BrowseData::Snapshot(Box::new(r)))
                    }
                    app::BrowseSource::Diff { old, new } => {
                        snapshots::diff_browse(&s, &old.to_string(), &new.to_string(), path.as_deref(), LIMIT).map(|r| app::BrowseData::Diff(Box::new(r)))
                    }
                };
                let _ = tx.send(Msg::Browse { id, result: Box::new(r.map_err(|e| e.to_string())) });
            }),
            Command::RunFind { id, pattern, disk, set } => {
                self.find_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.find_cancel.clone();
                let mounted = app.volumes.clone();
                spawn(move || {
                    let q = Query { disk, set, limit: app::FIND_LIMIT, ..Query::new(&pattern) };
                    let r = search::current_cancellable(&s, &q, mounted.as_deref(), &cancel).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::FindCurrent { id, result: Box::new(r) });
                });
            }
            Command::RunFindHistory { id, always } => {
                let (Some(q), Some(cur)) = (app.find.query.clone(), app.find.current.done().cloned()) else { return };
                self.find_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.find_cancel.clone();
                spawn(move || {
                    let progress_tx = tx.clone();
                    let progress = move |done, total| {
                        let _ = progress_tx.send(Msg::FindProgress { id, done, total });
                    };
                    let r = search::history(&s, &q, &cur, always, &cancel, &progress).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::FindHistory { id, result: Box::new(r) });
                });
            }
            Command::CancelFind => self.find_cancel.store(true, Ordering::SeqCst),
            Command::Careful(true) => {
                let events_tx = tx.clone();
                let settings = s.clone();
                let started = diskdrift_app::platform::MountGuard::start(Box::new(move |e| match e {
                    diskdrift_app::platform::GuardEvent::Held(v) => {
                        // Off the guard thread: reading the store must not hold up Disk Arbitration.
                        let (tx, settings) = (events_tx.clone(), settings.clone());
                        spawn(move || {
                            let r = diskdrift_app::careful::describe(&settings, *v).map_err(|e| e.to_string());
                            let _ = tx.send(Msg::CarefulHeld(Box::new(r)));
                        });
                    }
                    diskdrift_app::platform::GuardEvent::Gone(bsd) => {
                        let _ = events_tx.send(Msg::CarefulGone(bsd));
                    }
                }));
                let r = started.map(|g| self.guard = Some(Arc::new(g))).map_err(|e| e.to_string());
                let _ = tx.send(Msg::CarefulStarted(r));
            }
            // Dropping the guard ends the policy; a running careful scan keeps its own handle until it has ejected.
            Command::Careful(false) => self.guard = None,
            Command::CarefulScan { bsd, choice } => {
                let (Some(guard), Some(held)) = (self.guard.clone(), app.careful.held.iter().find(|h| h.bsd() == bsd).cloned()) else { return };
                self.careful_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.careful_cancel.clone();
                let progress = app.careful_progress().unwrap_or_default();
                spawn(move || {
                    let r = diskdrift_app::careful::careful_scan(&s, &guard, &held, choice, &cancel, Some(progress.as_ref())).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::CarefulDone { bsd, result: Box::new(r) });
                });
            }
            Command::CarefulMount { bsd } | Command::CarefulEject { bsd } if self.guard.is_none() => {
                let _ = tx.send(Msg::CarefulAction { bsd, result: Err("careful mode is off".into()) });
            }
            Command::CarefulMount { bsd } => {
                let (Some(guard), Some(held)) = (self.guard.clone(), app.careful.held.iter().find(|h| h.bsd() == bsd).cloned()) else { return };
                spawn(move || {
                    let r = diskdrift_app::careful::mount_for_finder(&guard, &held).map(Some).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::CarefulAction { bsd, result: r });
                });
            }
            Command::CarefulEject { bsd } => {
                let (Some(guard), Some(held)) = (self.guard.clone(), app.careful.held.iter().find(|h| h.bsd() == bsd).cloned()) else { return };
                spawn(move || {
                    let r = diskdrift_app::careful::eject(&guard, &held).map(|()| None).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::CarefulAction { bsd, result: r });
                });
            }
            Command::CancelCareful => self.careful_cancel.store(true, Ordering::SeqCst),
            Command::RemoveSnapshots { ids } => {
                self.store_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.store_cancel.clone();
                spawn(move || {
                    let r = diskdrift_app::prune::remove(&s, &ids, &cancel).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Removed(Box::new(r)));
                });
            }
            Command::Compact => {
                self.store_cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.store_cancel.clone();
                spawn(move || {
                    let progress_tx = tx.clone();
                    let r = diskdrift_app::prune::compact(&s, &cancel, &mut |step| {
                        let _ = progress_tx.send(Msg::StoreProgress(diskdrift_app::text::prune::compact_step(&step)));
                    })
                    .map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Compacted(Box::new(r)));
                });
            }
            Command::CancelStoreJob => self.store_cancel.store(true, Ordering::SeqCst),
            Command::CancelAnalyze => self.analyze_cancel.store(true, Ordering::SeqCst),
            Command::SkipDoctorStep => self.doctor_skip.store(true, Ordering::SeqCst),
            Command::CancelJobs => {
                self.cancel.store(true, Ordering::SeqCst);
                self.scan_cancel.store(true, Ordering::SeqCst);
                self.analyze_cancel.store(true, Ordering::SeqCst);
                self.find_cancel.store(true, Ordering::SeqCst);
                self.careful_cancel.store(true, Ordering::SeqCst);
                self.store_cancel.store(true, Ordering::SeqCst);
            }
            Command::Quit => {}
        }
    }
}

/// Reads the mount table every 2 seconds (cheap) and describes the volumes
/// (Disk Arbitration) only when the set of mounts changed, starting with the
/// volumes mounted now (the baseline). Stops with the app.
fn watch_mounts(tx: Sender<Msg>, stop: Arc<AtomicBool>) {
    spawn(move || {
        let mut last: Option<Vec<std::path::PathBuf>> = None;
        while !stop.load(Ordering::SeqCst) {
            let now = diskdrift_app::disks::disk_mount_points();
            if last.as_ref() != Some(&now) {
                if tx.send(Msg::Volumes(diskdrift_app::disks::volumes())).is_err() {
                    return;
                }
                last = Some(now);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, settings: Effective) -> anyhow::Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = unbounded();
    let mut workers = Workers {
        tx,
        cancel: Arc::new(AtomicBool::new(false)),
        scan_cancel: Arc::new(AtomicBool::new(false)),
        analyze_cancel: Arc::new(AtomicBool::new(false)),
        doctor_skip: Arc::new(AtomicBool::new(false)),
        find_cancel: Arc::new(AtomicBool::new(false)),
        guard: None,
        careful_cancel: Arc::new(AtomicBool::new(false)),
        store_cancel: Arc::new(AtomicBool::new(false)),
    };
    let mut app = App::new(settings);
    for c in app.startup() {
        workers.start(c, &app);
    }
    watch_mounts(workers.tx.clone(), workers.cancel.clone());
    let mut dirty = true;
    let mut last_draw = std::time::Instant::now();
    while !app.should_quit {
        while let Ok(msg) = rx.try_recv() {
            for c in app.handle_msg(msg) {
                workers.start(c, &app);
            }
            dirty = true;
        }
        for c in app.idle() {
            workers.start(c, &app);
            dirty = true;
        }
        // Live progress needs periodic redraws while a scan runs.
        if app.needs_ticks() && last_draw.elapsed() >= Duration::from_millis(250) {
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, &app))?;
            dirty = false;
            last_draw = std::time::Instant::now();
        }
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    for c in app.handle_key(key) {
                        workers.start(c, &app);
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
    workers.start(Command::CancelJobs, &app);
    // Give a cancelled scan a moment to record itself as CANCELLED.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while app.scan_running() && std::time::Instant::now() < deadline {
        if let Ok(Msg::ScanDone(_)) = rx.recv_timeout(Duration::from_millis(100)) {
            break;
        }
    }
    // A stopped careful scan still unmounts and ejects its disk: wait for that.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while app.careful.running.is_some() && std::time::Instant::now() < deadline {
        if let Ok(Msg::CarefulDone { .. }) = rx.recv_timeout(Duration::from_millis(100)) {
            break;
        }
    }
    // Careful mode ends here (the guard is dropped with the workers).
    drop(workers);
    Ok(())
}
