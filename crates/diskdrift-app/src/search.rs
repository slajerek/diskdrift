//! `diskdrift find`: file search over the saved scans of every disk (spec 3.5).
//!
//! No index: the newest scan of each disk and folder is streamed once, and the
//! history is read from the stored reverse deltas, whose ops already say which
//! paths disappeared or appeared between two adjacent scans
//! (docs/research/search-index.md, section 3.2).

use crate::config::Effective;
use crate::disks::Volume;
use crate::snapshots::{AppError, Result, open_store};
use diskdrift_core::delta::{self, OpKind};
use diskdrift_core::paths::PathStack;
use diskdrift_core::pathser::NativePath;
use diskdrift_core::record::{Kind, Record};
use diskdrift_storage::{OpStream, Registry, SnapshotMeta, Store};
use regex_automata::meta::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// What to look for.
#[derive(Debug, Clone)]
pub struct Query {
    pub pattern: String,
    /// The pattern is a regular expression (also when it starts with `re:`).
    pub regex: bool,
    /// Match case and diacritics exactly.
    pub exact: bool,
    pub kind: Option<Kind>,
    /// Minimum allocated size.
    pub min_size: u64,
    /// Only this disk (label, short ID or prefix).
    pub disk: Option<String>,
    /// Only the disks of this set (spec 3C).
    pub set: Option<String>,
    /// Results kept per list; every match is still counted.
    pub limit: usize,
}

impl Query {
    pub fn new(pattern: &str) -> Query {
        Query { pattern: pattern.into(), regex: false, exact: false, kind: None, min_size: 0, disk: None, set: None, limit: 100 }
    }
}

/// When to search the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryMode {
    /// Only when nothing is found in the newest scans.
    Auto,
    Always,
    Never,
}

// ---------------------------------------------------------------- matching

/// Appends the search key of `text`: lower case, diacritics removed (NFD,
/// combining marks dropped, a few letters without a decomposition mapped).
/// Bytes that are not UTF-8 are kept, ASCII lowered.
pub fn fold_into(text: &[u8], out: &mut Vec<u8>) {
    fold(text, true, out)
}

fn fold(text: &[u8], lower: bool, out: &mut Vec<u8>) {
    if text.is_ascii() {
        out.extend(text.iter().map(|b| if lower { b.to_ascii_lowercase() } else { *b }));
        return;
    }
    let Ok(s) = std::str::from_utf8(text) else {
        out.extend(text.iter().map(|b| if lower { b.to_ascii_lowercase() } else { *b }));
        return;
    };
    let mut buf = [0u8; 4];
    let mut push = |c: char, out: &mut Vec<u8>| out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    for c in s.nfd() {
        if is_combining_mark(c) {
            continue;
        }
        let mapped: Option<&str> = match c {
            'ł' => Some("l"),
            'Ł' => Some("L"),
            'ø' => Some("o"),
            'Ø' => Some("O"),
            'đ' => Some("d"),
            'Đ' => Some("D"),
            'ß' => Some("ss"),
            'æ' => Some("ae"),
            'Æ' => Some("AE"),
            'œ' => Some("oe"),
            'Œ' => Some("OE"),
            'þ' => Some("th"),
            'Þ' => Some("TH"),
            'ı' => Some("i"),
            _ => None,
        };
        match mapped {
            Some(m) => out.extend(m.bytes().map(|b| if lower { b.to_ascii_lowercase() } else { b })),
            None if lower => c.to_lowercase().for_each(|l| push(l, out)),
            None => push(c, out),
        }
    }
}

enum How {
    Part(memchr::memmem::Finder<'static>),
    Re(Box<Regex>),
}

/// A compiled query pattern.
pub struct Matcher {
    how: How,
    /// Matched against the path below the scan root, not the name.
    on_path: bool,
    exact: bool,
}

fn glob_to_regex(glob: &str, on_path: bool) -> String {
    // A path glob matches whole components at the end of the path.
    let mut re = String::from(if on_path { "(?s)(?:^|/)" } else { "(?s)^" });
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    let any = if on_path { "[^/]" } else { "." };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
            }
            '*' => {
                re.push_str(any);
                re.push('*');
            }
            '?' => re.push_str(any),
            '[' => match chars[i + 1..].iter().position(|&x| x == ']') {
                Some(len) if len > 0 => {
                    let mut class: String = chars[i + 1..i + 1 + len].iter().collect();
                    if let Some(rest) = class.strip_prefix('!') {
                        class = format!("^{rest}");
                    }
                    re.push('[');
                    re.push_str(&class.replace('\\', "\\\\").replace('[', "\\["));
                    re.push(']');
                    i += len + 1;
                }
                _ => re.push_str("\\["),
            },
            c if "\\.+()|{}^$#&-~]".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    re
}

impl Matcher {
    pub fn new(q: &Query) -> Result<Matcher> {
        let (pattern, regex) = match q.pattern.strip_prefix("re:") {
            Some(rest) => (rest, true),
            None => (q.pattern.as_str(), q.regex),
        };
        if pattern.is_empty() {
            return Err(AppError::Usage("the search pattern is empty".into()));
        }
        let on_path = pattern.contains('/');
        let pattern = if on_path { pattern.trim_start_matches('/') } else { pattern };
        let mut folded = Vec::new();
        let glob = !regex && pattern.contains(['*', '?', '[']);
        let how = if regex || glob {
            // A regex keeps its case (`\D` is not `\d`) and matches case-insensitively.
            fold(pattern.as_bytes(), glob && !q.exact, &mut folded);
            let mut text = if q.exact { pattern.to_string() } else { String::from_utf8_lossy(&folded).into_owned() };
            if glob {
                text = glob_to_regex(&text, on_path);
            }
            let re = Regex::builder()
                .syntax(regex_automata::util::syntax::Config::new().case_insensitive(!q.exact).utf8(false))
                .build(&text)
                .map_err(|e| AppError::Usage(format!("invalid {}: {e}", if glob { "glob" } else { "regular expression" })))?;
            How::Re(Box::new(re))
        } else {
            if q.exact {
                folded.extend_from_slice(pattern.as_bytes());
            } else {
                fold_into(pattern.as_bytes(), &mut folded);
            }
            How::Part(memchr::memmem::Finder::new(&folded).into_owned())
        };
        Ok(Matcher { how, on_path, exact: q.exact })
    }

