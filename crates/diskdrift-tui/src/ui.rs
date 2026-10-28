//! Rendering. Every screen renders only what fits; long lists are sliced to the
//! visible window before building widgets.

use crate::app::{AnalysisView, App, BrowseData, BrowseSource, DIFF_LIMIT, DIFF_TABS, DiffFocus, DiffTab, FindRow, Job, TopFocus, LogLine, MENU, ScanField, ScanStage, Screen, TopView};
use diskdrift_app::storage::Strength;
use diskdrift_app::text::browse::{child_changed, child_detail, child_name, child_size, diff_change, diff_detail, share};
use diskdrift_app::diagnostics::classify::verdict;
use diskdrift_app::diagnostics::suspects::Risk;
use diskdrift_app::diagnostics::usage::is_low;
use diskdrift_app::text::analyze::{has_owners, item_changed, item_detail, item_owner, item_size, shown_verdict};
use diskdrift_app::core::diff::{Change, DiffItem, DiffSort};
use diskdrift_app::core::top::{SizeMetric, TopItem};
use diskdrift_app::core::units::{SizeUnits, format_count, format_delta, format_size};
use diskdrift_app::text::table::ellipsize_start;
use diskdrift_app::snapshots::{self, SnapshotRow};
use diskdrift_app::storage::{SnapshotMeta, Status};
use diskdrift_app::text;
use diskdrift_app::config::FieldKind;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use diskdrift_app::diagnostics::suspects::MeasureStatus;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use std::sync::atomic::Ordering;

const ACCENT: Color = Color::Cyan;

fn units(app: &App) -> SizeUnits {
    app.settings.size_units.value
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    draw_header(f, app, header);
    match app.screen {
        Screen::Dashboard => draw_dashboard(f, app, body),
        Screen::Usage => draw_job_text(f, app, body, &app.usage, usage_text),
        Screen::Doctor if app.doctor.is_running() => draw_measuring(f, app, body),
        Screen::Doctor => draw_job_text(f, app, body, &app.doctor, |r, a| text::doctor_summary_first(r, units(a))),
        Screen::Help => draw_text(f, app, body, &help_text(app)),
        Screen::Scan => draw_scan(f, app, body),
        Screen::Snapshots => draw_snapshots(f, app, body),
        Screen::Diff => draw_diff(f, app, body),
        Screen::Top => draw_top(f, app, body),
        Screen::Recover => draw_recover(f, app, body),
        Screen::Browse => draw_browse(f, app, body),
        Screen::Settings => draw_settings(f, app, body),
        Screen::Disks => draw_disks(f, app, body),
        Screen::Find => draw_find(f, app, body),
        Screen::Careful => draw_careful(f, app, body),
        Screen::Analyze => match app.analysis.stack.last() {
            Some(view) => draw_analysis(f, app, body, view),
            None => draw_text(f, app, body, "Nothing to analyse."),
        },
    }
    draw_footer(f, app, footer);
    if app.confirm_exit {
        let msg = if app.scan_running() {
            "Quit DiskDrift? The running scan stops and nothing of it is kept. Enter quits · Esc stays"
        } else {
            "Quit DiskDrift? Enter quits · Esc stays"
        };
        draw_confirm(f, area, msg);
    } else if app.confirm_quit {
        draw_confirm(f, area, "A scan is running. Cancel it and quit? [y/N]");
    } else if app.confirm_cancel {
        let msg = if app.auto_scan_running() {
            "Stop the background scan? Nothing of it is kept. [y/N]"
        } else {
            "Stop the running scan? Nothing of it is kept. [y/N]"
        };
        draw_confirm(f, area, msg);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" DiskDrift ", Style::new().fg(Color::Black).bg(ACCENT).bold())];
    // Where you are comes first: on a narrow terminal the volume figures are cut, not this.
    if app.screen != Screen::Dashboard {
        spans.push(Span::styled(format!(" › {}", app.screen.title()), Style::new().fg(ACCENT).bold()));
    }
    if let Some(d) = &app.disk_filter
        && matches!(app.screen, Screen::Snapshots | Screen::Diff | Screen::Top | Screen::Scan | Screen::Browse | Screen::Find)
    {
        spans.push(Span::styled(format!(" · {}", d.label()), Style::new().fg(Color::Yellow).bold()));
    }
    // Careful mode changes how macOS mounts disks: always visible while on.
    if app.careful.is_on() && app.screen != Screen::Careful {
        spans.push(Span::styled(" CAREFUL MODE ", Style::new().fg(Color::White).bg(Color::Red).bold()));
    }
    spans.push(Span::raw("   "));
    match &app.usage {
        Job::Done(u) => {
            if let Some(v) = u.primary() {
                let avail = v.available_important_bytes.unwrap_or(v.available_bytes);
                // Narrow: the volume name goes, the free space and the warning stay.
                if area.width >= 100 {
                    spans.push(Span::raw(format!("{} · {}   ", v.name, v.fs_type.to_uppercase())));
                }
                spans.push(Span::raw("Available: "));
                spans.push(Span::styled(format_size(avail, units(app)), Style::new().bold()));
                if is_low(avail, v.total_bytes) {
                    spans.push(Span::styled("   [!] Low", Style::new().fg(Color::Red).bold()));
                }
            }
        }
        Job::Running(m) => spans.push(Span::raw(m.clone())),
        Job::Failed(e) => spans.push(Span::styled(e.clone(), Style::new().fg(Color::Red))),
        Job::Idle => {}
    }
    if let Some(e) = app.disk_event.as_ref().filter(|_| app.screen != Screen::Dashboard && app.disk_event_is_recent()) {
        spans.push(Span::styled(format!("   [{}]", clip(&e.text, 60)), Style::new().fg(Color::Yellow)));
    }
    if app.auto_scan_running() {
        spans.push(Span::styled("   [background scan · S shows it · x stops]", Style::new().fg(Color::Yellow)));
    } else if app.scan_running() && app.screen != Screen::Scan {
        spans.push(Span::styled("   [scanning…]", Style::new().fg(Color::Yellow)));
    }
    let p = Paragraph::new(Line::from(spans)).block(Block::new().borders(Borders::ALL).border_style(Style::new().fg(ACCENT)));
    f.render_widget(p, area);
}

/// The disks panel of the dashboard: every disk the store knows and the new
/// volumes connected now, newest scan first (spec 3A, item 1).
fn disk_panel_lines(app: &App, max: usize, width: u16) -> Vec<Line<'static>> {
    let Some(l) = app.disks.list.done() else { return Vec::new() };
    let u = units(app);
    let now = chrono::Local::now().timestamp();
    let ago = |rfc: &str| chrono::DateTime::parse_from_rfc3339(rfc).map(|t| diskdrift_app::analyze::ago(now - t.timestamp())).unwrap_or_else(|_| rfc.into());
    let mut lines: Vec<Line> = l
        .disks
        .iter()
        .take(max)
        .map(|r| {
            let name = format!(" {:<16} ", clip(r.disk.display_name(), 16));
            let (state, rest) = match (r.known, &r.last_scan) {
                (false, _) => ("new".to_string(), "not scanned · K to label or scan it".to_string()),
                (true, Some(s)) => (
                    if r.connected { "connected".into() } else { "not connected".into() },
                    format!(
                        "scanned {}{}",
                        ago(s),
                        // The size is of the folder that scan covered, which may not be the whole disk.
                        r.last_size
                            .map(|b| {
                                let whole = r.last_root.as_deref() == r.mount_point.as_deref().and_then(|p| p.to_str()) || r.last_root.as_deref() == Some("/");
                                let home = app.settings.home.display().to_string();
                                let place = match r.last_root.as_deref() {
                                    Some(_) if whole => "on the disk".to_string(),
                                    // What is left of the line after the name, the state and "scanned … · size in … then".
                                    Some(root) => format!("in {}", home_path(root, &home, width.saturating_sub(2 + 33 + 34).max(12))),
                                    None => "in it".to_string(),
                                };
                                format!(" · {} {place}", format_size(b, u))
                            })
                            .unwrap_or_default()
                    ),
                ),
                (true, None) => (if r.connected { "connected".into() } else { "not connected".into() }, "no scan yet".into()),
            };
            let style = if r.connected { Style::new() } else { Style::new().fg(Color::DarkGray) };
            Line::styled(format!("{name}{state:<14} {rest}"), style)
        })
        .collect();
    if l.disks.len() > max {
        lines.push(Line::styled(format!(" … and {} more: K (My disks)", l.disks.len() - max), Style::new().fg(Color::DarkGray)));
    }
    lines
}

