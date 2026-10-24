//! Markdown report from raw results: medians, p95, throughput, footprint.

use crate::runner::{Results, Row};
use std::collections::BTreeMap;
use std::fmt::Write;

pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if s.is_empty() {
        return f64::NAN;
    }
    let m = s.len() / 2;
    if s.len().is_multiple_of(2) { (s[m - 1] + s[m]) / 2.0 } else { s[m] }
}

/// Nearest-rank p95 (with few trials this is the maximum).
pub fn p95(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if s.is_empty() {
        return f64::NAN;
    }
    let rank = ((0.95 * s.len() as f64).ceil() as usize).clamp(1, s.len());
    s[rank - 1]
}

fn mb(b: f64) -> String {
    format!("{:.1}", b / 1e6)
}

fn fmt_ms(ms: f64) -> String {
    if ms >= 10_000.0 { format!("{:.1} s", ms / 1000.0) } else if ms >= 100.0 { format!("{ms:.0} ms") } else { format!("{ms:.1} ms") }
}

fn rate(items: u64, ms: f64) -> String {
    if ms <= 0.0 {
        return "-".into();
    }
    let r = items as f64 / (ms / 1000.0);
    if r >= 1e6 { format!("{:.2} M/s", r / 1e6) } else { format!("{:.0} k/s", r / 1e3) }
}

type Key = (String, String, u64, String);

fn groups<'a>(rows: &'a [Row], group: &str) -> BTreeMap<Key, Vec<&'a Row>> {
    let mut m: BTreeMap<Key, Vec<&Row>> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.group == group) {
        m.entry((r.group.clone(), r.backend.clone(), r.n, r.param.clone())).or_default().push(r);
    }
    m
}

pub struct Agg {
    pub med: f64,
    pub p95: f64,
    pub trials: usize,
    pub failures: usize,
    pub items: u64,
    pub rss: f64,
    pub footprint: f64,
}

pub fn agg(rows: &[&Row]) -> Agg {
    let ok: Vec<&&Row> = rows.iter().filter(|r| r.ok).collect();
    let ms: Vec<f64> = ok.iter().map(|r| r.ms).collect();
    Agg {
        med: median(&ms),
        p95: p95(&ms),
        trials: rows.len(),
        failures: rows.len() - ok.len(),
        items: ok.first().map(|r| r.items).unwrap_or(0),
        rss: ok.iter().map(|r| r.rss as f64).fold(0.0, f64::max),
        footprint: median(&ok.iter().map(|r| r.footprint as f64).collect::<Vec<_>>()),
    }
}

pub fn lookup<'a>(res: &'a Results, group: &str, backend: &str, n: u64, param: &str) -> Vec<&'a Row> {
    res.rows.iter().filter(|r| r.group == group && r.backend == backend && r.n == n && r.param == param).collect()
}