    pub fn on_path(&self) -> bool {
        self.on_path
    }

    /// Does `text` (a name, or the path below the root when [`Matcher::on_path`]) match?
    /// `buf` is scratch space.
    pub fn is_match(&self, text: &[u8], buf: &mut Vec<u8>) -> bool {
        let hay = if self.exact {
            text
        } else {
            buf.clear();
            fold_into(text, buf);
            buf.as_slice()
        };
        match &self.how {
            How::Part(f) => f.find(hay).is_some(),
            How::Re(re) => re.is_match(hay),
        }
    }
}

// ---------------------------------------------------------------- results

/// A scan, as results name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRef {
    pub id: u64,
    pub name: String,
    pub started_at: String,
}

impl ScanRef {
    fn of(m: &SnapshotMeta) -> ScanRef {
        ScanRef { id: m.id, name: m.name.clone(), started_at: m.started_at.clone() }
    }
}

/// Where a result was found: the disk and the folder scanned on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    /// Short disk ID (none for a scan without a disk).
    pub disk: Option<String>,
    /// Label, else volume name, else short ID; else the scan root.
    pub disk_name: String,
    pub connected: bool,
    /// Where the disk is mounted now, when connected.
    pub mount_point: Option<PathBuf>,
    /// The scan root (absolute, as scanned).
    pub root: String,
}

/// A match in the newest scan of a disk and folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    /// Full path as scanned.
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    /// Allocated bytes ("on disk").
    pub size: u64,
    pub logical_size: u64,
    pub mtime: i64,
    pub scan: ScanRef,
    #[serde(flatten)]
    pub place: Place,
}

/// What the history says about one path.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lifetime {
    /// Full path as last scanned.
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    /// Last known allocated size and modification time.
    pub size: u64,
    pub mtime: i64,
    #[serde(flatten)]
    pub place: Place,
    /// Newest scan that has it.
    pub last_seen: ScanRef,
    /// The first scan after `last_seen` without it; `None`: it is still there.
    pub gone_in: Option<ScanRef>,
    /// The scan it appeared in (the scan before did not have it); `None`: it
    /// is in the oldest scan searched (`oldest`).
    pub appeared_in: Option<ScanRef>,
    pub oldest: ScanRef,
    /// Times its size or date changed between adjacent scans.
    pub changes: u32,
    /// Why "gone" may be wrong (the newer scan could not see it).
    pub uncertain: Option<String>,
    /// The same name appeared elsewhere in `gone_in` (by name only).
    pub moved_to: Option<String>,
}

/// The newest scans and what matched in them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Current {
    pub pattern: String,
    pub store_path: PathBuf,
    pub store_exists: bool,
    /// The scans searched as "now", one per disk and folder.
    pub searched: Vec<SearchedScan>,
    pub hits: Vec<Hit>,
    /// Every match (more than `hits` when `truncated`).
    pub total: u64,
    pub truncated: bool,
    /// Only this disk was searched.
    pub disk: Option<String>,
    pub elapsed_ms: u64,
    #[serde(skip)]
    plan: Plan,
    #[serde(skip)]
    keys: Option<HashSet<(String, Vec<u8>)>>,
}