fn draw_dashboard(f: &mut Frame, app: &App, area: Rect) {
    // The disks panel gets the rows the menu and status do not need.
    let menu_h = MENU.len().div_ceil(2) as u16 + 3;
    let status_h = if app.disk_event.is_some() { 7 } else { 6 };
    let room = area.height.saturating_sub(menu_h + status_h).saturating_sub(2) as usize;
    let disk_lines = disk_panel_lines(app, room.saturating_sub(1).max(1), area.width);
    let disks_h = if disk_lines.is_empty() || room < 2 { 0 } else { disk_lines.len() as u16 + 2 };
    let [menu_area, disks_area, status_area] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(disks_h), Constraint::Length(status_h)]).areas(area);
    if disks_h > 0 {
        f.render_widget(Paragraph::new(disk_lines).block(Block::bordered().title(" Disks ")), disks_area);
    }
    let mut lines = vec![Line::raw("")];
    for row in MENU.chunks(2) {
        let mut spans = Vec::new();
        for s in row {
            let idx = MENU.iter().position(|m| m == s).unwrap();
            let selected = idx == app.menu_index;
            let marker = if selected { " > " } else { "   " };
            let label = format!("{marker}[{}] {:<24}", s.hotkey(), s.title());
            spans.push(if selected {
                Span::styled(label, Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD))
            } else {
                Span::raw(label)
            });
            spans.push(Span::raw("  "));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Menu ")), menu_area);

    let s = &app.settings;
    let status = vec![
        Line::from(vec![Span::raw(" Last scan: "), Span::styled(last_scan_text(app), Style::new().bold())]),
        Line::raw(format!(" Latest change: {}", latest_change_text(app))),
        Line::raw(format!(" Storage: {}", s.storage_path.value.display())),
        scan_status_line(app),
    ];
    let mut status = status;
    if let Some(e) = &app.disk_event {
        status.push(Line::styled(format!(" {} {}", e.at, e.text), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(status).block(Block::bordered()), status_area);
}

/// The scan running now (live counters and an estimate against the previous
/// scan), or what the automatic scan did.
fn scan_status_line(app: &App) -> Line<'static> {
    if let ScanStage::Running { progress, started, root, auto, expected } = &app.scan {
        let n = progress.entries.load(Ordering::Relaxed);
        let pct = expected.filter(|&e| e > 0).map(|e| format!(" · ~{:.0}% of the last scan's entries", (n as f64 * 100.0 / e as f64).min(99.0))).unwrap_or_default();
        let label = if *auto { "Background scan" } else { "Scan" };
        let stop = if *auto { " · x stops it" } else { " · S to watch · x stops it" };
        return Line::from(vec![
            Span::styled(format!(" {label}: "), Style::new().fg(Color::Yellow).bold()),
            Span::raw(format!(
                "{root} · {} entries · {}{pct} · {:.0}s{stop}",
                format_count(n),
                format_size(progress.bytes_alloc.load(Ordering::Relaxed), units(app)),
                started.elapsed().as_secs_f64()
            )),
        ]);
    }
    match &app.auto_status {
        Some(m) => Line::styled(format!(" {}", capitalize(m)), Style::new().fg(Color::DarkGray)),
        None => Line::raw(""),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn short_time(rfc: &str) -> String {
    rfc.get(..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| rfc.to_string())
}

fn last_scan_text(app: &App) -> String {
    match &app.snapshots {
        // The latest with data: a cancelled or failed one is not "the last scan".
        Job::Done(l) => match l.snapshots.iter().rev().find(|r| r.meta.status.is_usable()).or(l.snapshots.last()) {
            Some(r) => {
                let m = &r.meta;
                let cov = match m.status {
                    Status::Completed => "complete".to_string(),
                    Status::Partial => "partial coverage".to_string(),
                    other => other.to_string(),
                };
                format!("{} · {} · {}", short_time(&m.started_at), m.name, cov)
            }
            None => "none yet — press S to create a baseline".into(),
        },
        Job::Running(_) | Job::Idle => "…".into(),
        Job::Failed(e) => format!("unavailable ({e})"),
    }
}

fn latest_change_text(app: &App) -> String {
    let u = app.usable();
    if u.len() < 2 {
        return "needs two scans (press D to compare once you have them)".into();
    }
    let (a, b) = (&u[u.len() - 2].meta, &u[u.len() - 1].meta);
    let tracked = |m: &diskdrift_app::storage::SnapshotMeta| m.counts.as_ref().map(|c| c.allocated_bytes as i128).unwrap_or(0);
    let free = match (&a.space_after, &b.space_after) {
        (Some(x), Some(y)) => format!(", OS free {}", format_delta(y.free_bytes as i128 - x.free_bytes as i128, units(app))),
        _ => String::new(),
    };
    format!("{} → {}: tracked {}{free}", a.name, b.name, format_delta(tracked(b) - tracked(a), units(app)))
}

/// The answer first, then the full table.
fn usage_text(r: &diskdrift_app::diagnostics::usage::UsageReport, app: &App) -> String {
    let at = app.usage_at.as_deref().map(|t| format!(" (read at {t}; F5 reads again)")).unwrap_or_default();
    let mut out = format!("In short{at}:\n");
    for l in text::usage_summary(r, units(app)) {
        out.push_str(&format!("  • {l}\n"));
    }
    out.push_str("Where the space went: I (Doctor). What grows over time: S scans now, D compares with a later scan.\n\nDetails\n");
    out.push_str(&text::usage(r, units(app)));
    out
}

fn draw_job_text<T>(f: &mut Frame, app: &App, area: Rect, job: &Job<T>, render: impl Fn(&T, &App) -> String) {
    match job {
        Job::Idle => draw_text(f, app, area, "Starting…"),
        Job::Running(msg) => draw_text(f, app, area, &format!("{msg}\n\nEsc goes back; the task keeps running.")),
        Job::Failed(e) => draw_text(f, app, area, &format!("Failed: {e}")),
        Job::Done(r) => draw_text(f, app, area, &render(r, app)),
    }
}

/// Rows `text` takes when wrapped to `width` columns.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    text.lines().map(|l| l.chars().count().max(1).div_ceil(w) as u16).sum()
}

/// Long lines wrap rather than being cut: nothing of a report is lost.
fn draw_text(f: &mut Frame, app: &App, area: Rect, body: &str) {
    let lines = wrapped_rows(body, area.width.saturating_sub(2));
    let visible = area.height.saturating_sub(2);
    let scroll = app.scroll.min(lines.saturating_sub(visible));
    let p = Paragraph::new(Text::raw(body.to_string()))
        .block(Block::bordered().title(format!(" {} ", app.screen.title())))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_scan(f: &mut Frame, app: &App, area: Rect) {
    let u = units(app);
    let block = Block::bordered().title(" Scan disk ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let home = app.settings.home.display().to_string();
    let store = home_path(&app.settings.storage_path.value.display().to_string(), &home, inner.width.saturating_sub(20));
    let dim = Style::new().fg(Color::DarkGray);
    // What the previous scan of this folder took: how long this one will.
    let last_time = |root: &str| match app.last_scan_of(root) {
        Some(m) => match &m.counts {
            Some(c) => format!(
                "{}: {} entries, {}, took {:.0} s",
                scan_title(&m),
                format_count(c.entries),
                format_size(c.allocated_bytes, u),
                c.elapsed_ms as f64 / 1000.0
            ),
            None => scan_title(&m),
        },
        None => "never: this is the first scan of it".into(),
    };
    let mut lines: Vec<Line> = Vec::new();
    match &app.scan {
        ScanStage::Setup { root, name, field, error } => {
            let fieldline = |label: &str, value: &str, active: bool| {
                let style = if active { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().add_modifier(Modifier::UNDERLINED) };
                Line::from(vec![
                    Span::raw(format!(" {label:<18}")),
                    Span::styled(format!("{value}{}", if active { "▏" } else { " " }), style),
                ])
            };
            let note = |text: String, style: Style| Line::styled(format!(" {:<18}{text}", ""), style);
            lines.push(Line::raw(""));
            lines.push(fieldline("Folder to scan:", root, *field == ScanField::Root));
            match error {
                Some(e) => lines.push(note(e.clone(), Style::new().fg(Color::Red))),
                None => lines.push(note("/ the whole disk · ~ your home folder · or any folder, e.g. ~/Library".into(), dim)),
            }
            lines.push(Line::raw(""));
            lines.push(fieldline("Name (optional):", name, *field == ScanField::Name));
            lines.push(note("Empty: named by its start time. A name helps find it in the history.".into(), dim));
            lines.push(Line::raw(""));
            let last = snapshots::check_root(root, &app.settings.home).map(|p| last_time(&p.display().to_string()));
            lines.push(Line::raw(format!(" {:<18}{}", "Last scanned:", last.unwrap_or_else(|_| "-".into()))));
            lines.push(Line::raw(format!(" {:<18}{store}", "Saved to:")));
            lines.push(Line::raw(""));
            lines.push(Line::styled(" Reads names, sizes and dates only: never file contents, never changes a file.", dim));
        }
        ScanStage::Confirm { root, name } => {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(" {:<18}{root}", "Scan:")));
            lines.push(Line::raw(format!(" {:<18}{}", "Name:", if name.is_empty() { "(its start time)" } else { name })));
            lines.push(Line::raw(format!(" {:<18}{}", "Last scanned:", last_time(root))));
            lines.push(Line::raw(format!(" {:<18}{store}", "Saved to:")));
            lines.push(Line::raw(""));
            lines.push(Line::styled(" Enter starts the scan · Esc changes it", Style::new().bold()));
            lines.push(Line::styled(" (Settings: \"Confirm before a scan\" off skips this step.)", dim));
        }
        ScanStage::AskDisk { root, message, .. } => {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(" {:<18}{root}", "Scan:")));
            lines.push(Line::raw(""));
            lines.push(Line::styled(" Is this the disk DiskDrift knows?", Style::new().bold().fg(Color::Yellow)));
            lines.push(Line::raw(format!(" {message}")));
            lines.push(Line::raw(""));
            lines.push(Line::raw(" s  the same disk: its scans join that disk's history"));
            lines.push(Line::raw(" n  another disk: it gets an entry of its own"));
            lines.push(Line::raw(" Esc  not now (nothing is scanned)"));
            lines.push(Line::raw(""));
            lines.push(Line::styled(" DiskDrift asks every time, because a copy of a disk looks the same to it.", dim));
        }
        ScanStage::Running { progress, started, root, expected, .. } => {
            let n = progress.entries.load(Ordering::Relaxed);
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let cur = progress.current.lock().map(|c| c.clone()).unwrap_or_default();
            let issues = progress.issues.load(Ordering::Relaxed);
            let of_last = expected
                .filter(|&e| e > 0)
                .map(|e| format!("   (the last scan of it had {}: about {:.0}% so far)", format_count(e), (n as f64 * 100.0 / e as f64).min(99.0)))
                .unwrap_or_default();
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(" Scanning {root}")).bold());
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![Span::raw(" Entries:    "), Span::styled(format_count(n), Style::new().bold()), Span::styled(of_last, dim)]));
            lines.push(Line::raw(format!(" Allocated:  {}", format_size(progress.bytes_alloc.load(Ordering::Relaxed), u))));
            lines.push(Line::raw(format!(" Rate:       {:.0}k entries/s · {:.0} s", n as f64 / secs / 1000.0, secs)));
            let unreadable = if issues == 0 { "none so far".to_string() } else { format!("{} (folders macOS protects; listed when the scan ends)", format_count(issues)) };
            lines.push(Line::raw(format!(" Unreadable: {unreadable}")));
            lines.push(Line::raw(format!(" Now in:     {}", tail(&cur, inner.width.saturating_sub(14) as usize))));
            lines.push(Line::raw(""));
            lines.push(Line::styled(" ← back to the menu (the scan goes on) · Esc or x stops it (asks first)", dim));
        }
        ScanStage::Finished(r) => {
            lines.push(Line::raw(""));
            match r {
                Ok(o) => {
                    let prev = app.previous_comparable(&o.snapshot).filter(|_| !o.cancelled);
                    let now = chrono::Local::now().timestamp();
                    for (i, l) in text::scan_summary(o, prev.as_ref(), now, u).into_iter().enumerate() {
                        lines.push(if i == 0 { Line::raw(format!(" {l}")).bold() } else { Line::raw(format!(" {l}")) });
                    }
                    lines.push(Line::raw(""));
                    let mut keys = Vec::new();
                    if o.snapshot.status.is_usable() && !o.cancelled {
                        keys.push("Enter browse this scan".to_string());
                        if let Some(p) = &prev {
                            keys.push(format!("c what changed since #{}", p.id));
                        }
                    }
                    keys.push("n new scan".into());
                    keys.push("Esc back".into());
                    lines.push(Line::styled(format!(" {}", keys.join(" · ")), Style::new().fg(ACCENT)));
                    if !o.cancelled {
                        lines.push(Line::raw(""));
                        lines.push(Line::styled(" Details", dim));
                        lines.extend(text::scan_outcome(o, u).lines().skip(1).map(|l| Line::raw(format!(" {l}"))));
                    }
                }
                Err(e) => {
                    lines.push(Line::raw(format!(" Scan failed: {e}")).bold());
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(" Enter back to the form (the folder is kept) · Esc back", Style::new().fg(ACCENT)));
                }
            }
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 2 { s.to_string() } else { format!("…{}", s.chars().skip(n - max + 1).collect::<String>()) }
}

/// Rows `scroll..scroll+height` of `items`, with the scroll clamped.
fn window<T>(items: &[T], scroll: u16, height: u16) -> (&[T], usize) {
    let h = height as usize;
    let start = (scroll as usize).min(items.len().saturating_sub(h.max(1)));
    (&items[start..(start + h).min(items.len())], start)
}

fn snapshot_cells(r: &SnapshotRow, u: SizeUnits) -> Vec<String> {
    let m = &r.meta;
    vec![
        m.id.to_string(),
        if m.name.starts_with("auto-") { "(background scan)".into() } else { m.name.clone() },
        short_time(&m.started_at),
        m.root.clone(),
        m.status.to_string(),
        // A cancelled or failed scan's partial counts are not data: leave them out.
        m.counts.as_ref().filter(|_| m.status.is_usable()).map(|c| format_count(c.entries)).unwrap_or_else(|| "-".into()),
        m.counts.as_ref().filter(|_| m.status.is_usable()).map(|c| format_size(c.allocated_bytes, u)).unwrap_or_else(|| "-".into()),
        format_size(r.size_on_disk, u),
    ]
}

fn draw_snapshots(f: &mut Frame, app: &App, area: Rect) {
    let l = match &app.snapshots {
        Job::Done(l) => l,
        Job::Failed(e) => return draw_text(f, app, area, &format!("Cannot read snapshots: {e}")),
        _ => return draw_text(f, app, area, "Loading…"),
    };
    if l.snapshots.is_empty() {
        return draw_text(f, app, area, &format!("No snapshots yet in {}.\n\nPress S to scan.", l.store_path.display()));
    }
    let shown = app.history();
    let u = units(app);
    let mut title = match &app.disk_filter {
        Some(d) => format!(" Snapshot history — {}: {} of {} scans · * every disk ", d.label(), shown.len(), l.snapshots.len()),
        None => format!(" Snapshot history — {} scans, store {} ", l.snapshots.len(), format_size(l.store_bytes, u)),
    };
    if !app.snap_marks.is_empty() {
        title.push_str(&format!("· {} marked ", app.snap_marks.len()));
    }
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [list, detail] = Layout::vertical([Constraint::Min(2), Constraint::Length(6)]).areas(inner);
    let height = list.height.saturating_sub(1);
    let (rows, start) = window(&shown, follow(app.snap_sel, height), height);
    let header = Row::new(["ID", "Name", "Started", "Root", "Status", "Entries", "On disk", "Stored"]).style(Style::new().bold());
    let rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut cells = snapshot_cells(r, u);
            let marked = app.snap_marks.contains(&r.meta.id);
            if marked {
                // The mark replaces the space in front of the id.
                cells[0] = format!("×{}", cells[0].trim_start());
            }
            let row = Row::new(cells);
            if start + i == app.snap_sel {
                row.style(Style::new().fg(Color::Black).bg(ACCENT))
            } else if marked {
                row.style(Style::new().fg(Color::Red))
            } else if !r.meta.status.is_usable() {
                // Cancelled or failed: nothing to open.
                row.style(Style::new().fg(Color::DarkGray))
            } else {
                row
            }
        })
        .collect();
    let widths = [
        Constraint::Length(4),
        Constraint::Min(12),
        Constraint::Length(16),
        Constraint::Min(8),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    f.render_widget(Table::new(rows, widths).header(header), list);
    // What the selected scan is, and what can be done with it.
    let rule = Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray));
    // The question takes the whole panel: it must not be cut off.
    if let Some(p) = &app.snap_remove {
        let ids: Vec<String> = p.snapshots.iter().map(|r| format!("#{}", r.meta.id)).collect();
        let lines = vec![
            Line::styled(format!(" Remove {}?", ids.join(" ")), Style::new().fg(Color::Yellow).bold()),
            Line::styled(format!(" {}", text::prune::proposal_summary(p, u)), Style::new().fg(Color::Yellow)),
            Line::raw(" y removes them, any other key keeps them."),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(rule), detail);
        return;
    }
    let Some(r) = shown.get(app.snap_sel) else { return };
    let m = &r.meta;
    let mut lines = vec![Line::styled(format!(" {} · {}", scan_title(m), m.root), Style::new().bold())];
    lines.extend(text::snapshots::snapshot_detail(m).into_iter().map(|t| Line::raw(format!(" {t}"))));
    if let Some(job) = &app.store_job {
        lines.push(Line::styled(format!(" {job}… (x stops; the store stays valid)"), Style::new().fg(Color::Yellow)));
    } else if let Some(note) = &app.snap_note {
        lines.push(Line::styled(format!(" {note}"), Style::new().fg(Color::Yellow)));
    } else if app.snap_marks.is_empty() {
        lines.push(Line::styled(
            " Space marks scans to remove (o older ones, n those without data, p by the retention rules); Del removes; z compacts the store.",
            Style::new().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(rule), detail);
}

fn sort_label(s: DiffSort) -> &'static str {
    match s {
        DiffSort::Growth => "growth",
        DiffSort::Size => "size",
        DiffSort::Modified => "modified",
        DiffSort::Created => "created",
        DiffSort::Path => "path",
    }
}

fn change_label(c: Change) -> &'static str {
    match c {
        Change::Unchanged => "same",
        Change::Added => "new",
        Change::Deleted => "deleted",
        Change::Modified => "changed",
        Change::NewlyVisible => "now visible",
        Change::NoLongerVisible => "not visible",
    }
}