pub fn render(res: &Results) -> String {
    let mut o = String::new();
    let m = &res.machine;
    let _ = writeln!(o, "# DiskDrift storage & scanner benchmarks — `{}` suite\n", res.suite);
    let _ = writeln!(o, "Run {} → {}  ", res.started, res.finished);
    let _ = writeln!(o, "Machine: {} · {} · {} CPUs · {:.0} GB RAM · {}  ", m.model, m.cpu, m.cpus, m.memory_bytes as f64 / 1e9, m.os);
    let _ = writeln!(o, "Storage: {} on {} · {}  ", m.filesystem, m.device, m.rustc);
    let _ = writeln!(o, "Cache: {}\n", m.cache_note);
    let _ = writeln!(o, "Times are the operation itself measured inside a fresh child process (median, p95 by nearest rank; with ≤5 trials p95 is the maximum). RSS is the child's peak resident memory, which includes the synthetic generator where it feeds the operation. Footprint is allocated bytes on disk.\n");

    // Generator baseline
    let gens = groups(&res.rows, "gen");
    if !gens.is_empty() {
        let _ = writeln!(o, "## Synthetic generator baseline\n\n| entries | generated | median | p95 | rate | peak RSS MB |\n|---:|---:|---:|---:|---:|---:|");
        for ((_, _, n, _), rows) in &gens {
            let a = agg(rows);
            let _ = writeln!(o, "| {n} | {} | {} | {} | {} | {} |", a.items, fmt_ms(a.med), fmt_ms(a.p95), rate(a.items, a.med), mb(a.rss));
        }
        let _ = writeln!(o);
    }

    // Ops per size
    let ops = ["import", "read", "top", "top-native", "prefix"];
    let mut sizes: Vec<u64> = res.rows.iter().filter(|r| r.group == "import").map(|r| r.n).collect();
    sizes.sort();
    sizes.dedup();
    if !sizes.is_empty() {
        let _ = writeln!(o, "## Single snapshot: import, read, queries\n");
        for n in &sizes {
            let _ = writeln!(o, "### {n} entries\n");
            let _ = writeln!(o, "| backend | import median (p95) | import rate | import RSS MB | footprint MB | bytes/entry | full read median (p95) | read rate | top-100 | native top-100 | prefix query |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
            let mut backends: Vec<String> = res.rows.iter().filter(|r| r.group == "import" && r.n == *n).map(|r| r.backend.clone()).collect();
            backends.dedup();
            for b in backends {
                let get = |g: &str| {
                    let rows: Vec<&Row> = res.rows.iter().filter(|r| r.group == g && r.backend == b && r.n == *n).collect();
                    (!rows.is_empty()).then(|| agg(&rows))
                };
                let imp = get("import").unwrap();
                let cell = |a: Option<Agg>| a.map(|a| format!("{} ({})", fmt_ms(a.med), fmt_ms(a.p95))).unwrap_or_else(|| "-".into());
                let read = get("read");
                let read_rate = read.as_ref().map(|a| rate(a.items, a.med)).unwrap_or_else(|| "-".into());
                let fail = if imp.failures > 0 { format!(" ({} failed)", imp.failures) } else { String::new() };
                let _ = writeln!(
                    o,
                    "| {b}{fail} | {} ({}) | {} | {} | {} | {:.1} | {} | {} | {} | {} | {} |",
                    fmt_ms(imp.med),
                    fmt_ms(imp.p95),
                    rate(imp.items, imp.med),
                    mb(imp.rss),
                    mb(imp.footprint),
                    imp.footprint / imp.items.max(1) as f64,
                    cell(read),
                    read_rate,
                    cell(get("top")),
                    cell(get("top-native")),
                    cell(get("prefix")),
                );
            }
            let _ = ops;
            let _ = writeln!(o);
        }
    }

    // Change-rate sweep
    let deltas = groups(&res.rows, "delta-write");
    if !deltas.is_empty() {
        let n = deltas.keys().next().unwrap().2;
        let _ = writeln!(o, "## Subsequent snapshot at {n} entries by change rate\n");
        let _ = writeln!(o, "Delta = complete comparison against the previous snapshot + write of the changes only. Full = write the whole new snapshot. Diff = full comparison report (via delta reconstruction / of two full snapshots).\n");
        let _ = writeln!(o, "| backend | change | ops | delta write | delta MB | full write | full MB | diff (delta) | diff (full/full) |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        let mut keys: Vec<&Key> = deltas.keys().collect();
        keys.sort_by(|a, b| a.1.cmp(&b.1).then(a.3.parse::<f64>().unwrap_or(0.0).partial_cmp(&b.3.parse::<f64>().unwrap_or(0.0)).unwrap()));
        for k in keys {
            let (b, c) = (&k.1, &k.3);
            let d = agg(&deltas[k]);
            let added = |g: &str| {
                let rows = lookup(res, g, b, n, c);
                median(&rows.iter().filter_map(|r| r.extra["addedBytes"].as_f64()).collect::<Vec<_>>())
            };
            let ops = deltas[k].first().and_then(|r| r.extra["ops"].as_u64()).unwrap_or(0);
            let f = agg(&lookup(res, "full-write", b, n, c));
            let dd = agg(&lookup(res, "diff-via-delta", b, n, c));
            let df = agg(&lookup(res, "diff-full-full", b, n, c));
            let pct = c.parse::<f64>().unwrap_or(0.0) * 100.0;
            let _ = writeln!(o, "| {b} | {pct}% | {ops} | {} | {} | {} | {} | {} | {} |", fmt_ms(d.med), mb(added("delta-write")), fmt_ms(f.med), mb(added("full-write")), fmt_ms(dd.med), fmt_ms(df.med));
        }
        let _ = writeln!(o);
    }

    // History
    let hist = groups(&res.rows, "history-write");
    if !hist.is_empty() {
        let n = hist.keys().next().unwrap().2;
        let _ = writeln!(o, "## History of snapshots at {n} entries, ~1% change per snapshot\n");
        let _ = writeln!(o, "| backend | strategy | snapshots | total write | median step | p95 step | footprint MB | × one full | distant diff (first↔last) | adjacent diff | read latest | list |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
        for (k, rows) in &hist {
            let r = rows[0];
            if !r.ok {
                let _ = writeln!(o, "| {} | {} | FAILED: {} |", k.1, k.3, r.error.as_deref().unwrap_or(""));
                continue;
            }
            let steps: Vec<f64> = r.extra["stepMs"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
            let fps: Vec<f64> = r.extra["footprintAfter"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
            let one_full = fps.first().copied().unwrap_or(f64::NAN);
            let get = |g: &str| fmt_ms(agg(&lookup(res, g, &k.1, n, &k.3)).med);
            let _ = writeln!(
                o,
                "| {} | {} | {} | {} | {} | {} | {} | {:.2} | {} | {} | {} | {} |",
                k.1,
                k.3,
                r.items,
                fmt_ms(steps.iter().sum()),
                fmt_ms(median(&steps)),
                fmt_ms(p95(&steps)),
                mb(r.footprint as f64),
                r.footprint as f64 / one_full,
                get("history-diff-distant"),
                get("history-diff-adjacent"),
                get("history-read-latest"),
                get("history-list"),
            );
        }
        let _ = writeln!(o);
    }

    // Robustness
    let rob: Vec<&Row> = res.rows.iter().filter(|r| r.group.starts_with("crash") || r.group.starts_with("diskfull")).collect();
    if !rob.is_empty() {
        let _ = writeln!(o, "## Crash safety and write failure\n");
        let _ = writeln!(o, "Crash: SIGKILL the writer half-way through importing snapshot 1, then verify snapshot 0 record-by-record against the generator and write another snapshot. Write failure: `RLIMIT_FSIZE` makes writes fail with EFBIG part-way (a stand-in for a full disk that does not touch the real volume).\n");
        let _ = writeln!(o, "| backend | step | detail | outcome |\n|---|---|---|---|");
        for r in rob {
            let expect_fail = r.group == "diskfull-write";
            let outcome = match (r.group.as_str(), r.ok) {
                ("crash-kill", _) => "killed".to_string(),
                (_, true) if expect_fail => "**unexpectedly succeeded**".to_string(),
                (_, false) if expect_fail => format!("failed as intended: {}", r.error.as_deref().unwrap_or("")),
                (_, true) => {
                    let snaps = r.extra.get("snapshots").map(|s| format!(" (listed: {s})")).unwrap_or_default();
                    format!("ok{snaps}")
                }
                (_, false) => format!("**FAILED**: {}", r.error.as_deref().unwrap_or("")),
            };
            let _ = writeln!(o, "| {} | {} | {} | {} |", r.backend, r.group, r.param, outcome);
        }
        let _ = writeln!(o);
    }

    // Scanner
    let scans = groups(&res.rows, "scan");
    if !scans.is_empty() {
        let _ = writeln!(o, "## Scanner (metadata only, real trees, read-only)\n");
        let firsts = groups(&res.rows, "scan-first");
        for ((_, _, _, p), rows) in &firsts {
            let a = agg(rows);
            let _ = writeln!(o, "- First traversal of `{p}` (getattrlistbulk, 8 threads): {} entries in {} ({}).", a.items, fmt_ms(a.med), rate(a.items, a.med));
        }
        let _ = writeln!(o, "\n| lister | tree, threads | entries | median (p95) | rate | peak RSS MB |\n|---|---|---:|---:|---:|---:|");
        for ((_, b, _, p), rows) in &scans {
            let a = agg(rows);
            let _ = writeln!(o, "| {b} | {p} | {} | {} ({}) | {} | {} |", a.items, fmt_ms(a.med), fmt_ms(a.p95), rate(a.items, a.med), mb(a.rss));
        }
        let e2e = groups(&res.rows, "scan-store");
        if !e2e.is_empty() {
            let _ = writeln!(o, "\n### End-to-end: scan + store (getattrlistbulk, 8 threads)\n\n| backend | tree | entries | median | rate | peak RSS MB | footprint MB | bytes/entry |\n|---|---|---:|---:|---:|---:|---:|---:|");
            for ((_, b, _, p), rows) in &e2e {
                let a = agg(rows);
                let _ = writeln!(o, "| {b} | {p} | {} | {} | {} | {} | {} | {:.1} |", a.items, fmt_ms(a.med), rate(a.items, a.med), mb(a.rss), mb(a.footprint), a.footprint / a.items.max(1) as f64);
            }
        }
        let _ = writeln!(o);
    }
    o
}