impl Current {
    /// Results as a search would return them, for tests of the screens.
    #[doc(hidden)]
    pub fn for_test(searched: Vec<SearchedScan>, hits: Vec<Hit>, total: u64) -> Current {
        Current {
            pattern: String::new(),
            store_path: PathBuf::new(),
            store_exists: true,
            searched,
            truncated: total > hits.len() as u64,
            hits,
            total,
            disk: None,
            elapsed_ms: 0,
            plan: Plan::default(),
            keys: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchedScan {
    pub scan: ScanRef,
    #[serde(flatten)]
    pub place: Place,
    pub entries: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    /// Paths that are gone, the most recently deleted first.
    pub gone: Vec<Lifetime>,
    pub gone_total: u64,
    /// For current matches: when they appeared and how often they changed
    /// (only with `--history`).
    pub current: Vec<Lifetime>,
    /// Pairs of adjacent scans read, of `pairs`.
    pub pairs_done: usize,
    pub pairs: usize,
    pub cancelled: bool,
    /// More than [`MAX_EVENTS`] matching changes: the search stopped there
    /// (newest pairs first), so older ones may be missing.
    pub too_many: bool,
    pub elapsed_ms: u64,
}

/// Matching changes a history search keeps before it stops (memory bound
/// for a query that matches much of the disk, such as one letter).
pub const MAX_EVENTS: usize = 100_000;

/// `diskdrift find`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResult {
    #[serde(flatten)]
    pub current: Current,
    pub history: Option<History>,
}

// ---------------------------------------------------------------- planning

/// The usable scans of one disk and folder, newest first.
#[derive(Debug, Clone)]
struct Group {
    disk_key: String,
    root_in_volume: String,
    scans: Vec<SnapshotMeta>,
    place: Place,
    /// A newer scan of an enclosing folder on the same disk (group index).
    covered_by: Option<usize>,
}

#[derive(Debug, Clone, Default)]
struct Plan {
    groups: Vec<Group>,
}

fn inside(inner: &str, outer: &str) -> bool {
    outer == "/" || inner == outer || inner.strip_prefix(outer).is_some_and(|r| r.starts_with('/'))
}

fn plan(store: &Store, reg: &Registry, mounted: &[Volume], only: Option<&[String]>) -> Result<Plan> {
    let mut groups: Vec<Group> = Vec::new();
    let metas = store.list()?;
    for m in metas.iter().rev().filter(|m| m.status.is_usable()) {
        if only.is_some_and(|ids| !m.disk.as_ref().is_some_and(|d| ids.contains(d))) {
            continue;
        }
        let disk_key = m.disk.clone().unwrap_or_else(|| format!("{}|{}", m.volume.uuid.as_deref().unwrap_or("-"), m.root));
        let riv = m.derived_root_in_volume();
        match groups.iter_mut().find(|g| g.disk_key == disk_key && g.root_in_volume == riv) {
            Some(g) => g.scans.push(m.clone()),
            None => {
                let known = m.disk.as_deref().and_then(|id| reg.get(id));
                let at = known.and_then(|d| mounted.iter().find(|v| v.identity == d.identity)).map(|v| v.mount_point.clone());
                let place = Place {
                    disk: m.disk.clone(),
                    disk_name: crate::disks::disk_name(reg, m).unwrap_or_else(|| m.root.clone()),
                    connected: at.is_some(),
                    mount_point: at,
                    root: m.root.clone(),
                };
                groups.push(Group { disk_key, root_in_volume: riv, scans: vec![m.clone()], place, covered_by: None });
            }
        }
    }
    // Groups are in order of their newest scan, newest first: an enclosing
    // folder scanned later covers this one.
    for i in 0..groups.len() {
        groups[i].covered_by = (0..i).find(|&j| {
            groups[j].disk_key == groups[i].disk_key
                && groups[j].covered_by.is_none()
                && inside(&groups[i].root_in_volume, &groups[j].root_in_volume)
                && groups[j].scans[0].started_at > groups[i].scans[0].started_at
        });
    }
    Ok(Plan { groups })
}

/// Path below the scan root, `/`-separated (empty for the root itself).
fn rel_path(comps: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, c) in comps.iter().enumerate().skip(1) {
        if i > 1 {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    out
}

/// The same path as a key that is equal across scans of different folders of one disk.
fn volume_key(g: &Group, rel: &[u8]) -> (String, Vec<u8>) {
    let mut p = g.root_in_volume.trim_end_matches('/').as_bytes().to_vec();
    if !rel.is_empty() {
        p.push(b'/');
        p.extend_from_slice(rel);
    }
    (g.disk_key.clone(), p)
}

/// The path below `root` of a full path of a scan with that root.
fn rel_of<'a>(root: &str, path: &'a [u8]) -> &'a [u8] {
    match path.strip_prefix(root.as_bytes()) {
        Some(r) => r.strip_prefix(b"/").unwrap_or(r),
        None => path,
    }
}

fn full_path(root: &str, rel: &[u8]) -> Vec<u8> {
    let mut p = root.as_bytes().to_vec();
    if !rel.is_empty() {
        if p.last() != Some(&b'/') {
            p.push(b'/');
        }
        p.extend_from_slice(rel);
    }
    p
}

fn passes(q: &Query, r: &Record) -> bool {
    q.kind.is_none_or(|k| k == r.kind) && r.alloc >= q.min_size
}

/// Runs `f` over `items` on up to one thread per core.
fn par_each<T: Sync>(items: &[T], f: impl Fn(usize, &T) + Sync) {
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(items.len()).max(1);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    f(i, item);
                }
            });
        }
    });
}

struct Found {
    hits: Vec<Hit>,
    keys: Vec<(String, Vec<u8>)>,
    total: u64,
    entries: u64,
}

/// Streams one snapshot and collects its matches.
fn search_snapshot(store: &Store, g: &Group, m: &SnapshotMeta, q: &Query, matcher: &Matcher, want_keys: bool, cancel: &AtomicBool) -> Result<Found> {
    let mut found = Found { hits: Vec::new(), keys: Vec::new(), total: 0, entries: 0 };
    let mut stack = PathStack::new();
    let mut buf = Vec::new();
    for (n, r) in store.records(m.id)?.enumerate() {
        let r = r.map_err(|e| AppError::Runtime(format!("reading scan #{}: {e}", m.id)))?;
        if n % 65536 == 0 && cancel.load(Ordering::Relaxed) {
            break;
        }
        found.entries += 1;
        stack.set(r.depth, &r.name).map_err(|e| AppError::Runtime(e.to_string()))?;
        if r.depth == 0 || !passes(q, &r) {
            continue;
        }
        let hit = if matcher.on_path() { matcher.is_match(&rel_path(stack.components()), &mut buf) } else { matcher.is_match(&r.name, &mut buf) };
        if !hit {
            continue;
        }
        found.total += 1;
        if want_keys {
            found.keys.push(volume_key(g, &rel_path(stack.components())));
        }
        if found.hits.len() < q.limit {
            found.hits.push(Hit {
                path: NativePath(stack.path_bytes()),
                kind: r.kind,
                size: r.alloc,
                logical_size: r.size,
                mtime: r.mtime,
                scan: ScanRef::of(m),
                place: g.place.clone(),
            });
        }
    }
    Ok(found)
}

/// Searches the newest scan of every disk and folder. `mounted`: the volumes
/// mounted now, when the caller has them (else they are read).
pub fn current(settings: &Effective, q: &Query, mounted: Option<&[Volume]>) -> Result<Current> {
    current_cancellable(settings, q, mounted, &AtomicBool::new(false))
}