/// A path inside the scanned root, relative to it and shortened from the start
/// so the name (the most specific part) stays visible.
fn short_path(path: &str, root: &str, width: u16) -> String {
    let rel = match path.strip_prefix(root) {
        Some("") => return ellipsize_start(path, width as usize),
        Some(rest) if root == "/" => rest,
        Some(rest) => rest.strip_prefix('/').unwrap_or(path),
        None => path,
    };
    ellipsize_start(if rel.is_empty() { path } else { rel }, width as usize)
}

/// Width left for the last column of a table after the fixed ones (and the
/// one-cell gaps between columns).
fn rest_width(area: Rect, fixed: &[u16]) -> u16 {
    area.width.saturating_sub(fixed.iter().sum::<u16>() + fixed.len() as u16).max(10)
}

fn diff_rows<'a>(items: &'a [DiffItem], u: SizeUnits, start: usize, sel: usize, root: &str, width: u16) -> Vec<Row<'a>> {
    items
        .iter()
        .enumerate()
        .map(|(n, i)| {
            let color = if i.delta_allocated > 0 { Color::Yellow } else { Color::Green };
            let row = Row::new(vec![
                Cell::from(format_delta(i.delta_allocated as i128, u)).style(Style::new().fg(color)),
                Cell::from(format_size(i.old_allocated, u)),
                Cell::from(format_size(i.new_allocated, u)),
                Cell::from(if i.is_dir { "" } else { change_label(i.change) }),
                Cell::from(short_path(&i.path.lossy(), root, width)),
            ]);
            selected(row, start + n == sel)
        })
        .collect()
}

/// "#4 2026-10-21 13:13 “two”"; background scans are marked instead of named.
fn scan_title(m: &SnapshotMeta) -> String {
    if m.name.starts_with("auto-") {
        format!("#{} {} (background scan)", m.id, short_time(&m.started_at))
    } else if snapshots::is_default_name(&m.name) {
        format!("#{} {}", m.id, short_time(&m.started_at))
    } else {
        format!("#{} {} “{}”", m.id, short_time(&m.started_at), m.name)
    }
}

fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    let usable = app.usable();
    let d = &app.diff;
    if matches!(d.focus, DiffFocus::PickOld | DiffFocus::PickNew) {
        return draw_diff_picker(f, app, area);
    }
    let block = Block::bordered().title(" Compare snapshots ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if matches!(d.result, Job::Idle) {
        let msg = if app.snapshots.is_running() || matches!(app.snapshots, Job::Idle) {
            "Loading…".to_string()
        } else {
            "Comparing needs two scans of the same folder (with the same exclusions).\n Scan it again with S, then come back here.".to_string()
        };
        f.render_widget(Paragraph::new(format!("\n {msg}")), inner);
        return;
    }
    let u = units(app);
    let [head, tabs, body] = Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let (old, new) = (usable.get(d.old).map(|r| &r.meta), usable.get(d.new).map(|r| &r.meta));
    if let (Some(o), Some(n)) = (old, new) {
        let focused = d.focus == DiffFocus::Header;
        let pair = format!(" {}  →  {} ", scan_title(o), scan_title(n));
        let hint = if focused { "  Enter choose other scans" } else { "  ↑ or p to choose other scans" };
        let style = if focused { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().bold() };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![Span::styled(pair, style), Span::styled(hint, Style::new().fg(Color::DarkGray))]),
                Line::styled(format!(" of {} (paths below are inside it)", n.root), Style::new().fg(Color::DarkGray)),
            ]),
            head,
        );
    }
    let r = match &d.result {
        Job::Running(m) => {
            f.render_widget(Paragraph::new(format!("\n {m}")), body);
            return;
        }
        Job::Failed(e) => {
            f.render_widget(Paragraph::new(format!("\n {e}")).wrap(Wrap { trim: true }), body);
            return;
        }
        Job::Done(r) => r,
        Job::Idle => return,
    };
    let rep = &r.report;
    // A list cut at the limit shows "200+": the true count is not known.
    let count = |lists: &[usize]| {
        let n: usize = lists.iter().sum();
        if lists.iter().any(|&l| l >= DIFF_LIMIT) { format!("{n}+") } else { n.to_string() }
    };
    let tab_spans: Vec<Span> = DIFF_TABS
        .iter()
        .map(|t| {
            let name = match t {
                DiffTab::Summary => " Summary ".to_string(),
                DiffTab::GrowingDirs => format!(" Growing folders ({}) ", count(&[rep.growing_dirs.len()])),
                DiffTab::Files => format!(" Changed files ({}) ", count(&[rep.files.len()])),
                DiffTab::Shrinking => format!(" Shrinking ({}) ", count(&[rep.shrinking_dirs.len(), rep.shrinking_files.len()])),
            };
            if *t == d.tab {
                Span::styled(name, Style::new().fg(Color::Black).bg(ACCENT))
            } else {
                Span::raw(name)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(tab_spans)), tabs);
    if d.tab == DiffTab::Summary {
        // The line above already names the two scans.
        let full = text::diff(r, u, false, false);
        let full = if full.starts_with("Comparing snapshot") { full.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default() } else { full };
        let lines = wrapped_rows(&full, body.width);
        let scroll = app.scroll.min(lines.saturating_sub(body.height));
        f.render_widget(Paragraph::new(full).wrap(Wrap { trim: false }).scroll((scroll, 0)), body);
        return;
    }
    let [info, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
    let min = if d.min_size == 0 { "any size".to_string() } else { format!("at least {}", format_size(d.min_size, u)) };
    f.render_widget(
        Paragraph::new(Line::styled(
            format!(" sorted by {} · {min}    o change order · f change minimum size", sort_label(d.sort)),
            Style::new().fg(Color::DarkGray),
        )),
        info,
    );
    let items = app.diff_items();
    if items.is_empty() {
        f.render_widget(Paragraph::new("\n Nothing in this list."), list);
        return;
    }
    let height = list.height.saturating_sub(1);
    let sel = if d.focus == DiffFocus::Results { d.sel } else { usize::MAX };
    let (slice, start) = window(&items, follow(d.sel, height), height);
    let header = Row::new(["Change", "Before", "After", "", "Path"]).style(Style::new().bold());
    let fixed = [11, 10, 10, 11];
    let widths = [Constraint::Length(11), Constraint::Length(10), Constraint::Length(10), Constraint::Length(11), Constraint::Min(10)];
    let root = usable.get(d.new).map(|r| r.meta.root.as_str()).unwrap_or("");
    f.render_widget(Table::new(diff_rows(slice, u, start, sel, root, rest_width(list, &fixed)), widths).header(header), list);
}

/// Choosing the two scans: a list like the history, in two steps.
fn draw_diff_picker(f: &mut Frame, app: &App, area: Rect) {
    let usable = app.usable();
    let d = &app.diff;
    let second = d.focus == DiffFocus::PickNew;
    let title = if second { " Compare — step 2 of 2: compare it with which later scan? " } else { " Compare — step 1 of 2: compare from which scan? " };
    let info = match usable.get(d.old) {
        Some(o) if second => format!(" From {}. Greyed scans are older or cover another folder.", scan_title(&o.meta)),
        _ => " Pick the earlier scan. Greyed scans have no later scan of the same folder to compare with.".to_string(),
    };
    let from = if second { Some(d.old) } else { None };
    draw_scan_picker(f, app, area, title, &info, d.cursor, from, &|i| app.diff_choosable(i));
}

/// A list of the scans that hold data, to pick one from: `cursor` is marked ▶,
/// `from` (an earlier choice) "from", and rows `choosable` rejects are greyed.
#[allow(clippy::too_many_arguments)]
fn draw_scan_picker(f: &mut Frame, app: &App, area: Rect, title: &str, info: &str, cursor: usize, from: Option<usize>, choosable: &dyn Fn(usize) -> bool) {
    let usable = app.usable();
    let u = units(app);
    let block = Block::bordered().title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [top, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(info.to_string()).style(Style::new().fg(Color::DarkGray)), top);
    let height = list.height.saturating_sub(1);
    let (slice, start) = window(&usable, follow(cursor, height), height);
    let rows: Vec<Row> = slice
        .iter()
        .enumerate()
        .map(|(n, r)| {
            let i = start + n;
            let mark = if i == cursor { " ▶" } else if Some(i) == from { "from" } else { "" };
            let mut cells = vec![mark.to_string()];
            cells.extend(snapshot_cells(r, u).into_iter().take(7));
            let row = Row::new(cells);
            if i == cursor {
                row.style(Style::new().fg(Color::Black).bg(ACCENT))
            } else if !choosable(i) {
                row.style(Style::new().fg(Color::DarkGray))
            } else {
                row
            }
        })
        .collect();
    let header = Row::new(["", "ID", "Name", "Started", "Root", "Status", "Entries", "On disk"]).style(Style::new().bold());
    let widths = [
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Min(12),
        Constraint::Length(16),
        Constraint::Min(8),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Length(10),
    ];
    f.render_widget(Table::new(rows, widths).header(header), list);
}

fn top_rows(items: &[TopItem], u: SizeUnits, metric: SizeMetric, start: usize, sel: usize, root: &str, width: u16) -> Vec<Row<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(n, i)| {
            let (main, other) = match metric {
                SizeMetric::Allocated => (i.allocated_bytes, i.logical_bytes),
                SizeMetric::Logical => (i.logical_bytes, i.allocated_bytes),
            };
            let dir = i.kind == diskdrift_app::core::record::Kind::Dir;
            let suffix = match (dir, i.incomplete) {
                (true, true) => "/  (incomplete)",
                (true, false) => "/",
                _ => "",
            };
            let path = short_path(&i.path.lossy(), root, width.saturating_sub(suffix.chars().count() as u16));
            let row = Row::new(vec![
                Cell::from(format_size(main, u)).style(Style::new().bold()),
                Cell::from(format_size(other, u)),
                Cell::from(if dir { format_count(i.entries) } else { String::new() }),
                Cell::from(format!("{path}{suffix}")),
            ]);
            selected(row, start + n == sel)
        })
        .collect()
}

fn draw_top(f: &mut Frame, app: &App, area: Rect) {
    let u = units(app);
    let t = &app.top;
    if t.focus == TopFocus::Pick {
        let info = " Pick the scan to list the largest files and folders of.";
        return draw_scan_picker(f, app, area, " Largest files — which scan? ", info, t.cursor, None, &|_| true);
    }
    let block = Block::bordered().title(" Largest files and folders ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let usable = app.usable();
    let Some(m) = usable.get(t.snapshot).map(|r| &r.meta) else {
        let msg = if app.snapshots.is_running() || matches!(app.snapshots, Job::Idle) { "Loading…" } else { "No scan yet. Press S to scan." };
        f.render_widget(Paragraph::new(format!("\n {msg}")), inner);
        return;
    };
    let [head, tabs, hint, body] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let focused = t.focus == TopFocus::Header;
    let latest = if t.snapshot + 1 == usable.len() { " · latest" } else { "" };
    let style = if focused { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().bold() };
    let tip = if focused { "  Enter choose another scan" } else { "  ↑ or p to choose another scan" };
    let mut second = format!(" of {} (paths below are inside it)", m.root);
    if let Some(r) = t.result.done() {
        second.push_str(&format!(" · {} entries · {} on disk", format_count(r.report.entries), format_size(r.report.total_allocated, u)));
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled(format!(" {}{latest} ", scan_title(m)), style), Span::styled(tip, Style::new().fg(Color::DarkGray))]),
            Line::styled(second, Style::new().fg(Color::DarkGray)),
        ]),
        head,
    );
    let view = |v: TopView, name: &str| {
        if t.view == v { Span::styled(format!(" {name} "), Style::new().fg(Color::Black).bg(ACCENT)) } else { Span::raw(format!(" {name} ")) }
    };
    f.render_widget(Paragraph::new(Line::from(vec![view(TopView::Files, "Largest files"), Span::raw(" "), view(TopView::Dirs, "Largest folders (deepest)")])), tabs);
    let explain = match (t.view, t.metric) {
        (TopView::Dirs, _) => "where the space sits (a folder is left out when its big subfolder is listed)",
        (TopView::Files, _) => "single files, biggest first",
    };
    let metric = match t.metric {
        SizeMetric::Allocated => "by space on disk · m: by file size",
        SizeMetric::Logical => "by file size · m: by space on disk",
    };
    f.render_widget(Paragraph::new(Line::styled(format!(" {explain} · {metric}"), Style::new().fg(Color::DarkGray))), hint);
    let r = match &t.result {
        Job::Idle => return,
        Job::Running(m) => {
            f.render_widget(Paragraph::new(format!("\n {m}")), body);
            return;
        }
        Job::Failed(e) => {
            f.render_widget(Paragraph::new(format!("\n {e}")).wrap(Wrap { trim: true }), body);
            return;
        }
        Job::Done(r) => r,
    };
    let items = if t.view == TopView::Dirs { &r.report.dirs } else { &r.report.files };
    let height = body.height.saturating_sub(1);
    let sel = if t.focus == TopFocus::List { t.sel } else { usize::MAX };
    let (slice, start) = window(items, follow(t.sel, height), height);
    let (h1, h2) = match t.metric {
        SizeMetric::Allocated => ("On disk", "File size"),
        SizeMetric::Logical => ("File size", "On disk"),
    };
    let header = Row::new([h1, h2, "Entries", "Path"]).style(Style::new().bold());
    let fixed = [10, 10, 9];
    let widths = [Constraint::Length(10), Constraint::Length(10), Constraint::Length(9), Constraint::Min(10)];
    f.render_widget(Table::new(top_rows(slice, u, t.metric, start, sel, &m.root, rest_width(body, &fixed)), widths).header(header), body);
}

/// A timestamped log, newest at the bottom, with an optional live status line.
fn draw_log(f: &mut Frame, area: Rect, title: &str, log: &[LogLine], status: Option<String>) {
    let block = Block::bordered().title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let room = inner.height.saturating_sub(if status.is_some() { 2 } else { 0 }) as usize;
    let mut lines: Vec<Line> = log
        .iter()
        .skip(log.len().saturating_sub(room))
        .map(|l| Line::from(vec![Span::styled(format!(" {} ", l.at), Style::new().fg(Color::DarkGray)), Span::raw(l.text.clone())]))
        .collect();
    if let Some(s) = status {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!(" {s}"), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The measurement Doctor and Recover share, drawn the same on both screens.
fn draw_measuring(f: &mut Frame, app: &App, area: Rect) {
    let msg = match &app.doctor {
        Job::Running(m) => m.as_str(),
        _ => "",
    };
    // The keys are in the footer; this line only says what is happening.
    let status = format!("{msg} {:.0}s", app.doctor_started.elapsed().as_secs_f64());
    let title = format!(" {} — measuring (one run for Doctor and Recovery suggestions) ", app.screen.title());
    draw_log(f, area, &title, &app.doctor_log, Some(status));
}

fn risk_color(r: Option<Risk>) -> Color {
    match r {
        Some(Risk::Low) => Color::Green,
        Some(Risk::Medium) => Color::Yellow,
        Some(Risk::High) => Color::Red,
        Some(Risk::SystemManaged) => Color::Blue,
        None => Color::DarkGray,
    }
}

fn risk_from_label(s: &str) -> Option<Risk> {
    match s {
        "low" => Some(Risk::Low),
        "medium" => Some(Risk::Medium),
        "high" => Some(Risk::High),
        "system" => Some(Risk::SystemManaged),
        _ => None,
    }
}

/// First row index that keeps `sel` visible in `height` rows.
fn follow(sel: usize, height: u16) -> u16 {
    (sel as u16).saturating_sub(height.saturating_sub(1))
}

fn selected(row: Row<'_>, is_sel: bool) -> Row<'_> {
    if is_sel { row.style(Style::new().fg(Color::Black).bg(ACCENT)) } else { row }
}

fn draw_recover(f: &mut Frame, app: &App, area: Rect) {
    let ui = &app.recover_ui;
    let r = match &app.recover {
        Job::Running(_) if app.doctor.is_running() => return draw_measuring(f, app, area),
        Job::Running(msg) => return draw_text(f, app, area, msg),
        Job::Idle => return draw_text(f, app, area, "Starting…"),
        Job::Failed(e) => return draw_text(f, app, area, &format!("Failed: {e}\n\nF5 tries again.")),
        Job::Done(r) => r,
    };
    let u = units(app);
    let block = Block::bordered().title(" Recovery suggestions — best first (size weighted by risk) · read-only, nothing is deleted ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if r.candidates.is_empty() {
        f.render_widget(Paragraph::new("\n No large candidates found."), inner);
        return;
    }
    let detail_h = 7.min(inner.height / 2);
    let [list, detail] = Layout::vertical([Constraint::Min(2), Constraint::Length(detail_h)]).areas(inner);
    let height = list.height.saturating_sub(1);
    let (rows, start) = window(&r.candidates, follow(ui.sel, height), height);
    let fixed = [3, 11, 7, 34];
    let path_w = rest_width(list, &fixed);
    let home = app.settings.home.display().to_string();
    let rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let size = match c.observed_bytes {
                _ if c.skipped => "skipped".into(),
                Some(b) if c.confidence == "lower bound" => format!("≥ {}", format_size(b, u)),
                Some(b) => format_size(b, u),
                None => "?".into(),
            };
            let path = c.path.as_deref().map(|p| home_path(p, &home, path_w)).unwrap_or_default();
            let cells = vec![
                Cell::from(format!("{:>2}", c.rank)),
                Cell::from(size).style(Style::new().bold()),
                Cell::from(c.risk.clone()).style(Style::new().fg(risk_color(risk_from_label(&c.risk)))),
                Cell::from(clip(&c.label, 34)),
                Cell::from(path),
            ];
            if c.skipped {
                let row = Row::new(cells).style(Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM));
                return if start + i == ui.sel { row.style(Style::new().fg(Color::Black).bg(Color::DarkGray)) } else { row };
            }
            selected(Row::new(cells), start + i == ui.sel)
        })
        .collect();
    let header = Row::new(["#", "Size", "Risk", "Location", "Path"]).style(Style::new().bold());
    let widths = [Constraint::Length(3), Constraint::Length(11), Constraint::Length(7), Constraint::Length(34), Constraint::Min(10)];
    f.render_widget(Table::new(rows, widths).header(header), list);
    if let Some(c) = r.candidates.get(ui.sel) {
        let analysable = c.path.as_ref().is_some_and(|p| std::path::Path::new(p).is_dir());
        let mut lines = vec![Line::styled(format!(" {} — reclaimable: {}", c.label, c.reclaimable), Style::new().bold())];
        if c.skipped {
            lines.push(Line::styled(" Skipped while measuring: size unknown. F5 re-runs all locations; Enter analyses just this one.", Style::new().fg(Color::DarkGray)));
        }
        let risk = match risk_from_label(&c.risk) {
            Some(Risk::Low) => "risk low: losing it costs little (usually rebuilt or re-downloaded)",
            Some(Risk::Medium) => "risk medium: may hold settings, offline data or work in progress",
            Some(Risk::High) => "risk high: likely your own data; removing it can lose work",
            Some(Risk::SystemManaged) => "managed by macOS: leave it to the system",
            None => "risk unknown: depends on what is inside",
        };
        let mut facts = vec![risk.to_string()];
        if let Some(w) = &c.within {
            facts.push(format!("inside {w}, so its size is counted there too"));
        }
        lines.push(Line::styled(format!(" {}", capitalize(&facts.join(" · "))), Style::new().fg(Color::DarkGray)));
        // For growth the action below does what the CLI hint says; show where instead.
        match (&c.growth, &c.path) {
            (Some(_), Some(p)) => lines.push(Line::raw(format!(" In {}", home_path(p, &home, u16::MAX)))),
            _ => lines.push(Line::raw(format!(" → {}", c.how))),
        }
        let action = match c.growth {
            Some((old, new)) => format!(" Enter: browse what changed here (scan #{old} → #{new}) · a: analyse the folder as it is now"),
            None if analysable => " Enter: analyse this folder (what each subfolder is and whether it matters)".into(),
            None => " (not a folder: nothing to open)".into(),
        };
        lines.push(Line::styled(action, Style::new().fg(ACCENT)));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)), detail);
    }
}

/// `s` cut to `max` characters at the end (for names, whose start matters).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
}

/// A path with the home folder shown as `~`, shortened from the start.
fn home_path(path: &str, home: &str, width: u16) -> String {
    let p = match path.strip_prefix(home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    };
    ellipsize_start(&p, width as usize)
}

fn draw_analysis(f: &mut Frame, app: &App, area: Rect, view: &AnalysisView) {
    let u = units(app);
    let depth = app.analysis.stack.len();
    let block = Block::bordered().title(format!(" Analyse ({depth}) {} ", tail(&view.path.display().to_string(), area.width.saturating_sub(16) as usize)));
    let r = match &view.result {
        Job::Done(r) => r,
        Job::Running(_) | Job::Idle => {
            let p = &view.progress;
            let cur = p.current.lock().map(|c| c.clone()).unwrap_or_default();
            let status = format!(
                "{} entries · {} · {:.0}s · now in {}",
                p.entries.load(Ordering::Relaxed),
                format_size(p.bytes_alloc.load(Ordering::Relaxed), u),
                view.started.elapsed().as_secs_f64(),
                tail(&cur, area.width.saturating_sub(50) as usize),
            );
            let title = format!(" Analyse ({depth}) {} ", view.path.display());
            return draw_log(f, area, &title, &view.log, Some(status));
        }
        Job::Failed(e) => {
            f.render_widget(Paragraph::new(format!("\n {e}\n\n Esc goes back.")).wrap(Wrap { trim: true }).block(block), area);
            return;
        }
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let detail_h = 6.min(inner.height / 2);
    let [head, list, detail] = Layout::vertical([Constraint::Length(3), Constraint::Min(2), Constraint::Length(detail_h)]).areas(inner);
    let status = match r.status {
        MeasureStatus::Complete => "complete".to_string(),
        MeasureStatus::Partial => "partial: some folders unreadable".to_string(),
        MeasureStatus::TimedOut => "time ran out: lower bounds".to_string(),
        _ => "incomplete".to_string(),
    };
    let mut verdicts: Vec<Span> = vec![Span::raw(" ")];
    for v in &r.by_verdict {
        let risk = match v.verdict.as_str() {
            "regenerable" => Some(Risk::Low),
            "review" => Some(Risk::Medium),
            "keep/user data" => Some(Risk::High),
            "system" => Some(Risk::SystemManaged),
            _ => None,
        };
        verdicts.push(Span::styled(format!("{} {}", shown_verdict(&v.verdict), format_size(v.allocated_bytes, u)), Style::new().fg(risk_color(risk))));
        verdicts.push(Span::raw("  "));
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::raw(format!(
                    " {} on disk · {} entries · {}{}",
                    format_size(r.allocated_bytes, u),
                    format_count(r.entries),
                    status,
                    if r.sizes_from.is_some() { String::new() } else { format!(" · {:.1}s", r.elapsed_ms as f64 / 1000.0) }
                )),
                Span::styled(
                    r.sizes_from.as_ref().map(|s| format!(" · from {s} · F5 measures now")).unwrap_or_default(),
                    Style::new().fg(Color::Yellow),
                ),
            ]),
            Line::from(verdicts),
            match &r.context {
                Some(c) => Line::from(vec![
                    Span::raw(" Inside: "),
                    Span::styled(format!("{} [{}]", c.what, verdict(Some(c.risk))), Style::new().fg(risk_color(Some(c.risk)))),
                ]),
                None => Line::raw(""),
            },
        ]),
        head,
    );
    if r.items.is_empty() {
        f.render_widget(Paragraph::new("\n Empty folder."), list);
        return;
    }
    let height = list.height.saturating_sub(1);
    let (rows, start) = window(&r.items, follow(view.sel, height), height);
    let owners = has_owners(r);
    let rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let risk = it.assessment.risk;
            let mut cells = vec![
                Cell::from(item_size(it, u)).style(Style::new().bold()),
                Cell::from(item_changed(it, r.analyzed_at)),
                Cell::from(text::verdict_label(risk)).style(Style::new().fg(risk_color(risk))),
                Cell::from(it.assessment.what.clone()),
            ];
            if owners {
                cells.push(Cell::from(item_owner(it)));
            }
            cells.push(Cell::from(format!("{}{}", it.name, if it.is_dir { "/" } else { "" })));
            selected(Row::new(cells), start + i == view.sel)
        })
        .collect();
    let mut head_cells = vec!["On disk", "Last change", "Verdict", "What"];
    let mut widths = vec![Constraint::Length(13), Constraint::Length(15), Constraint::Length(15), Constraint::Length(28)];
    if owners {
        head_cells.push("Owner");
        widths.push(Constraint::Length(18));
    }
    head_cells.push("Name");
    widths.push(Constraint::Min(10));
    let header = Row::new(head_cells).style(Style::new().bold());
    f.render_widget(Table::new(rows, widths).header(header), list);
    if let Some(it) = r.items.get(view.sel) {
        let mut lines: Vec<Line> = item_detail(it, r.analyzed_at).into_iter().map(|l| Line::raw(format!(" {l}"))).collect();
        if let Some(first) = lines.first_mut() {
            *first = first.clone().bold();
        }
        if let Some(n) = &view.note {
            lines.push(Line::styled(format!(" {n}"), Style::new().fg(Color::Yellow)));
        }
        if r.more_items > 0 && view.sel + 1 == r.items.len() {
            lines.push(Line::raw(format!(" … {} more item(s) not listed, {}", r.more_items, format_size(r.more_allocated_bytes, u))));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)),
            detail,
        );
    }
}

/// A proportional bar, `width` cells wide.
fn bar(part: u64, whole: u64, width: usize) -> String {
    let filled = if whole == 0 { 0 } else { ((part as f64 / whole as f64) * width as f64).round() as usize };
    format!("{}{}", "█".repeat(filled.min(width)), " ".repeat(width - filled.min(width)))
}

/// Column headers and widths of the browser table; the same while a level
/// loads, so entering a folder only swaps the rows.
fn browse_columns(diff: bool) -> (Row<'static>, Vec<Constraint>) {
    let (names, widths): (Vec<&str>, Vec<Constraint>) = if diff {
        (
            vec!["Size change", "Before", "After", "Entries", "What", "Name"],
            vec![Constraint::Length(11), Constraint::Length(10), Constraint::Length(10), Constraint::Length(8), Constraint::Length(14), Constraint::Min(10)],
        )
    } else {
        (
            vec!["On disk", "Share", "", "Entries", "Last change", "Name"],
            vec![Constraint::Length(11), Constraint::Length(5), Constraint::Length(10), Constraint::Length(9), Constraint::Length(15), Constraint::Min(10)],
        )
    };
    (Row::new(names).style(Style::new().bold()), widths)
}

/// Summary line, visible rows and detail lines of one loaded level.
struct BrowseBody {
    summary: String,
    rows: Vec<Row<'static>>,
    detail: Vec<String>,
}