pub fn current_cancellable(settings: &Effective, q: &Query, mounted: Option<&[Volume]>, cancel: &AtomicBool) -> Result<Current> {
    let start = std::time::Instant::now();
    let matcher = Matcher::new(q)?;
    let store_path = settings.storage_path.value.clone();
    let Some(store) = open_store(settings)? else {
        return Ok(Current {
            pattern: q.pattern.clone(),
            store_path,
            store_exists: false,
            searched: Vec::new(),
            hits: Vec::new(),
            total: 0,
            truncated: false,
            disk: None,
            elapsed_ms: 0,
            plan: Plan::default(),
            keys: None,
        });
    };
    let reg = store.registry()?;
    let disk = q.disk.as_deref().map(|d| crate::disks::resolve(&store, d)).transpose()?;
    let read;
    let mounted = match mounted {
        Some(v) => v,
        None => {
            read = crate::disks::volumes();
            &read
        }
    };
    let only: Option<Vec<String>> = match (&disk, &q.set) {
        (Some(d), _) => Some(vec![d.id.clone()]),
        (None, Some(set)) => {
            let ids: Vec<String> = reg.disks.iter().filter(|d| d.sets.iter().any(|x| x.eq_ignore_ascii_case(set))).map(|d| d.id.clone()).collect();
            if ids.is_empty() {
                return Err(AppError::Usage(format!("no disk is in the set '{set}' (see `diskdrift disks`)")));
            }
            Some(ids)
        }
        (None, None) => None,
    };
    let plan = plan(&store, &reg, mounted, only.as_deref())?;
    let want_keys = plan.groups.iter().any(|g| g.covered_by.is_some());
    let live: Vec<&Group> = plan.groups.iter().filter(|g| g.covered_by.is_none()).collect();
    let results: Mutex<Vec<Option<Result<Found>>>> = Mutex::new((0..live.len()).map(|_| None).collect());
    par_each(&live, |i, g| {
        let r = search_snapshot(&store, g, &g.scans[0], q, &matcher, want_keys, cancel);
        results.lock().expect("not poisoned")[i] = Some(r);
    });
    let mut hits = Vec::new();
    let mut total = 0;
    let mut searched = Vec::new();
    let mut keys = want_keys.then(HashSet::new);
    let mut seen: HashSet<(String, Vec<u8>)> = HashSet::new();
    for (g, r) in live.iter().zip(results.into_inner().expect("not poisoned")) {
        let found = r.expect("every group searched")?;
        searched.push(SearchedScan { scan: ScanRef::of(&g.scans[0]), place: g.place.clone(), entries: found.entries });
        total += found.total;
        for h in found.hits {
            // One path in two scans of the same disk (overlapping folders): the newer one.
            if seen.insert(volume_key(g, rel_of(&g.place.root, &h.path))) {
                hits.push(h);
            } else {
                total -= 1;
            }
        }
        if let Some(k) = keys.as_mut() {
            k.extend(found.keys);
        }
    }
    let truncated = total > hits.len() as u64;
    hits.truncate(q.limit);
    Ok(Current {
        pattern: q.pattern.clone(),
        store_path,
        store_exists: true,
        searched,
        hits,
        total,
        truncated,
        disk: disk.map(|d| d.display_name().to_string()).or_else(|| q.set.as_ref().map(|s| format!("set {s}"))),
        elapsed_ms: start.elapsed().as_millis() as u64,
        plan,
        keys,
    })
}

// ---------------------------------------------------------------- history

/// One op that matched, in pair `pair` (scans `pair + 1` → `pair` of a group).
struct Event {
    pair: usize,
    kind: OpKind,
    record: Record,
}

/// The matching ops of each path, by group and path below the root.
type Events = HashMap<(usize, Vec<u8>), Vec<Event>>;

/// What a pair of adjacent scans says about one name: the ops between them.
fn pair_ops(store: &Store, older: &SnapshotMeta, newer: &SnapshotMeta) -> Result<OpStream> {
    let err = |e: std::io::Error| AppError::Runtime(format!("comparing scans #{} and #{}: {e}", older.id, newer.id));
    if older.storage.base == Some(newer.id)
        && let Some((_, ops)) = store.delta_ops(older.id)?
    {
        return Ok(ops);
    }
    // A keyframe (or a delta against another scan): compare the two.
    let ops = delta::compute(store.records_as(newer.id, &older.root)?, store.records(older.id)?).map_err(err)?;
    Ok(Box::new(ops.into_iter().map(Ok)))
}

/// Why a path missing from `newer` may still be there.
fn uncertainty(older: &SnapshotMeta, newer: &SnapshotMeta, rel: &[u8]) -> Option<String> {
    let path = full_path(&newer.root, rel);
    let comps: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    for e in &newer.options.exclude {
        let hit = if e.starts_with('/') { path.starts_with(e.trim_end_matches('/').as_bytes()) } else { comps.contains(&e.as_bytes()) };
        if hit {
            return Some(format!("scan #{} excluded {e}", newer.id));
        }
    }
    if older.options.exclude != newer.options.exclude || older.options.stay_on_filesystem != newer.options.stay_on_filesystem {
        return Some(format!("scans #{} and #{} were made with different settings", older.id, newer.id));
    }
    for i in newer.issues() {
        let p = i.path.trim_end_matches('/').as_bytes();
        if path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/') {
            return Some(format!("scan #{} could not read {}", newer.id, i.path));
        }
    }
    let counts = newer.counts.as_ref()?;
    (counts.issue_count > newer.issues().len() as u64 && counts.inaccessible_dirs > 0)
        .then(|| format!("scan #{} could not read {} folders; it may be in one of them", newer.id, counts.inaccessible_dirs))
}