/// Rows of a level; `pending` greys them out (a folder of it is being opened)
/// and keeps the opened one marked.
fn browse_row(cells: Vec<Cell<'static>>, styles: Vec<Style>, is_sel: bool, pending: bool) -> Row<'static> {
    if pending {
        let row = Row::new(cells);
        return if is_sel { row.style(Style::new().fg(Color::Black).bg(Color::DarkGray)) } else { row.style(Style::new().fg(Color::DarkGray)) };
    }
    selected(Row::new(cells.into_iter().zip(styles).map(|(c, st)| c.style(st)).collect::<Vec<_>>()), is_sel)
}

fn browse_body(data: &BrowseData, sel: usize, height: u16, u: SizeUnits, pending: bool) -> BrowseBody {
    let plain = Style::new();
    match data {
        BrowseData::Snapshot(b) => {
            let l = &b.listing;
            let now = chrono::Local::now().timestamp();
            let total = l.dir.as_ref().map(|d| d.allocated_bytes).unwrap_or(0);
            let summary = match &l.dir {
                Some(d) => format!(
                    " {} on disk · {} · {} items{}",
                    child_size(d, u),
                    text::entries(d.entries),
                    l.items.len() as u64 + l.more_items,
                    if l.more_items > 0 { format!(" ({} more not listed, {})", l.more_items, format_size(l.more_allocated_bytes, u)) } else { String::new() }
                ),
                None => String::new(),
            };
            let (rows, start) = window(&l.items, follow(sel, height), height);
            let rows = rows
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let cells = vec![
                        Cell::from(child_size(c, u)),
                        Cell::from(share(c.allocated_bytes, total)),
                        Cell::from(bar(c.allocated_bytes, total, 10)),
                        Cell::from(format_count(c.entries)),
                        Cell::from(child_changed(c, now)),
                        Cell::from(child_name(c.name(), c.is_dir())),
                    ];
                    let styles = vec![Style::new().bold(), plain, Style::new().fg(ACCENT), plain, plain, plain];
                    browse_row(cells, styles, start + i == sel, pending)
                })
                .collect();
            let detail = l.items.get(sel).map(|c| child_detail(c, u, now)).unwrap_or_else(|| vec!["Empty folder.".into()]);
            BrowseBody { summary, rows, detail }
        }
        BrowseData::Diff(d) => {
            let l = &d.listing;
            let summary = match &l.dir {
                Some(x) => {
                    let changed = l.items.len() as u64 + l.more_items;
                    format!(
                        " {} → {} ({}) · {} changed, in {changed} of {} items here{}",
                        format_size(x.old_allocated, u),
                        format_size(x.new_allocated, u),
                        format_delta(x.delta_allocated as i128, u),
                        text::entries(x.changed_entries),
                        changed + l.unchanged_items,
                        if l.unchanged_items > 0 { " (unchanged ones not listed)" } else { "" }
                    )
                }
                None => String::new(),
            };
            let (rows, start) = window(&l.items, follow(sel, height), height);
            let rows = rows
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let color = if c.delta_allocated > 0 { Color::Yellow } else if c.delta_allocated < 0 { Color::Green } else { Color::DarkGray };
                    let cells = vec![
                        Cell::from(format_delta(c.delta_allocated as i128, u)),
                        Cell::from(format_size(c.old_allocated, u)),
                        Cell::from(format_size(c.new_allocated, u)),
                        Cell::from(format_count(c.changed_entries)),
                        Cell::from(diff_change(c)),
                        Cell::from(child_name(c.name(), c.is_dir())),
                    ];
                    let styles = vec![Style::new().fg(color).bold(), plain, plain, plain, plain, plain];
                    browse_row(cells, styles, start + i == sel, pending)
                })
                .collect();
            let detail = l.items.get(sel).map(|c| diff_detail(c, u)).unwrap_or_else(|| vec!["Nothing changed here.".into()]);
            BrowseBody { summary, rows, detail }
        }
    }
}

fn browse_title(data: Option<&BrowseData>, source: BrowseSource) -> String {
    match (data, source) {
        (Some(BrowseData::Snapshot(b)), _) => {
            let m = &b.snapshot;
            format!(" Browse {} ", scan_title(m))
        }
        (Some(BrowseData::Diff(d)), _) => {
            format!(" Changes #{} {} → #{} {} ", d.old.id, short_time(&d.old.started_at), d.new.id, short_time(&d.new.started_at))
        }
        (None, BrowseSource::Snapshot(id)) => format!(" Browse #{id} "),
        (None, BrowseSource::Diff { old, new }) => format!(" Changes #{old} → #{new} "),
    }
}

fn draw_browse(f: &mut Frame, app: &App, area: Rect) {
    let u = units(app);
    let Some((b, level)) = app.browse.as_ref().and_then(|b| b.stack.last().map(|l| (b, l))) else {
        return draw_text(f, app, area, "Nothing to browse.");
    };
    // While a level loads, the one it was opened from stays on screen, dimmed,
    // with the folder being opened still highlighted: only the rows change.
    let (shown, sel, status) = match &level.result {
        Job::Done(r) => (Some(r.as_ref()), level.sel, None),
        Job::Failed(e) => (None, 0, Some(Span::styled(format!(" {e} · Esc goes back"), Style::new().fg(Color::Red)))),
        Job::Running(_) | Job::Idle => {
            let backdrop = b.backdrop();
            let secs = level.started.elapsed().as_secs_f64();
            let text = if secs < 0.5 { " reading…".to_string() } else { format!(" reading… {secs:.0}s") };
            (backdrop.and_then(|l| l.result.done()), backdrop.map(|l| l.sel).unwrap_or(0), Some(Span::styled(text, Style::new().fg(Color::Yellow))))
        }
    };
    let loading = status.is_some();
    let block = Block::bordered().title(browse_title(shown, b.source));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let detail_h = 4.min(inner.height / 3);
    let [head, list, detail] = Layout::vertical([Constraint::Length(2), Constraint::Min(2), Constraint::Length(detail_h)]).areas(inner);
    let height = list.height.saturating_sub(1);
    let body = shown.map(|d| browse_body(d, sel, height, u, loading));
    let path = level.path.as_ref().or(b.root()).map(|p| p.display().to_string()).unwrap_or_default();
    let path_line = Line::from(vec![
        Span::styled(format!(" ({}) ", b.stack.len()), Style::new().fg(Color::DarkGray)),
        Span::styled(tail(&path, inner.width.saturating_sub(8) as usize), Style::new().bold()),
    ]);
    let summary = match (&status, &body) {
        (Some(s), _) => Line::from(s.clone()),
        (None, _) if b.note.is_some() => Line::styled(format!(" {}", b.note.as_deref().unwrap_or_default()), Style::new().fg(Color::Yellow)),
        (None, Some(body)) => Line::raw(body.summary.clone()),
        (None, None) => Line::raw(""),
    };
    f.render_widget(Paragraph::new(vec![path_line, summary]), head);
    let (header, widths) = browse_columns(matches!(b.source, BrowseSource::Diff { .. }));
    let rows = body.as_ref().map(|b| b.rows.clone()).unwrap_or_default();
    f.render_widget(Table::new(rows, widths).header(header), list);
    let lines: Vec<Line> = match &body {
        Some(body) if !loading => {
            body.detail.iter().enumerate().map(|(i, t)| if i == 0 { Line::raw(format!(" {t}")).bold() } else { Line::raw(format!(" {t}")) }).collect()
        }
        _ => Vec::new(),
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)), detail);
}

fn draw_disks(f: &mut Frame, app: &App, area: Rect) {
    let ui = &app.disks;
    let l = match &ui.list {
        Job::Done(l) => l,
        Job::Failed(e) => return draw_text(f, app, area, &format!("Cannot read the disks: {e}")),
        _ => return draw_text(f, app, area, "Reading disks…"),
    };
    if l.disks.is_empty() {
        return draw_text(f, app, area, &format!("No disks yet in {}.\n\nA scan records the disk it was taken on. Press S to scan.", l.store_path.display()));
    }
    let u = units(app);
    let now = chrono::Local::now().timestamp();
    let connected = l.disks.iter().filter(|r| r.connected).count();
    let block = Block::bordered().title(format!(" My disks — {} known, {connected} connected ", l.disks.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [list, detail] = Layout::vertical([Constraint::Min(2), Constraint::Length(7)]).areas(inner);
    let height = list.height.saturating_sub(1);
    let (rows, start) = window(&l.disks, follow(ui.sel, height), height);
    let ago = |rfc: &str| chrono::DateTime::parse_from_rfc3339(rfc).map(|t| diskdrift_app::analyze::ago(now - t.timestamp())).unwrap_or_else(|_| rfc.into());
    // Narrow: the volume name moves to the detail lines.
    let wide = area.width >= 100;
    let rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let d = &r.disk;
            // `?`: recognised weakly (explained below the list).
            let name = if r.known && d.identity.strength == Strength::Weak { format!("{} ?", d.display_name()) } else { d.display_name().to_string() };
            let mut cells = vec![name];
            if wide {
                cells.push(d.volume.name.clone().unwrap_or_else(|| "-".into()));
            }
            cells.extend([
                d.volume.fs_type.clone(),
                d.volume.capacity_bytes.map(|b| format_size(b, u)).unwrap_or_else(|| "-".into()),
                if r.connected { "yes".into() } else { "no".into() },
                if r.known { r.last_scan.as_deref().map(ago).unwrap_or_else(|| "never".into()) } else { "new".into() },
                format_count(r.snapshots as u64),
                d.sets.join(", "),
            ]);
            let row = Row::new(cells);
            if start + i == ui.sel {
                row.style(Style::new().fg(Color::Black).bg(ACCENT))
            } else if !r.connected {
                // Not connected: its scans can still be browsed and compared.
                row.style(Style::new().fg(Color::DarkGray))
            } else {
                row
            }
        })
        .collect();
    let mut header = vec!["Disk"];
    let mut widths = vec![Constraint::Min(14)];
    if wide {
        header.push("Volume");
        widths.push(Constraint::Min(12));
    }
    header.extend(["FS", "Capacity", "Connected", "Last scan", "Scans", "Sets"]);
    widths.extend([
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(14),
        Constraint::Length(5),
        Constraint::Min(12),
    ]);
    f.render_widget(Table::new(rows, widths).header(Row::new(header).style(Style::new().bold())), list);

    let Some(r) = l.disks.get(ui.sel) else { return };
    let d = &r.disk;
    let volume = format!("volume {} ({})", d.volume.name.as_deref().unwrap_or("without a name"), d.volume.fs_type);
    let mut lines = vec![Line::styled(format!(" {} · {volume} · {}", d.display_name(), d.id), Style::new().bold())];
    // A question or a field being typed takes the panel: on a short terminal it must show.
    let asking = ui.edit.is_some() || ui.forget.is_some();
    let place = match &r.mount_point {
        Some(p) => format!("connected at {}", p.display()),
        None => format!("not connected (last at {}); its scans can still be browsed and compared", d.last_mount_point.as_deref().unwrap_or("?")),
    };
    if !asking {
        lines.push(Line::raw(format!(" {place} · {} in the store", format_size(r.store_bytes, u))));
    }
    let device: Vec<String> = d
        .device
        .iter()
        .flat_map(|dev| [dev.model.clone(), dev.bus.clone(), dev.serial.as_ref().map(|s| format!("serial {s}"))])
        .flatten()
        .collect();
    let identity = match d.identity.strength {
        _ if !r.known => "new: not in the store yet. l records it with a label, g makes its first scan (DiskDrift never scans a new disk on its own)",
        Strength::Uuid => "known by its volume UUID",
        Strength::Serial => "known by the UUID macOS derives from its FAT/exFAT serial",
        Strength::Weak => "? recognised weakly (no reliable UUID): a match is asked about; `diskdrift disks merge` joins two entries of one disk",
    };
    let device = if device.is_empty() { String::new() } else { format!(" · {}", device.join(", ")) };
    if !asking {
        lines.push(Line::raw(format!(" {identity}{device}")));
        if let Some(desc) = &d.description {
            lines.push(Line::raw(format!(" {desc}")));
        }
    }
    if let Some((field, text)) = &ui.edit {
        lines.push(Line::from(vec![
            Span::raw(format!(" {}: ", field.prompt())),
            Span::styled(format!("{text}▏"), Style::new().fg(Color::Black).bg(Color::Yellow)),
        ]));
    } else if let Some(plan) = &ui.forget {
        let what = text::forget(plan, u);
        lines.push(Line::styled(format!(" {} Forget it? y forgets, any other key keeps it.", what.trim_end()), Style::new().fg(Color::Yellow).bold()));
    }
    match &ui.note {
        Some(Ok(m)) => lines.push(Line::styled(format!(" {m}"), Style::new().fg(Color::Green))),
        Some(Err(m)) => lines.push(Line::styled(format!(" {m}"), Style::new().fg(Color::Red))),
        None => {}
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray))),
        detail,
    );
}

/// The path of `path` below a scan root (the root itself: `.`).
fn below_root(path: &[u8], root: &str) -> String {
    let rest = match path.strip_prefix(root.as_bytes()) {
        Some(r) => r.strip_prefix(b"/").unwrap_or(r),
        None => path,
    };
    if rest.is_empty() { ".".into() } else { String::from_utf8_lossy(rest).into_owned() }
}

/// What the search found, in one line above the results.
fn find_status(app: &App) -> Line<'static> {
    let ui = &app.find;
    let warn = Style::new().fg(Color::Yellow);
    let mut spans: Vec<Span> = Vec::new();
    match &ui.current {
        Job::Idle => {
            return Line::styled(" part of a name, *.pdf, re:regex or Docs/plan · case and diacritics ignored", Style::new().fg(Color::DarkGray));
        }
        Job::Running(m) => return Line::raw(format!(" {m}")),
        Job::Failed(e) => return Line::styled(format!(" Cannot search: {e}"), Style::new().fg(Color::Red)),
        Job::Done(c) if !c.store_exists => return Line::raw(" No scans yet: nothing to search. Press S to scan."),
        Job::Done(c) => {
            let entries: u64 = c.searched.iter().map(|s| s.entries).sum();
            let scans = format!("{} latest {}, {}", c.searched.len(), if c.searched.len() == 1 { "scan" } else { "scans" }, text::entries(entries));
            if c.total == 0 {
                spans.push(Span::styled(" Not on any disk as of the latest scans. ".to_string(), Style::new().bold()));
            } else {
                let shown = if c.truncated { format!(", the first {} listed", c.hits.len()) } else { String::new() };
                spans.push(Span::styled(format!(" {} {}", format_count(c.total), if c.total == 1 { "match" } else { "matches" }), Style::new().bold()));
                spans.push(Span::raw(format!(" in {scans}{shown}. ")));
            }
        }
    }
    let found_now = ui.current.done().is_some_and(|c| c.total > 0);
    match &ui.history {
        Job::Idle => {}
        Job::Running(_) => {
            let n = ui.progress.map(|(d, t)| format!(" (pair {} of {t})", (d + 1).min(t.max(1)))).unwrap_or_default();
            spans.push(Span::styled(format!("Searching the history for deleted files…{n} x stops"), warn));
        }
        Job::Failed(e) => spans.push(Span::styled(format!("The history search failed: {e}"), Style::new().fg(Color::Red))),
        Job::Done(h) if h.too_many => spans.push(Span::styled(
            format!("In the history: {} paths gone so far; too many changes match, type more of the name.", format_count(h.gone_total)),
            warn,
        )),
        Job::Done(h) if h.cancelled => spans.push(Span::styled(format!("History search stopped after {} of {} pairs of scans.", h.pairs_done, h.pairs), warn)),
        Job::Done(h) if h.pairs == 0 => spans.push(Span::raw("There is no older scan to search.")),
        Job::Done(h) if h.gone.is_empty() && found_now => spans.push(Span::raw(format!("Nothing that matches is gone ({} pairs of scans read).", h.pairs))),
        Job::Done(h) if h.gone.is_empty() => spans.push(Span::raw(format!("Not in the history either ({} pairs of scans read).", h.pairs))),
        Job::Done(h) => spans.push(Span::styled(
            format!("In the history: {} no longer there.", if h.gone_total == 1 { "1 path".into() } else { format!("{} paths", format_count(h.gone_total)) }),
            warn,
        )),
    }
    Line::from(spans)
}

fn draw_find(f: &mut Frame, app: &App, area: Rect) {
    use diskdrift_app::core::record::Kind;
    use diskdrift_app::search::Place;
    let ui = &app.find;
    let u = units(app);
    let now = chrono::Local::now().timestamp();
    let title = match &app.disk_filter {
        Some(d) => format!(" Find a file — only {} (* in History shows every disk) ", d.label()),
        None => " Find a file — every disk, connected or not ".to_string(),
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // The status wraps to a second line on a narrow terminal.
    let [field, status, list, detail] = Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Min(2), Constraint::Length(6)]).areas(inner);
    let input = if ui.editing {
        Span::styled(format!("{}▏", ui.input), Style::new().fg(Color::Black).bg(Color::Yellow))
    } else {
        Span::styled(ui.input.clone(), Style::new().bold())
    };
    f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" Find: "), input])), field);
    f.render_widget(Paragraph::new(find_status(app)).wrap(Wrap { trim: false }), status);

    let hits = ui.current.done().map(|c| c.hits.as_slice()).unwrap_or_default();
    let gone = ui.history.done().map(|h| h.gone.as_slice()).unwrap_or_default();
    let total = hits.len() + gone.len();
    let height = list.height.saturating_sub(1);
    let start = follow(ui.sel, height) as usize;
    // Narrow: the modification date goes to the panel below.
    let wide = area.width >= 100;
    let (disk_w, scan_w) = if wide { (16, 23) } else { (12, 16) };
    let fixed = disk_w + 10 + scan_w + if wide { 11 } else { 0 } + 5;
    let path_width = (list.width as usize).saturating_sub(fixed).max(12);
    // A disk that is not connected: "off" stays visible, the name is shortened.
    let disk_cell = |p: &Place| {
        if p.connected {
            return p.disk_name.clone();
        }
        let room = disk_w - 5;
        let name: String = if p.disk_name.chars().count() > room { p.disk_name.chars().take(room - 1).chain(['…']).collect() } else { p.disk_name.clone() };
        format!("{name} off")
    };
    // Narrow: the day of the scan without the time.
    let when = |rfc: &str| {
        let full = text::search::scan_when(rfc, now);
        if wide { full } else { full.rsplit_once(' ').map(|(day, _)| day.to_string()).unwrap_or(full) }
    };
    let size_cell = |kind: Kind, size: u64| if kind == Kind::Dir { "folder".to_string() } else { format_size(size, u) };
    let mut rows: Vec<Row> = Vec::new();
    for i in start..total.min(start + height as usize) {
        let (mut cells, scan, mtime, dim, is_gone) = match hits.get(i) {
            Some(h) => (
                vec![disk_cell(&h.place), ellipsize_start(&below_root(&h.path, &h.place.root), path_width), size_cell(h.kind, h.size)],
                format!("#{} · {}", h.scan.id, when(&h.scan.started_at)),
                h.mtime,
                !h.place.connected,
                false,
            ),
            None => {
                let l = &gone[i - hits.len()];
                let when = l.gone_in.as_ref().map(|g| format!("gone #{} · {}", g.id, when(&g.started_at))).unwrap_or_default();
                (
                    vec![disk_cell(&l.place), ellipsize_start(&below_root(&l.path, &l.place.root), path_width), size_cell(l.kind, l.size)],
                    if l.uncertain.is_some() { format!("{when} ?") } else { when },
                    l.mtime,
                    !l.place.connected,
                    true,
                )
            }
        };
        if wide {
            cells.push(text::changed_ago(mtime, now));
        }
        cells.push(scan);
        let row = Row::new(cells);
        rows.push(if i == ui.sel && !ui.editing {
            row.style(Style::new().fg(Color::Black).bg(ACCENT))
        } else if is_gone {
            row.style(Style::new().fg(Color::Yellow))
        } else if dim {
            row.style(Style::new().fg(Color::DarkGray))
        } else {
            row
        });
    }
    let mut header = vec!["Disk", "Path below the scanned folder", "On disk"];
    let mut widths = vec![Constraint::Length(disk_w as u16), Constraint::Min(12), Constraint::Length(10)];
    if wide {
        header.push("Modified");
        widths.push(Constraint::Length(11));
    }
    header.push("Scan");
    widths.push(Constraint::Length(scan_w as u16));
    f.render_widget(Table::new(rows, widths).header(Row::new(header).style(Style::new().bold())), list);

    let mut lines: Vec<Line> = Vec::new();
    match app.find_selected().filter(|_| !ui.editing) {
        Some(FindRow::Now(h)) => {
            lines.push(Line::styled(format!(" {}", h.path.lossy()), Style::new().bold()));
            let place = match &h.place.mount_point {
                Some(m) => format!("on {}, connected at {}", h.place.disk_name, m.display()),
                None => format!("on {}, not connected: connect it to get at the file", h.place.disk_name),
            };
            lines.push(Line::raw(format!(" {place} · modified {}", text::date(h.mtime))));
            lines.push(Line::raw(format!(" as of {} · Enter opens its folder in that scan", text::search::scan_ref(&h.scan, now))));
        }
        Some(FindRow::Gone(l)) => {
            lines.push(Line::styled(format!(" {}", l.path.lossy()), Style::new().bold()));
            if let Some(g) = &l.gone_in {
                lines.push(Line::raw(format!(
                    " on {}: last seen in {}, gone in {}: deleted or moved between those two",
                    l.place.disk_name,
                    text::search::scan_ref(&l.last_seen, now),
                    text::search::scan_ref(g, now)
                )));
            }
            let mut extra = Vec::new();
            if let Some(a) = &l.appeared_in {
                extra.push(format!("appeared in #{}", a.id));
            }
            if let Some(m) = &l.moved_to {
                extra.push(format!("probably moved to {m} (same name)"));
            }
            if let Some(why) = &l.uncertain {
                extra.push(format!("uncertain: {why}"));
            }
            if !extra.is_empty() {
                lines.push(Line::styled(format!(" {}", extra.join(" · ")), Style::new().fg(Color::Yellow)));
            }
            lines.push(Line::styled(
                " DiskDrift cannot bring it back: look in the Trash, Time Machine or the disk's own backups. Enter opens the last scan that has it.",
                Style::new().fg(Color::DarkGray),
            ));
        }
        None => {}
    }
    if let Some(n) = &ui.note {
        lines.push(Line::styled(format!(" {n}"), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray))),
        detail,
    );
}