/// Reads the history of every disk and folder, newest pairs first, and says
/// when each matching path disappeared (and, for current matches with
/// `HistoryMode::Always`, when it appeared). `progress(done, total)`.
pub fn history(settings: &Effective, q: &Query, cur: &Current, with_current: bool, cancel: &AtomicBool, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<History> {
    let start = std::time::Instant::now();
    let matcher = Matcher::new(q)?;
    let Some(store) = open_store(settings)? else {
        return Ok(History::default());
    };
    let groups = &cur.plan.groups;
    // (group, pair) for every adjacent pair, newest pairs first across groups.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        pairs.extend((0..g.scans.len().saturating_sub(1)).map(|p| (gi, p)));
    }
    pairs.sort_by(|a, b| groups[b.0].scans[b.1].started_at.cmp(&groups[a.0].scans[a.1].started_at));
    // Covered groups: their newest scan is searched too; what is only there is gone.
    let covered: Vec<usize> = (0..groups.len()).filter(|&i| groups[i].covered_by.is_some()).collect();
    let total = pairs.len() + covered.len();
    let done = AtomicUsize::new(0);
    let kept = AtomicUsize::new(0);
    let too_many = AtomicBool::new(false);
    let events: Mutex<Events> = Mutex::new(HashMap::new());
    let failure: Mutex<Option<AppError>> = Mutex::new(None);
    progress(0, total);
    par_each(&pairs, |_, &(gi, p)| {
        if cancel.load(Ordering::Relaxed) || too_many.load(Ordering::Relaxed) {
            return;
        }
        let g = &groups[gi];
        let (newer, older) = (&g.scans[p], &g.scans[p + 1]);
        let mut buf = Vec::new();
        let mut mine: Vec<(Vec<u8>, Event)> = Vec::new();
        let mut run = || -> Result<()> {
            for (n, op) in pair_ops(&store, older, newer)?.enumerate() {
                let op = op.map_err(|e| AppError::Runtime(format!("reading scan #{}: {e}", older.id)))?;
                if n % 65536 == 0 && (cancel.load(Ordering::Relaxed) || too_many.load(Ordering::Relaxed)) {
                    return Ok(());
                }
                if op.record.depth == 0 {
                    continue;
                }
                let rel = || {
                    let mut r = rel_path(&op.parent);
                    if !r.is_empty() {
                        r.push(b'/');
                    }
                    r.extend_from_slice(&op.record.name);
                    r
                };
                let hit = if matcher.on_path() { matcher.is_match(&rel(), &mut buf) } else { matcher.is_match(&op.record.name, &mut buf) };
                if !hit || !(op.kind == OpKind::Remove || passes(q, &op.record)) {
                    continue;
                }
                let rel = rel();
                // A change only counts: its record is not kept (broad queries match many).
                let record = if op.kind == OpKind::Replace { Record { kind: op.record.kind, ..Default::default() } } else { op.record };
                mine.push((rel, Event { pair: p, kind: op.kind, record }));
                if kept.fetch_add(1, Ordering::Relaxed) + 1 > MAX_EVENTS {
                    too_many.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            failure.lock().expect("not poisoned").get_or_insert(e);
        }
        let mut ev = events.lock().expect("not poisoned");
        for (rel, e) in mine {
            ev.entry((gi, rel)).or_default().push(e);
        }
        drop(ev);
        progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
    });
    if let Some(e) = failure.into_inner().expect("not poisoned") {
        return Err(e);
    }
    let mut out = History { pairs: total, ..Default::default() };
    let events = events.into_inner().expect("not poisoned");
    // Names that appeared in each pair, for "probably moved".
    let mut appeared: HashMap<(usize, usize, Vec<u8>), Vec<u8>> = HashMap::new();
    for ((gi, rel), evs) in &events {
        for e in evs.iter().filter(|e| e.kind == OpKind::Remove) {
            appeared.entry((*gi, e.pair, e.record.name.clone())).or_insert_with(|| rel.clone());
        }
    }
    let current_keys: HashSet<(String, Vec<u8>)> = cur
        .hits
        .iter()
        .filter_map(|h| {
            let g = groups.iter().find(|g| g.place.root == h.place.root && g.place.disk == h.place.disk)?;
            Some(volume_key(g, rel_of(&g.place.root, &h.path)))
        })
        .collect();
    let alive_now = |key: &(String, Vec<u8>)| current_keys.contains(key) || cur.keys.as_ref().is_some_and(|k| k.contains(key));
    let mut gone: HashMap<(String, Vec<u8>), Lifetime> = HashMap::new();
    for ((gi, rel), mut evs) in events {
        let g = &groups[gi];
        evs.sort_by_key(|e| e.pair);
        let oldest = ScanRef::of(g.scans.last().expect("a group has scans"));
        let first = &evs[0];
        let mut life = Lifetime {
            path: NativePath(full_path(&g.scans[first.pair + 1].root, &rel)),
            kind: first.record.kind,
            size: first.record.alloc,
            mtime: first.record.mtime,
            place: g.place.clone(),
            last_seen: ScanRef::of(&g.scans[0]),
            gone_in: None,
            appeared_in: None,
            oldest,
            changes: 0,
            uncertain: None,
            moved_to: None,
        };
        let alive = first.kind != OpKind::Add;
        if !alive {
            let (older, newer) = (&g.scans[first.pair + 1], &g.scans[first.pair]);
            life.last_seen = ScanRef::of(older);
            life.gone_in = Some(ScanRef::of(newer));
            life.uncertain = uncertainty(older, newer, &rel);
            life.moved_to = appeared
                .get(&(gi, first.pair, first.record.name.clone()))
                .filter(|to| **to != rel)
                .map(|to| String::from_utf8_lossy(&full_path(&newer.root, to)).into_owned());
        }
        for e in evs.iter().skip(usize::from(!alive)) {
            match e.kind {
                OpKind::Replace => life.changes += u32::from(e.record.kind != Kind::Dir),
                OpKind::Remove => {
                    life.appeared_in = Some(ScanRef::of(&g.scans[e.pair]));
                    break;
                }
                OpKind::Add => break,
            }
        }
        let key = volume_key(g, &rel);
        if alive {
            if with_current && current_keys.contains(&key) {
                if let Some(h) = cur.hits.iter().find(|h| h.path.0 == full_path(&g.place.root, &rel)) {
                    life.kind = h.kind;
                    life.size = h.size;
                    life.mtime = h.mtime;
                    life.last_seen = h.scan.clone();
                }
                out.current.push(life);
            }
            continue;
        }
        if alive_now(&key) {
            continue;
        }
        match gone.get(&key) {
            Some(prev) if prev.last_seen.started_at >= life.last_seen.started_at => {}
            _ => {
                gone.insert(key, life);
            }
        }
    }
    // The newest scan of a covered folder: what a later scan of the enclosing folder does not have.
    for &gi in &covered {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let g = &groups[gi];
        let cover = &groups[g.covered_by.expect("covered")].scans[0];
        let unlimited = Query { limit: 100_000, ..q.clone() };
        let found = search_snapshot(&store, g, &g.scans[0], &unlimited, &matcher, false, cancel)?;
        for h in found.hits {
            let rel = rel_of(&g.place.root, &h.path).to_vec();
            let key = volume_key(g, &rel);
            if alive_now(&key) || gone.get(&key).is_some_and(|p| p.last_seen.started_at >= h.scan.started_at) {
                continue;
            }
            gone.insert(
                key,
                Lifetime {
                    path: h.path.clone(),
                    kind: h.kind,
                    size: h.size,
                    mtime: h.mtime,
                    place: g.place.clone(),
                    last_seen: h.scan.clone(),
                    gone_in: Some(ScanRef::of(cover)),
                    appeared_in: None,
                    oldest: ScanRef::of(g.scans.last().expect("a group has scans")),
                    changes: 0,
                    uncertain: Some(format!("not in scan #{} of {}, a larger folder", cover.id, cover.root)),
                    moved_to: None,
                },
            );
        }
        progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
    }
    let mut gone: Vec<Lifetime> = gone.into_values().collect();
    gone.sort_by(|a, b| {
        let at = |l: &Lifetime| l.gone_in.as_ref().map(|s| s.started_at.clone()).unwrap_or_default();
        at(b).cmp(&at(a)).then_with(|| a.path.cmp(&b.path))
    });
    out.gone_total = gone.len() as u64;
    gone.truncate(q.limit);
    out.gone = gone;
    out.current.sort_by(|a, b| a.path.cmp(&b.path));
    out.pairs_done = done.into_inner();
    out.cancelled = cancel.load(Ordering::Relaxed);
    out.too_many = too_many.into_inner();
    out.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(out)
}

/// `diskdrift find`: the newest scans, then the history when `mode` says so.
pub fn find(settings: &Effective, q: &Query, mode: HistoryMode, cancel: &AtomicBool, progress: &(dyn Fn(usize, usize) + Sync)) -> Result<FindResult> {
    let current = current_cancellable(settings, q, None, cancel)?;
    let run = match mode {
        HistoryMode::Never => false,
        HistoryMode::Always => true,
        HistoryMode::Auto => current.total == 0,
    };
    let history = if run && current.store_exists { Some(history(settings, q, &current, mode == HistoryMode::Always, cancel, progress)?) } else { None };
    Ok(FindResult { current, history })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str) -> Matcher {
        Matcher::new(&Query::new(pattern)).unwrap()
    }

    fn hit(pattern: &str, text: &str) -> bool {
        m(pattern).is_match(text.as_bytes(), &mut Vec::new())
    }

    #[test]
    fn folding_ignores_case_and_diacritics() {
        let fold = |s: &str| {
            let mut v = Vec::new();
            fold_into(s.as_bytes(), &mut v);
            String::from_utf8(v).unwrap()
        };
        assert_eq!(fold("Ważna Sprawa.TXT"), "wazna sprawa.txt");
        // NFD input (z + combining dot above) folds the same.
        assert_eq!(fold("Waz\u{0307}na"), "wazna");
        assert_eq!(fold("Łódź"), "lodz");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Ørsted"), "orsted");
        assert!(hit("wazna", "bardzo_ważna_sprawa.txt"));
        assert!(hit("WAŻNA", "wazna.txt"));
        assert!(!hit("wazna", "waznosc.txt"));
    }

    #[test]
    fn invalid_utf8_names_still_match_their_ascii() {
        assert!(m("abc").is_match(b"x\xffABC", &mut Vec::new()));
    }

    #[test]
    fn globs_are_whole_names() {
        assert!(hit("*.pdf", "Raport.PDF"));
        assert!(!hit("*.pdf", "raport.pdf.bak"));
        assert!(hit("raport-20??.*", "raport-2014.doc"));
        assert!(hit("[ab]*.txt", "b1.txt"));
        assert!(!hit("[!ab]*.txt", "b1.txt"));
        assert!(hit("a+b(1).txt", "a+b(1).txt"));
    }

    #[test]
    fn regexes_and_exact_matching() {
        assert!(hit("re:^wazna\\d+\\.txt$", "Ważna10.txt"));
        assert!(!hit("re:^wazna\\D", "wazna10"));
        assert!(Matcher::new(&Query::new("re:(")).is_err());
        let exact = Matcher::new(&Query { exact: true, ..Query::new("Ważna") }).unwrap();
        assert!(exact.is_match("Ważna.txt".as_bytes(), &mut Vec::new()));
        assert!(!exact.is_match("wazna.txt".as_bytes(), &mut Vec::new()));
    }

    #[test]
    fn a_slash_matches_the_path() {
        let p = m("docs/wazna");
        assert!(p.on_path());
        assert!(p.is_match(b"Users/a/Docs/Wazna.txt", &mut Vec::new()));
        let g = m("docs/*.txt");
        assert!(g.is_match(b"docs/a.txt", &mut Vec::new()));
        assert!(!g.is_match(b"docs/sub/a.txt", &mut Vec::new()));
        assert!(m("**/2014/*.pdf").is_match(b"a/b/2014/x.pdf", &mut Vec::new()));
    }

    mod store {
        use super::super::*;
        use crate::config::{Overrides, resolve_with};
        use diskdrift_core::testutil::TestDir;
        use diskdrift_core::walk::WalkReport;
        use diskdrift_storage::{ScanOptions, Status, VolumeIdentity, meta};

        pub struct Fixture {
            pub _t: TestDir,
            pub settings: Effective,
            pub store: Store,
        }

        pub fn fixture(name: &str) -> Fixture {
            let t = TestDir::new(name);
            let db = t.path().join("store");
            let store = Store::create(&db).unwrap();
            let settings = resolve_with(&Overrides { db: Some(db), ..Default::default() }, t.path(), t.path()).unwrap();
            Fixture { _t: t, settings, store }
        }

        /// A scan of `root` holding `files` (path below the root, allocated size).
        pub fn scan(f: &Fixture, root: &str, at: &str, exclude: &[&str], files: &[(&str, u64)]) -> u64 {
            let mut paths: Vec<(Vec<Vec<u8>>, Kind, u64)> = Vec::new();
            for (p, size) in files {
                let comps: Vec<Vec<u8>> = p.split('/').map(|c| c.as_bytes().to_vec()).collect();
                for d in 1..comps.len() {
                    if !paths.iter().any(|(c, _, _)| *c == comps[..d]) {
                        paths.push((comps[..d].to_vec(), Kind::Dir, 0));
                    }
                }
                paths.push((comps, Kind::File, *size));
            }
            paths.sort_by(|a, b| a.0.cmp(&b.0));
            // Two disks: X and Y.
            let (disk, mount) = if root.starts_with("/Volumes/Y") { ("d00000002", "/Volumes/Y") } else { ("d00000001", "/Volumes/X") };
            let template = SnapshotMeta {
                format_version: meta::META_FORMAT_VERSION,
                id: 0,
                name: format!("s-{at}"),
                root: root.into(),
                // Recorded in the registry by `begin`, from the volume UUID.
                disk: None,
                root_in_volume: Some(root.strip_prefix(mount).map(|r| if r.is_empty() { "/" } else { r }).unwrap().into()),
                status: Status::InProgress,
                started_at: at.into(),
                finished_at: None,
                volume: VolumeIdentity { uuid: Some(disk.into()), fs_type: "apfs".into(), mount_point: mount.into(), ..Default::default() },
                options: ScanOptions { stay_on_filesystem: true, exclude: exclude.iter().map(|e| e.to_string()).collect(), ..Default::default() },
                space_before: None,
                space_after: None,
                storage: Default::default(),
                counts: None,
                error: None,
                diskdrift_version: "test".into(),
            };
            let lock = f.store.lock().unwrap();
            let mut w = f.store.begin(lock, template, None).unwrap();
            w.push(&Record { depth: 0, name: root.as_bytes().to_vec(), kind: Kind::Dir, ..Default::default() }).unwrap();
            for (comps, kind, size) in &paths {
                let name = comps.last().unwrap().clone();
                w.push(&Record { depth: comps.len() as u16, name, kind: *kind, size: *size, alloc: *size, mtime: 1_700_000_000, ..Default::default() }).unwrap();
            }
            let (m, _) = w.commit_and_rebase(WalkReport::default(), None, at.into()).unwrap();
            m.id
        }

        fn run(f: &Fixture, pattern: &str, mode: HistoryMode) -> FindResult {
            find(&f.settings, &Query::new(pattern), mode, &AtomicBool::new(false), &|_, _| {}).unwrap()
        }

        fn deltas_history(name: &str, exclude_last: &[&str]) -> (Fixture, [u64; 4]) {
            let f = fixture(name);
            let r = "/Volumes/X";
            let s1 = scan(&f, r, "2026-10-01T10:00:00+02:00", &[], &[("Docs/a.txt", 1), ("Docs/plan.doc", 5)]);
            let s2 = scan(&f, r, "2026-10-02T10:00:00+02:00", &[], &[("Docs/a.txt", 1), ("Docs/plan.doc", 5), ("Docs/Ważna sprawa 10.txt", 12)]);
            let s3 = scan(&f, r, "2026-10-03T10:00:00+02:00", &[], &[("Docs/a.txt", 1), ("Docs/plan.doc", 5), ("Docs/Ważna sprawa 10.txt", 14)]);
            let s4 = scan(&f, r, "2026-10-04T10:00:00+02:00", exclude_last, &[("Archive/plan.doc", 5), ("Docs/a.txt", 1)]);
            (f, [s1, s2, s3, s4])
        }

        #[test]
        fn a_deleted_file_is_found_in_the_history() {
            let (f, [_, s2, s3, s4]) = deltas_history("search-history", &[]);
            assert!(f.store.get(s3).unwrap().storage.base.is_some(), "stored as a delta");
            let r = run(&f, "wazna", HistoryMode::Auto);
            assert_eq!(r.current.total, 0);
            assert_eq!(r.current.searched.len(), 1);
            let h = r.history.expect("searched the history by itself");
            assert_eq!((h.pairs, h.pairs_done, h.cancelled), (3, 3, false));
            assert_eq!(h.gone.len(), 1, "{:?}", h.gone);
            let g = &h.gone[0];
            assert_eq!(g.path.lossy(), "/Volumes/X/Docs/Ważna sprawa 10.txt");
            assert_eq!((g.last_seen.id, g.gone_in.as_ref().map(|s| s.id), g.appeared_in.as_ref().map(|s| s.id)), (s3, Some(s4), Some(s2)));
            assert_eq!((g.size, g.changes, g.uncertain.as_deref(), g.moved_to.as_deref()), (14, 1, None, None));
        }

        #[test]
        fn a_moved_file_is_current_and_probably_moved() {
            let (f, [_, _, _, s4]) = deltas_history("search-moved", &[]);
            let r = run(&f, "PLAN", HistoryMode::Auto);
            assert_eq!(r.current.total, 1);
            assert_eq!(r.current.hits[0].path.lossy(), "/Volumes/X/Archive/plan.doc");
            assert!(r.history.is_none(), "found now: no history unless asked");
            let h = run(&f, "plan", HistoryMode::Always).history.unwrap();
            assert_eq!(h.gone.len(), 1);
            assert_eq!(h.gone[0].moved_to.as_deref(), Some("/Volumes/X/Archive/plan.doc"));
            assert_eq!(h.current.len(), 1);
            assert_eq!(h.current[0].appeared_in.as_ref().map(|s| s.id), Some(s4));
            assert!(h.current[0].gone_in.is_none());
            // Never: no history at all.
            assert!(run(&f, "wazna", HistoryMode::Never).history.is_none());
        }

        #[test]
        fn a_pair_without_a_delta_is_compared_and_marked_uncertain() {
            let (f, [_, _, s3, s4]) = deltas_history("search-keyframe", &["node_modules"]);
            assert!(f.store.get(s3).unwrap().storage.base.is_none(), "other settings: kept full");
            let h = run(&f, "wazna", HistoryMode::Auto).history.unwrap();
            assert_eq!(h.gone.len(), 1);
            assert_eq!(h.gone[0].gone_in.as_ref().map(|s| s.id), Some(s4));
            assert!(h.gone[0].uncertain.as_deref().unwrap().contains("different settings"), "{:?}", h.gone[0].uncertain);
        }

        #[test]
        fn a_folder_scanned_before_the_whole_disk_is_history() {
            let f = fixture("search-covered");
            let old = scan(&f, "/Volumes/X/Docs", "2026-10-01T10:00:00+02:00", &[], &[("old.txt", 3), ("both.txt", 1)]);
            let all = scan(&f, "/Volumes/X", "2026-10-02T10:00:00+02:00", &[], &[("Docs/both.txt", 1), ("Music/x.mp3", 9)]);
            let r = run(&f, "both", HistoryMode::Auto);
            assert_eq!(r.current.searched.len(), 1, "the folder scan is covered by the newer disk scan");
            assert_eq!(r.current.hits.len(), 1);
            assert_eq!(r.current.hits[0].scan.id, all);
            let r = run(&f, "old.txt", HistoryMode::Auto);
            assert_eq!(r.current.total, 0);
            let h = r.history.unwrap();
            assert_eq!(h.gone.len(), 1, "{:?}", h.gone);
            assert_eq!((h.gone[0].last_seen.id, h.gone[0].gone_in.as_ref().map(|s| s.id)), (old, Some(all)));
            // A file in both is not reported as gone.
            assert!(run(&f, "both", HistoryMode::Always).history.unwrap().gone.is_empty());
        }

        #[test]
        fn a_set_limits_the_search_to_its_disks() {
            let f = fixture("search-set");
            scan(&f, "/Volumes/X", "2026-10-01T10:00:00+02:00", &[], &[("a/plan.doc", 1)]);
            scan(&f, "/Volumes/Y", "2026-10-02T10:00:00+02:00", &[], &[("b/plan.doc", 1)]);
            let y = f.store.list().unwrap().last().unwrap().disk.clone().unwrap();
            f.store.update_registry(|reg| reg.add_to_set(&y, "stare-dyski")).unwrap();
            let q = |set: Option<&str>| Query { set: set.map(String::from), ..Query::new("plan") };
            assert_eq!(current(&f.settings, &q(None), Some(&[])).unwrap().total, 2);
            let c = current(&f.settings, &q(Some("Stare-Dyski")), Some(&[])).unwrap();
            assert_eq!(c.hits.iter().map(|h| h.path.lossy()).collect::<Vec<_>>(), ["/Volumes/Y/b/plan.doc"]);
            assert_eq!(c.disk.as_deref(), Some("set Stare-Dyski"));
            assert!(matches!(current(&f.settings, &q(Some("none")), Some(&[])), Err(AppError::Usage(_))));
        }

        #[test]
        fn filters_and_the_limit() {
            let (f, _) = deltas_history("search-filters", &[]);
            let q = |kind, min_size, limit| Query { kind, min_size, limit, ..Query::new("a") };
            let c = current(&f.settings, &q(Some(Kind::Dir), 0, 100), Some(&[])).unwrap();
            assert_eq!(c.hits.iter().map(|h| h.path.lossy()).collect::<Vec<_>>(), ["/Volumes/X/Archive"]);
            let c = current(&f.settings, &q(None, 0, 1), Some(&[])).unwrap();
            assert_eq!((c.hits.len(), c.total, c.truncated), (1, 3, true), "Archive, plan.doc, a.txt");
            let c = current(&f.settings, &q(Some(Kind::File), 5, 100), Some(&[])).unwrap();
            assert_eq!(c.hits.len(), 1);
        }
    }

    #[test]
    fn empty_pattern_is_a_usage_error() {
        assert!(matches!(Matcher::new(&Query::new("")), Err(AppError::Usage(_))));
    }
}