fn draw_careful(f: &mut Frame, app: &App, area: Rect) {
    let ui = &app.careful;
    let u = units(app);
    let now = chrono::Local::now().timestamp();
    let (title, style) = match ui.on {
        Some(true) => (" Careful mode — ON: external disks are not mounted ", Style::new().fg(Color::Red).bold()),
        Some(false) => (" Careful mode — turning on… ", Style::new().fg(Color::Yellow)),
        None => (" Careful mode — off ", Style::new()),
    };
    let block = Block::bordered().title(Span::styled(title, style));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list_h = (ui.held.len() as u16 + 1).clamp(2, 8);
    let [intro, list, log] = Layout::vertical([Constraint::Length(4), Constraint::Length(list_h), Constraint::Min(2)]).areas(inner);
    let intro_text = if ui.is_on() {
 " On: an external disk connected now is not mounted, so nothing is written to it. Connect old disks one at a time; \
         g scans the selected one read-only with one thread and ejects it. A hardware write blocker is the only real guarantee."
            .to_string()
    } else {
        " For old, possibly failing disks: while it is on, an external disk you connect is not mounted, so nothing is written to it \
         (no repair, no journal replay, no Finder or Spotlight files). DiskDrift then mounts it read-only, scans it gently and ejects it. \
         o turns it on; quitting DiskDrift turns it off."
            .to_string()
    };
    f.render_widget(Paragraph::new(intro_text).wrap(Wrap { trim: false }), intro);

    let rows: Vec<Row> = ui
        .held
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let v = &h.volume.info;
            let what = match (&h.disk, &h.doubt) {
                (_, Some(_)) => "doubtful: g asks".to_string(),
                (Some(_), None) => h.last_scan.as_deref().map(|a| format!("known, scanned {}", text::search::scan_when(a, now))).unwrap_or_else(|| "known, never scanned".into()),
                (None, None) => "new".into(),
            };
            let state = match &ui.running {
                Some((bsd, p, started)) if bsd == h.bsd() => {
                    format!("scanning: {} entries, {}s", format_count(p.entries.load(Ordering::Relaxed)), started.elapsed().as_secs())
                }
                _ => "held, not mounted".into(),
            };
            let device = [v.device.bus.as_deref(), v.device.model.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
            let row = Row::new(vec![h.name(), v.fs_type.clone(), v.capacity_bytes.map(|b| format_size(b, u)).unwrap_or_else(|| "-".into()), device, what, state]);
            if i == ui.sel { row.style(Style::new().fg(Color::Black).bg(ACCENT)) } else { row }
        })
        .collect();
    let widths = [Constraint::Min(12), Constraint::Length(6), Constraint::Length(9), Constraint::Min(10), Constraint::Length(24), Constraint::Length(26)];
    let header = Row::new(vec!["Disk", "FS", "Size", "Device", "In the store", "Now"]).style(Style::new().bold());
    if ui.held.is_empty() {
        let empty = if ui.is_on() { " No disk held back yet." } else { "" };
        f.render_widget(Paragraph::new(empty).style(Style::new().fg(Color::DarkGray)), list);
    } else {
        f.render_widget(Table::new(rows, widths).header(header), list);
    }
    let height = log.height.saturating_sub(1) as usize;
    let start = ui.log.len().saturating_sub(height);
    let lines: Vec<Line> = ui.log[start..].iter().map(|l| Line::raw(format!(" {} {}", l.at, l.text))).collect();
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray))),
        log,
    );
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    use diskdrift_app::config::FIELDS;
    let ui = &app.settings_ui;
    let home = app.settings.home.display().to_string();
    let title_path = home_path(&app.settings.config_path.value.display().to_string(), &home, area.width.saturating_sub(16));
    let block = Block::bordered().title(format!(" Settings — {title_path} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let file = match &ui.file {
        Ok(c) => c,
        Err(e) => {
            f.render_widget(Paragraph::new(format!("\n {e}\n\n Fix the file by hand; nothing is overwritten.")).wrap(Wrap { trim: true }), inner);
            return;
        }
    };
    let [list, detail] = Layout::vertical([Constraint::Min(3), Constraint::Length(5)]).areas(inner);
    let rows: Vec<Row> = FIELDS
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let is_sel = i == ui.sel;
            let value = match (&ui.edit, is_sel) {
                (Some(text), true) => format!("{text}▏"),
                // A command-line flag wins this session: show what is used.
                _ => match field.session_value(&app.settings) {
                    Some(v) => home_path(&v, &home, 60),
                    None => field.display(file),
                },
            };
            let note = if field.overridden(&app.settings) { "command-line flag" } else { "" };
            let row = Row::new(vec![Cell::from(field.label), Cell::from(value), Cell::from(note).style(Style::new().fg(Color::Yellow))]);
            match (&ui.edit, is_sel) {
                (Some(_), true) => row.style(Style::new().fg(Color::Black).bg(Color::Yellow)),
                _ => selected(row, is_sel),
            }
        })
        .collect();
    let header = Row::new(["Setting", "Value", ""]).style(Style::new().bold());
    let flagged = FIELDS.iter().any(|f| f.overridden(&app.settings));
    let widths = [Constraint::Length(29), Constraint::Min(20), Constraint::Length(if flagged { 18 } else { 0 })];
    f.render_widget(Table::new(rows, widths).header(header), list);
    let field = FIELDS[ui.sel.min(FIELDS.len() - 1)];
    let mut lines = vec![Line::raw(format!(" {} ({})", field.help, field.key))];
    if field.overridden(&app.settings) {
        lines.push(Line::styled(
            format!(" This session a command-line flag sets it; the file's value, {}, applies when you start without the flag.", field.display(file)),
            Style::new().fg(Color::Yellow),
        ));
    }
    match &ui.message {
        Some(Ok(m)) => lines.push(Line::styled(format!(" {m}"), Style::new().fg(Color::Green))),
        Some(Err(m)) => lines.push(Line::styled(format!(" {m}"), Style::new().fg(Color::Red))),
        None => lines.push(Line::styled(
            if app.settings.config_exists { " Changes are saved at once." } else { " No config file yet: the first change creates it." },
            Style::new().fg(Color::DarkGray),
        )),
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::TOP)), detail);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hint = match app.screen {
        Screen::Dashboard if app.scan_running() => " ↑↓←→ Navigate  Enter Select  x Stop scan (asks)  ? Help  Q Quit",
        Screen::Dashboard => " ↑↓←→ Navigate  Enter Select  S Scan  D Diff  I Doctor  ? Help  Q Quit",
        Screen::Doctor if app.doctor.is_running() => " n Skip this location  Tab Suggestions  Esc Back (keeps running)  Q Quit",
        Screen::Doctor => " ↑↓/PgUp/PgDn Scroll  Tab/Enter Suggestions  F5 Measure again  Esc Back  Q Quit",
        Screen::Recover if app.doctor.is_running() => " n Skip this location  Tab Diagnosis  Esc Back (keeps running)  Q Quit",
        Screen::Analyze => " ↑↓ Select  Enter Open folder  Esc/← Up a level  F5 Measure now  Q Quit",
        Screen::Recover => " ↑↓ Select  Enter Open  a Analyse folder  Tab Diagnosis  F5 Measure again  Esc Back  Q Quit",
        Screen::Scan if matches!(&app.scan, ScanStage::AskDisk { .. }) => " s Same disk  n Another disk  Esc Not now",
        Screen::Scan if app.text_input_active() => " Type the folder  Tab Next field  Ctrl+U Clear  Enter Continue  Esc Back",
        Screen::Scan if app.scan_running() => " ← Back (keeps scanning)  Esc/x Stop (asks)  Q Quit",
        Screen::Scan if matches!(&app.scan, ScanStage::Confirm { .. }) => " Enter Start  Esc Change  Q Quit",
        Screen::Scan if matches!(&app.scan, ScanStage::Finished(Ok(o)) if o.snapshot.status.is_usable() && !o.cancelled) => {
            " Enter Browse  c Changes since the previous scan  n New scan  Esc Back  Q Quit"
        }
        Screen::Scan => " Enter/n New scan  Esc Back  Q Quit",
        Screen::Usage => " F5 Read again  I Where did the space go (Doctor)  ↑↓ Scroll  Esc Back  Q Quit",
        Screen::Snapshots if app.snap_remove.is_some() => " y Remove them  any other key Keep them",
        Screen::Snapshots if app.store_job.is_some() => " x Stop  ↑↓ Select  Enter Browse  Esc Back (keeps going)",
        Screen::Snapshots if !app.snap_marks.is_empty() => " Space Mark  o Older  n No data  p Rules  Del Remove  Esc Clear marks",
        Screen::Snapshots if app.disk_filter.is_some() => " ↑↓ Select  Enter Open  c Changes  t Largest  Space Mark  * Every disk  Esc Back",
        Screen::Snapshots => " ↑↓ Select  Enter Browse  c Changes  t Largest  Space Mark  z Compact  Esc Back",
        Screen::Disks if app.disks.edit.is_some() => " Type  Ctrl+U Clear  Enter Save  Esc Cancel",
        Screen::Disks if app.disks.forget.is_some() => " y Forget it  any other key Keep it",
        Screen::Careful if app.careful.ask.is_some() => " s The same disk  n Another disk  any other key Not now",
        Screen::Careful if app.careful.running.is_some() => " x Stop the scan (it is still ejected)  ↑↓ Select  Esc Back (keeps scanning)",
        Screen::Careful if app.careful.is_on() => " ↑↓ Select  g Scan carefully  m Mount read-only for Finder  j Eject  o Turn off  Esc Back",
        Screen::Careful => " o Turn careful mode on  Esc Back  Q Quit",
        Screen::Find if app.find.editing => " Type  Enter Search  Ctrl+U Clear  ↓/Tab Results  Esc Back",
        Screen::Find if app.find.history.is_running() => " ↑↓ Select  Enter Open in its scan  x Stop the history search  / Search field  Q Quit",
        Screen::Find if app.find.history.done().is_some() => " ↑↓ Select  Enter Open in its scan  / Search field  F5 Again  Esc Field  Q Quit",
        Screen::Find => " ↑↓ Select  Enter Open in its scan  p Search the history too  / Search field  F5 Again  Esc Field  Q Quit",
        Screen::Disks => " Enter Scans  l Label  o Describe  +/- Set  g Scan now  f Forget  w Its set  F5 Refresh  Esc Back",
        Screen::Diff if app.diff.focus == DiffFocus::PickOld && !matches!(app.diff.result, Job::Idle) => {
            " ↑↓ Choose  Enter Next step  Esc Keep the current comparison"
        }
        Screen::Diff if app.diff.focus == DiffFocus::PickOld => " ↑↓ Choose  Enter Next step  Esc Back",
        Screen::Diff if app.diff.focus == DiffFocus::PickNew => " ↑↓ Choose  Enter Compare  Esc/← Previous step",
        Screen::Diff if app.diff.focus == DiffFocus::Header => " Enter Choose other scans  ↓ Results  ←→ Tabs  Esc Back  Q Quit",
        Screen::Diff if app.diff.tab == DiffTab::Summary => " ↑↓ Scroll  Enter Browse the changes  ←→ Tabs  p Other scans  Esc Back  Q Quit",
        Screen::Diff => " ↑↓ Select  Enter Browse here  ←→ Tabs  o Order  f Min size  p Other scans  Esc Back",
        Screen::Top if app.top.focus == TopFocus::Pick => " ↑↓ Choose  Enter Show this scan  Esc Back",
        Screen::Top if app.top.focus == TopFocus::Header => " Enter Choose another scan  ↓ List  ←→ Files/Folders  m Sort  Esc Back  Q Quit",
        Screen::Top => " ↑↓ Select  Enter Browse there  ←→ Files/Folders  m Sort  p Other scan  Esc Back  Q Quit",
        Screen::Settings if app.settings_ui.edit.is_some() => " Type the value  Ctrl+U Clear  Enter Save  Esc Cancel",
        Screen::Settings if matches!(app.selected_field().kind, FieldKind::Text | FieldKind::Number) => " ↑↓ Select  Enter Edit  Esc Back  Q Quit",
        Screen::Settings => " ↑↓ Select  Enter Toggle (saved at once)  Esc Back  Q Quit",
        Screen::Browse if matches!(app.browse.as_ref().map(|b| b.source), Some(BrowseSource::Diff { .. })) => {
            " ↑↓ Select  Enter/→ Open folder  Esc/← Up a level  c This folder in the newer scan  a Analyse now  Q Quit"
        }
        Screen::Browse => " ↑↓ Select  Enter/→ Open folder  Esc/← Up a level  c What changed here  a Analyse now  Q Quit",
        _ => " ↑↓/PgUp/PgDn Scroll  Esc Back  ? Help  Q Quit",
    };
    f.render_widget(Paragraph::new(fit_hint(hint, area.width as usize)).style(Style::new().fg(Color::DarkGray)), area);
}

/// A footer that fits: when the terminal is narrow, hints are dropped from the
/// middle so the first ones (what the screen is for) and the way out (Esc, Q)
/// stay visible.
fn fit_hint(hint: &str, width: usize) -> String {
    if hint.chars().count() <= width {
        return hint.to_string();
    }
    let parts: Vec<&str> = hint.trim_start().split("  ").collect();
    let exit = |p: &&str| p.starts_with("Esc") || p.starts_with("Q ") || p.starts_with("← Back");
    let mut keep: Vec<bool> = parts.iter().map(|_| true).collect();
    let len = |keep: &[bool]| 1 + parts.iter().zip(keep).filter(|(_, k)| **k).map(|(p, _)| p.chars().count() + 2).sum::<usize>();
    for i in (1..parts.len()).rev() {
        if len(&keep) <= width {
            break;
        }
        if !exit(&parts[i]) {
            keep[i] = false;
        }
    }
    let out: Vec<&str> = parts.iter().zip(&keep).filter(|(_, k)| **k).map(|(p, _)| *p).collect();
    format!(" {}", out.join("  "))
}

fn draw_confirm(f: &mut Frame, area: Rect, msg: &str) {
    let w = (msg.chars().count() as u16 + 6).min(area.width);
    let rect = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + area.height / 2, width: w, height: 3.min(area.height) };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(msg).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(Block::bordered().border_style(Style::new().fg(Color::Yellow))),
        rect,
    );
}

pub fn help_text(app: &App) -> String {
    let s = &app.settings;
    format!(
        "Keyboard shortcuts (case-insensitive outside text fields)\n\n\
         \x20 ↑ ↓            Move selection / scroll\n\
         \x20 ← →            Switch menu column, tab or snapshot\n\
         \x20 Enter          Open / confirm\n\
         \x20 Esc            Back; on the dashboard asks to quit (Enter quits)\n\
         \x20 Tab/Shift+Tab  Next/previous item or field\n\
         \x20 S  Scan        H  Snapshot history   D  Compare snapshots\n\
         \x20 T  Top files   I  Doctor             R  Recovery suggestions\n\
         \x20 U  Usage       E  Settings           K  My disks     F or /  Find a file\n\
         \x20 C  Careful mode (old disks)   ?  This help   Q  Quit\n\
         \x20 History: opens on the newest scan with data; Enter browse it, c its changes since the\n\
         \x20          previous scan, t its largest files (details of the selected scan below);\n\
         \x20          Space marks scans to remove, o older ones, n those without data, p by the\n\
         \x20          retention rules (Settings); Del removes (asks, says what it frees), z compacts\n\
         \x20 Compare: opens on the newest scan vs the previous one; ↑ (top) or p picks other scans\n\
         \x20          in two steps (from, then to), Enter browse the changes, o order, f minimum size\n\
         \x20 Largest: Enter browse there, ←→ files/folders, m space on disk/file size, ↑ (top) or p\n\
         \x20          another scan\n\
         \x20 Browse:  Enter/→ open a folder, Esc/← up a level (on to the scan root),\n\
         \x20          c what changed in this folder since the previous scan (c again: back),\n\
         \x20          a analyse the selected folder as it is now (what each subfolder is)\n\
         \x20 Doctor/Recover: one measurement for both, Tab (Enter in Doctor) switches, F5 measures again\n\
         \x20 Recover: Enter analyse a folder (on growth: browse what changed), a analyse, Esc up,\n\
         \x20          n skip the location being measured (also in Doctor)\n\
         \x20 Scan:    type a folder (~ is your home), Tab name, Ctrl+U clear, Enter continue; then\n\
         \x20          Enter browse it, c changes since the previous scan, n new scan\n\
         \x20 Usage:   F5 reads the volumes again\n\
         \x20 My disks: Enter its scans (History, Compare, Largest files and the scan form follow;\n\
         \x20          * in History shows every disk again), l label, o description, + / - set,\n\
         \x20          g scan it now (when connected), f forget it (asks; removes its scans, never files)\n\
         \x20 Find:    a part of a name, *.pdf, re:regex or a/b (path); case and diacritics ignored;\n\
         \x20          Enter opens the result in its scan; nothing now: the history is searched (x stops),\n\
         \x20          p searches it also when something was found\n\
         \x20 Careful: o on/off; while on, external disks are not mounted (nothing is written);\n\
         \x20          g scans the held disk read-only with one thread and ejects it, m mounts it\n\
         \x20          read-only for Finder, j ejects it, x stops a scan. A hardware write blocker\n\
         \x20          is the only real guarantee for a disk that matters\n\n\
         DiskDrift never modifies, moves or deletes your files.\n\n\
         Config file:     {}{}\n\
         Snapshot store:  {}\n",
        s.config_path.value.display(),
        if s.config_exists { "" } else { " (not present, defaults in use)" },
        s.storage_path.value.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{key, settings};
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn dashboard_shows_menu_and_storage() {
        let app = App::new(settings());
        let s = render(&app, 90, 20);
        assert!(s.contains("[S] Scan disk"));
        assert!(s.contains("[?] Help"));
        assert!(s.contains("Storage: /nonexistent-home/.diskdrift/data"));
    }

    #[test]
    fn every_screen_renders_in_tiny_terminals() {
        for screen in MENU.iter().copied().chain([Screen::Dashboard]) {
            let mut app = App::new(settings());
            app.handle_key(key(KeyCode::Char(screen.hotkey())));
            app.screen = screen;
            for (w, h) in [(20, 5), (40, 10), (80, 24), (200, 60), (1, 1)] {
                let _ = render(&app, w, h);
            }
            app.confirm_quit = true;
            let _ = render(&app, 30, 8);
            app.confirm_quit = false;
            app.confirm_exit = true;
            let _ = render(&app, 30, 8);
        }
    }

    #[test]
    fn recover_list_analysis_and_log_render() {
        use crate::app::tests::{analysis, recover_report};
        use crate::app::Msg;
        let dir = std::env::temp_dir();
        let mut app = App::new(settings());
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_msg(Msg::DoctorStep(diskdrift_app::diagnostics::doctor::DoctorStep::OpenFiles));
        let s = render(&app, 100, 20);
        assert!(s.contains("checking deleted-but-open files"), "{s}");
        app.handle_msg(Msg::Recover(Box::new(Ok(recover_report(&dir)))));
        let s = render(&app, 120, 20);
        assert!(s.contains("candidate 1") && s.contains("Enter: analyse this folder"), "{s}");
        app.handle_key(key(KeyCode::Enter));
        app.handle_msg(Msg::Analyze { id: 1, result: Box::new(Ok(analysis(&dir))) });
        let s = render(&app, 140, 24);
        assert!(s.contains("sub/") && s.contains("regenerable") && s.contains("no installed app"), "{s}");
        assert!(s.contains("Last change inside"), "detail of the selected item: {s}");
        for (w, h) in [(20, 5), (1, 1), (60, 10)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn browse_renders_levels_and_changes() {
        use crate::app::tests::browse_listing;
        use crate::app::{BrowseSource, Msg};
        use diskdrift_app::core::browse::{DiffChild, DiffListing};
        use diskdrift_app::core::diff::Change;
        let mut app = App::new(settings());
        app.open_browse(BrowseSource::Snapshot(7), None, None);
        let s = render(&app, 100, 20);
        assert!(s.contains("Browse #7") && s.contains("reading…") && s.contains("On disk"), "the frame is there while loading: {s}");
        app.handle_msg(Msg::Browse { id: 1, result: Box::new(Ok(browse_listing("/r", "/r", &[("Library", true), ("big.iso", false)]))) });
        let s = render(&app, 110, 20);
        assert!(s.contains("Browse #7 2026-10-21 10:00 “base”") && s.contains("Library/") && s.contains("big.iso") && s.contains("50%"), "{s}");
        // Entering a folder keeps the parent's rows (greyed) until the folder is read.
        app.handle_key(key(KeyCode::Enter));
        let s = render(&app, 110, 20);
        assert!(s.contains("/r/Library") && s.contains("reading…") && s.contains("big.iso"), "{s}");
        app.handle_msg(Msg::Browse { id: 2, result: Box::new(Ok(browse_listing("/r", "/r/Library", &[("Caches", true)]))) });
        let s = render(&app, 110, 20);
        assert!(s.contains("Caches/") && !s.contains("big.iso") && !s.contains("reading…"), "{s}");

        let BrowseData::Snapshot(b) = browse_listing("/r", "/r", &[]) else { unreachable!() };
        let child = |name: &str, change, old, new| DiffChild {
            path: format!("/r/{name}").into_bytes().into(),
            kind: diskdrift_app::core::record::Kind::Dir,
            change,
            old_allocated: old,
            new_allocated: new,
            delta_allocated: new as i64 - old as i64,
            changed_entries: 3,
            visibility_changes: 0,
            mtime: 0,
        };
        let data = BrowseData::Diff(Box::new(diskdrift_app::snapshots::DiffBrowseResult {
            old: b.snapshot.clone(),
            new: b.snapshot.clone(),
            path: "/r".into(),
            listing: DiffListing {
                dir: Some(child("", Change::Modified, 10, 5_000_000)),
                items: vec![child("Downloads", Change::Modified, 0, 4_000_000), child("gone", Change::Deleted, 10, 0)],
                more_items: 0,
                more_delta_allocated: 0,
                unchanged_items: 4,
            },
        }));
        app.open_browse(BrowseSource::Diff { old: 7, new: 7 }, None, None);
        app.handle_msg(Msg::Browse { id: 3, result: Box::new(Ok(data)) });
        let s = render(&app, 110, 20);
        assert!(s.contains("+4.0 MB") && s.contains("Downloads/") && s.contains("deleted") && s.contains("in 2 of 6 items here"), "{s}");
        for (w, h) in [(20, 5), (1, 1), (60, 10)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn narrow_footers_keep_the_way_out() {
        let hint = " ↑↓ Select  Enter Browse  c Changes since the previous scan  t Largest files  Esc Back  Q Quit";
        assert_eq!(fit_hint(hint, 200), hint);
        let short = fit_hint(hint, 60);
        assert!(short.chars().count() <= 60, "{short}");
        assert!(short.starts_with(" ↑↓ Select") && short.ends_with("Esc Back  Q Quit"), "{short}");
    }

    #[test]
    fn help_lists_config_and_store() {
        let app = App::new(settings());
        let h = help_text(&app);
        assert!(h.contains("/nonexistent-home/.diskdrift/config.json"));
        assert!(h.contains("never modifies"));
    }

    #[test]
    fn low_space_rule() {
        assert!(is_low(16_000_000_000, 500_000_000_000));
        assert!(!is_low(120_000_000_000, 500_000_000_000));
        assert!(is_low(300_000_000_000, 4_000_000_000_000));
    }

    #[test]
    fn window_clamps() {
        let v: Vec<u32> = (0..10).collect();
        assert_eq!(window(&v, 0, 3).0, &[0, 1, 2]);
        assert_eq!(window(&v, 50, 3).0, &[7, 8, 9]);
        assert_eq!(window(&v, 0, 0).0.len(), 0);
        assert_eq!(window(&Vec::<u32>::new(), 5, 3).0.len(), 0);
    }

    #[test]
    fn find_shows_current_and_gone_results_at_80_and_120_columns() {
        use crate::app::tests::{find_gone, find_hit};
        use crate::app::Msg;
        use diskdrift_app::search::{Current, History};
        let mut app = App::new(settings());
        app.open(Screen::Find);
        for c in "wazna".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let typing = render(&app, 80, 24);
        assert!(typing.contains("Find: wazna"), "{typing}");
        assert!(typing.contains("case and diacritics ignored"), "{typing}");
        app.handle_key(key(KeyCode::Enter));
        let hits = vec![find_hit("/Volumes/backupy-2014/Docs/wazna-plan.txt", false)];
        app.handle_msg(Msg::FindCurrent { id: 1, result: Box::new(Ok(Current::for_test(Vec::new(), hits, 1))) });
        app.handle_key(key(KeyCode::Char('p')));
        let mut gone = find_gone("/Volumes/backupy-2014/Docs/wazna10.txt");
        gone.moved_to = Some("/Volumes/backupy-2014/Old/wazna10.txt".into());
        let h = History { gone: vec![gone], gone_total: 1, pairs: 7, pairs_done: 7, ..Default::default() };
        app.handle_msg(Msg::FindHistory { id: 1, result: Box::new(Ok(h)) });
        for (w, h) in [(80, 24), (120, 30)] {
            let s = render(&app, w, h);
            assert!(s.contains("1 match in"), "{s}");
            assert!(s.contains("In the history: 1 path no longer there."), "{s}");
            assert!(s.contains("Docs/wazna-plan.txt") && s.contains("Docs/wazna10.txt"), "{s}");
            assert!(s.contains(" off "), "a disk that is not connected is marked: {s}");
            assert!(s.contains("gone #13"), "{s}");
        }
        app.handle_key(key(KeyCode::Down));
        let s = render(&app, 120, 30);
        assert!(s.contains("last seen in scan #12 (3 Oct 14:20), gone in scan #13 (10 Oct 09:05)"), "{s}");
        assert!(s.contains("probably moved to /Volumes/backupy-2014/Old/wazna10.txt"), "{s}");
    }

    #[test]
    fn careful_mode_is_visible_on_every_screen_and_lists_held_disks() {
        use crate::app::Msg;
        use crate::app::tests::held;
        let mut app = App::new(settings());
        app.open(Screen::Careful);
        let off = render(&app, 100, 24);
        assert!(off.contains("Careful mode — off") && off.contains("o turns it on"), "{off}");
        assert!(!off.contains("CAREFUL MODE"));
        app.careful.on = Some(true);
        app.handle_msg(Msg::CarefulHeld(Box::new(Ok(held("disk9s1", "OLD", None)))));
        let on = render(&app, 100, 24);
        assert!(on.contains("ON: external disks are not mounted"), "{on}");
        assert!(on.contains("OLD") && on.contains("held, not mounted") && on.contains("USB Old Stick"), "{on}");
        assert!(on.contains("g Scan carefully"), "{on}");
        app.screen = Screen::Dashboard;
        assert!(render(&app, 80, 24).contains("CAREFUL MODE"), "the header says so everywhere");
    }

    #[test]
    fn my_disks_fits_80_columns_and_explains_the_selected_disk() {
        use diskdrift_app::disks::{DiskList, DiskRow};
        use diskdrift_app::storage::disks::{Disk, Identity, VolumeFacts};
        let disk = |id: &str, label: Option<&str>, ident: Identity, fs: &str| Disk {
            id: id.into(),
            identity: ident,
            label: label.map(String::from),
            description: None,
            sets: vec!["stare-dyski".into()],
            volume: VolumeFacts { name: Some("OLD".into()), fs_type: fs.into(), capacity_bytes: Some(500_107_862_016) },
            device: None,
            first_seen: "2014-03-01T10:00:00+01:00".into(),
            last_seen: "2014-03-01T10:00:00+01:00".into(),
            last_mount_point: Some("/Volumes/OLD".into()),
        };
        let row = |d: Disk, connected: bool| DiskRow {
            disk: d,
            known: true,
            last_size: Some(288_000_000_000),
            last_root: None,
            connected,
            mount_point: connected.then(|| "/".into()),
            snapshots: 3,
            last_scan: Some("2014-03-01T10:00:00+01:00".into()),
            store_bytes: 1000,
        };
        let mut app = App::new(settings());
        app.screen = Screen::Disks;
        app.disks.list = Job::Done(Box::new(DiskList {
            store_path: "/s".into(),
            store_exists: true,
            disks: vec![
                row(disk("d11111111", None, Identity::new(Strength::Uuid, "U"), "apfs"), true),
                row(disk("d22222222", Some("backupy-2014"), Identity::new(Strength::Weak, "w"), "ntfs"), false),
            ],
        }));
        app.disks.sel = 1;
        let s = render(&app, 80, 24);
        assert!(s.contains("My disks — 2 known, 1 connected"), "{s}");
        assert!(s.contains("backupy-2014 ?") && s.contains("stare-dyski"), "{s}");
        assert!(s.contains("not connected (last at /Volumes/OLD)"), "{s}");
        assert!(s.contains("recognised weakly"), "{s}");
        assert!(s.contains("f Forget") && s.contains("Esc Back"), "{s}");
        app.disks.forget = Some(diskdrift_app::disks::Forget { disk: disk("d22222222", Some("backupy-2014"), Identity::new(Strength::Weak, "w"), "ntfs"), snapshots: vec![1, 2, 3], bytes: 1000, done: false });
        let s = render(&app, 80, 24);
        assert!(s.contains("removes its 3 snapshots") && s.contains("y forgets"), "{s}");
    }

    #[test]
    fn the_dashboard_lists_disks_connected_or_not() {
        use diskdrift_app::disks::{DiskList, DiskRow};
        use diskdrift_app::storage::disks::{Disk, Identity, VolumeFacts};
        let disk = |id: &str, label: &str| Disk {
            id: id.into(),
            identity: Identity::new(Strength::Uuid, id),
            label: Some(label.into()),
            description: None,
            sets: Vec::new(),
            volume: VolumeFacts { name: Some("V".into()), fs_type: "hfs".into(), capacity_bytes: None },
            device: None,
            first_seen: "2026-01-01T10:00:00+01:00".into(),
            last_seen: "2026-01-01T10:00:00+01:00".into(),
            last_mount_point: None,
        };
        let mut app = App::new(settings());
        app.disks.list = Job::Done(Box::new(DiskList {
            store_path: "/s".into(),
            store_exists: true,
            disks: vec![
                DiskRow { disk: disk("d1", "macbook"), known: true, connected: true, mount_point: Some("/".into()), snapshots: 3, last_scan: Some("2026-01-01T10:00:00+01:00".into()), last_size: Some(288_000_000_000), last_root: Some("/nonexistent-home/Library".into()), store_bytes: 0 },
                DiskRow { disk: disk("d2", "backupy-2014"), known: true, connected: false, mount_point: None, snapshots: 1, last_scan: Some("2014-03-01T10:00:00+01:00".into()), last_size: Some(450_000_000_000), last_root: Some("/".into()), store_bytes: 0 },
                DiskRow { disk: disk("d3", "STICK"), known: false, connected: true, mount_point: Some("/Volumes/STICK".into()), snapshots: 0, last_scan: None, last_size: None, last_root: None, store_bytes: 0 },
            ],
        }));
        let s = render(&app, 100, 30);
        assert!(s.contains(" Disks "), "{s}");
        assert!(s.contains("backupy-2014") && s.contains("not connected") && s.contains("450 GB on the disk"), "{s}");
        assert!(s.contains("288 GB in ~/Library"), "the size of the folder scanned, not of the disk: {s}");
        let narrow = render(&app, 80, 30);
        assert!(narrow.contains("450 GB on the disk ") && narrow.contains("288 GB in ~/Library "), "the lines fit: {narrow}");
        assert!(s.contains("STICK") && s.contains("not scanned · K to label or scan it"), "{s}");
        // A short terminal keeps the menu and the status; the panel shrinks.
        let s = render(&app, 80, 20);
        assert!(s.contains("[S] Scan disk") && s.contains("Last scan"), "{s}");
    }
}
