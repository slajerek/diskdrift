//! `~/.diskdrift/config.json` and effective-setting resolution (spec 3B).
//!
//! Precedence: CLI flag > config file > built-in default. Each effective value
//! remembers where it came from so `config show` can explain it.

use diskdrift_core::units::SizeUnits;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const APP_DIR: &str = ".diskdrift";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub output: OutputConfig,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageConfig {
    /// Snapshot store location. `~` is expanded; a relative path is resolved
    /// against the directory containing the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Retention rules for `snapshots prune` and the TUI (0 = rule off; all
    /// off by default). Nothing is ever removed without a confirmation.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keep_last: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keep_weekly: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keep_monthly: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ScanConfig {
    pub default_root: String,
    /// 0 = automatic.
    pub threads: usize,
    pub stay_on_filesystem: bool,
    pub follow_symlinks: bool,
    /// Absolute paths (`/Volumes/Backup`) or entry names (`node_modules`).
    pub exclude: Vec<String>,
    /// The TUI rescans the default root in the background at start when the
    /// latest scan of it is older than `autoMaxAgeHours` (never the first scan).
    pub auto_on_start: bool,
    pub auto_max_age_hours: u64,
    /// The TUI rescans a known disk when it is connected and its last scan is
    /// older than `autoMaxAgeHours` (never a disk seen for the first time).
    pub on_connect: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            default_root: "/".into(),
            threads: 0,
            stay_on_filesystem: true,
            follow_symlinks: false,
            exclude: Vec::new(),
            auto_on_start: true,
            auto_max_age_hours: 6,
            on_connect: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct UiConfig {
    pub confirm_before_scan: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { confirm_before_scan: true }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct OutputConfig {
    pub size_units: SizeUnits,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: SCHEMA_VERSION,
            storage: StorageConfig::default(),
            scan: ScanConfig::default(),
            ui: UiConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

impl Config {
    /// The template written by `config init`.
    pub fn template() -> Config {
        Config {
            storage: StorageConfig { path: Some(format!("~/{APP_DIR}/data")), ..Default::default() },
            ..Config::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine the home directory; set HOME or pass --config and --db explicitly")]
    NoHome,
    #[error("cannot read config file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid config file {path}: field `{field}`: {message}")]
    Invalid { path: PathBuf, field: String, message: String },
    #[error("cannot write config file {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("config file {0} already exists; not overwriting it")]
    Exists(PathBuf),
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Default,
    ConfigFile,
    CommandLine,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Default => "default",
            Source::ConfigFile => "config file",
            Source::CommandLine => "command line",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Sourced<T> {
    fn new(value: T, source: Source) -> Self {
        Sourced { value, source }
    }
}

/// Command-line overrides relevant to configuration.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub config: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub threads: Option<usize>,
    pub size_units: Option<SizeUnits>,
}

/// Effective settings shared by CLI and TUI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Effective {
    pub home: PathBuf,
    pub config_path: Sourced<PathBuf>,
    pub config_exists: bool,
    pub storage_path: Sourced<PathBuf>,
    pub default_root: Sourced<PathBuf>,
    pub threads: Sourced<usize>,
    pub stay_on_filesystem: Sourced<bool>,
    pub exclude: Sourced<Vec<String>>,
    pub confirm_before_scan: Sourced<bool>,
    pub size_units: Sourced<SizeUnits>,
    pub auto_scan: Sourced<bool>,
    pub auto_max_age_hours: Sourced<u64>,
    pub scan_on_connect: Sourced<bool>,
    pub retention: Sourced<crate::prune::Retention>,
}

pub fn home_dir() -> Result<PathBuf, ConfigError> {
    std::env::home_dir().filter(|h| h.is_absolute()).ok_or(ConfigError::NoHome)
}

/// Expands a leading `~` and resolves relative paths against `base`.
pub fn expand_path(p: &str, home: &Path, base: &Path) -> PathBuf {
    let expanded = if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() { expanded } else { base.join(expanded) }
}

/// Parses config JSON, naming the offending field on error.
pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
    let de = &mut serde_json::Deserializer::from_str(text);
    let cfg: Config = serde_path_to_error::deserialize(de).map_err(|e| {
        let field = e.path().to_string();
        ConfigError::Invalid {
            path: path.to_path_buf(),
            field: if matches!(field.as_str(), "." | "?" | "") { "(document)".into() } else { field },
            message: e.into_inner().to_string(),
        }
    })?;
    let invalid = |field: &str, message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        field: field.into(),
        message,
    };
    if cfg.schema_version != SCHEMA_VERSION {
        return Err(invalid(
            "schemaVersion",
            format!("unsupported version {} (this build supports {SCHEMA_VERSION})", cfg.schema_version),
        ));
    }
    if cfg.scan.follow_symlinks {
        return Err(invalid("scan.followSymlinks", "following symlinks is not supported in v0.1; set it to false".into()));
    }
    if !cfg.scan.default_root.starts_with('/') && !cfg.scan.default_root.starts_with('~') {
        return Err(invalid("scan.defaultRoot", "must be an absolute path".into()));
    }
    if cfg.storage.path.as_deref() == Some("") {
        return Err(invalid("storage.path", "must not be empty".into()));
    }
    Ok(cfg)
}

/// Loads the config (if present) and resolves effective settings. A missing config
/// file is normal; nothing is created.
pub fn resolve(ov: &Overrides) -> Result<Effective, ConfigError> {
    let home = home_dir()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    resolve_with(ov, &home, &cwd)
}

pub fn resolve_with(ov: &Overrides, home: &Path, cwd: &Path) -> Result<Effective, ConfigError> {
    let app_dir = home.join(APP_DIR);
    let config_path = match &ov.config {
        Some(p) => Sourced::new(expand_path(&p.to_string_lossy(), home, cwd), Source::CommandLine),
        None => Sourced::new(app_dir.join("config.json"), Source::Default),
    };
    let (cfg, exists) = match std::fs::read_to_string(&config_path.value) {
        Ok(text) => (Some(parse(&text, &config_path.value)?), true),
        Err(e) if e.kind() == io::ErrorKind::NotFound && config_path.source == Source::Default => (None, false),
        Err(source) => return Err(ConfigError::Read { path: config_path.value.clone(), source }),
    };
    let cfg_dir = config_path.value.parent().unwrap_or(Path::new("/")).to_path_buf();
    let file = |present: bool| if present { Source::ConfigFile } else { Source::Default };
    let defaults = Config::default();
    let c = cfg.as_ref().unwrap_or(&defaults);

    let storage_path = if let Some(db) = &ov.db {
        Sourced::new(expand_path(&db.to_string_lossy(), home, cwd), Source::CommandLine)
    } else if let Some(p) = cfg.as_ref().and_then(|c| c.storage.path.as_deref()) {
        Sourced::new(expand_path(p, home, &cfg_dir), Source::ConfigFile)
    } else {
        Sourced::new(app_dir.join("data"), Source::Default)
    };
    let threads = match ov.threads {
        Some(t) => Sourced::new(t, Source::CommandLine),
        None => Sourced::new(c.scan.threads, file(exists && c.scan.threads != defaults.scan.threads)),
    };
    let size_units = match ov.size_units {
        Some(u) => Sourced::new(u, Source::CommandLine),
        None => Sourced::new(c.output.size_units, file(exists && c.output.size_units != defaults.output.size_units)),
    };
    Ok(Effective {
        home: home.to_path_buf(),
        config_exists: exists,
        default_root: Sourced::new(
            expand_path(&c.scan.default_root, home, &cfg_dir),
            file(exists && c.scan.default_root != defaults.scan.default_root),
        ),
        stay_on_filesystem: Sourced::new(
            c.scan.stay_on_filesystem,
            file(exists && c.scan.stay_on_filesystem != defaults.scan.stay_on_filesystem),
        ),
        exclude: Sourced::new(c.scan.exclude.clone(), file(exists && !c.scan.exclude.is_empty())),
        confirm_before_scan: Sourced::new(
            c.ui.confirm_before_scan,
            file(exists && c.ui.confirm_before_scan != defaults.ui.confirm_before_scan),
        ),
        auto_scan: Sourced::new(c.scan.auto_on_start, file(exists && c.scan.auto_on_start != defaults.scan.auto_on_start)),
        auto_max_age_hours: Sourced::new(
            c.scan.auto_max_age_hours,
            file(exists && c.scan.auto_max_age_hours != defaults.scan.auto_max_age_hours),
        ),
        scan_on_connect: Sourced::new(c.scan.on_connect, file(exists && c.scan.on_connect != defaults.scan.on_connect)),
        retention: {
            let r = crate::prune::Retention { keep_last: c.storage.keep_last, keep_weekly: c.storage.keep_weekly, keep_monthly: c.storage.keep_monthly };
            Sourced::new(r, file(exists && !r.is_off()))
        },
        config_path,
        storage_path,
        threads,
        size_units,
    })
}

impl Effective {
    /// The command-line overrides these settings were resolved with, so they
    /// can be resolved again after the config file changes.
    pub fn overrides(&self) -> Overrides {
        let cli = |s: Source| s == Source::CommandLine;
        Overrides {
            config: cli(self.config_path.source).then(|| self.config_path.value.clone()),
            db: cli(self.storage_path.source).then(|| self.storage_path.value.clone()),
            threads: cli(self.threads.source).then_some(self.threads.value),
            size_units: cli(self.size_units.source).then_some(self.size_units.value),
        }
    }

    /// Resolves the settings again (after the config file was saved).
    pub fn reload(&self) -> Result<Effective, ConfigError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        resolve_with(&self.overrides(), &self.home, &cwd)
    }
}

/// The config file as written, or the defaults when there is none yet.
pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read { path: path.into(), source }),
    }
}

/// Writes `cfg` atomically (temporary file, fsync, rename) after checking it
/// passes the same validation as reading it would.
pub fn save(path: &Path, cfg: &Config) -> Result<(), ConfigError> {
    use std::io::Write;
    let mut text = serde_json::to_string_pretty(cfg).expect("serializable");
    text.push('\n');
    parse(&text, path)?;
    let write = |source| ConfigError::Write { path: path.into(), source };
    if let Some(dir) = path.parent() {
        create_private_dir(dir).map_err(write)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(write)?;
    f.write_all(text.as_bytes()).and_then(|()| f.sync_all()).map_err(write)?;
    std::fs::rename(&tmp, path).map_err(write)
}

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    /// One of a fixed list, cycled.
    Choice(&'static [&'static str]),
    Text,
    Number,
}

/// A setting editable from the TUI, with its config-file key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub help: &'static str,
}

pub const FIELDS: &[Field] = &[
    Field { key: "storage.path", label: "Snapshot store", kind: FieldKind::Text, help: "Where scans are saved. Empty: ~/.diskdrift/data." },
    Field {
        key: "storage.keepLast",
        label: "Keep: the last scans",
        kind: FieldKind::Number,
        help: "Retention (p in History, `snapshots prune`): keep the newest N scans of each disk and folder; 0 = rule off. Nothing is removed without asking.",
    },
    Field { key: "storage.keepWeekly", label: "…one a week for", kind: FieldKind::Number, help: "Weeks; the newest scan of each week is kept. 0 = rule off." },
    Field { key: "storage.keepMonthly", label: "…one a month for", kind: FieldKind::Number, help: "Months; the newest scan of each month is kept. 0 = rule off." },
    Field { key: "scan.defaultRoot", label: "Default scan root", kind: FieldKind::Text, help: "What a scan covers unless you type another root." },
    Field { key: "scan.exclude", label: "Exclude", kind: FieldKind::Text, help: "Comma-separated absolute paths or entry names (node_modules). New scans then compare only with scans that left out the same." },
    Field { key: "scan.stayOnFilesystem", label: "Stay on one filesystem", kind: FieldKind::Bool, help: "Do not descend into other mounted volumes. New scans then compare only with scans made the same way." },
    Field { key: "scan.threads", label: "Scan threads", kind: FieldKind::Number, help: "How many folders are read at once; 0 = automatic (one per core, 2 to 8)." },
    Field {
        key: "scan.autoOnStart",
        label: "Scan in background at start",
        kind: FieldKind::Bool,
        help: "Rescan the default root at low priority when the TUI starts and the latest scan of it is old (never the first scan). x stops it.",
    },
    Field { key: "scan.autoMaxAgeHours", label: "…if the last is older than", kind: FieldKind::Number, help: "Hours; 0 = at every start. Also for a disk you connect." },
    Field {
        key: "scan.onConnect",
        label: "Rescan a disk you connect",
        kind: FieldKind::Bool,
        help: "When a disk DiskDrift knows is connected while the TUI runs and its last scan is old, rescan it in the background (x stops it). Never a new disk.",
    },
    Field { key: "ui.confirmBeforeScan", label: "Confirm before a scan", kind: FieldKind::Bool, help: "Ask before starting a scan you request." },
    Field { key: "output.sizeUnits", label: "Size units", kind: FieldKind::Choice(&["decimal", "binary"]), help: "decimal: GB like Finder; binary: GiB." },
];

impl Field {
    /// The value as shown and edited.
    pub fn get(&self, c: &Config) -> String {
        match self.key {
            "storage.path" => c.storage.path.clone().unwrap_or_default(),
            "storage.keepLast" => c.storage.keep_last.to_string(),
            "storage.keepWeekly" => c.storage.keep_weekly.to_string(),
            "storage.keepMonthly" => c.storage.keep_monthly.to_string(),
            "scan.defaultRoot" => c.scan.default_root.clone(),
            "scan.exclude" => c.scan.exclude.join(", "),
            "scan.stayOnFilesystem" => c.scan.stay_on_filesystem.to_string(),
            "scan.threads" => c.scan.threads.to_string(),
            "scan.autoOnStart" => c.scan.auto_on_start.to_string(),
            "scan.autoMaxAgeHours" => c.scan.auto_max_age_hours.to_string(),
            "scan.onConnect" => c.scan.on_connect.to_string(),
            "ui.confirmBeforeScan" => c.ui.confirm_before_scan.to_string(),
            "output.sizeUnits" => match c.output.size_units {
                SizeUnits::Decimal => "decimal".into(),
                SizeUnits::Binary => "binary".into(),
            },
            _ => unreachable!("unknown field {}", self.key),
        }
    }

    /// Sets the value from its edited text.
    pub fn set(&self, c: &mut Config, v: &str) -> Result<(), String> {
        let v = v.trim();
        let flag = || match v {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("{}: expected true or false", self.key)),
        };
        let number = || v.parse::<usize>().map_err(|_| format!("{}: expected a whole number", self.key));
        match self.key {
            "storage.path" => c.storage.path = (!v.is_empty()).then(|| v.to_string()),
            "storage.keepLast" => c.storage.keep_last = number()?,
            "storage.keepWeekly" => c.storage.keep_weekly = number()?,
            "storage.keepMonthly" => c.storage.keep_monthly = number()?,
            "scan.defaultRoot" => c.scan.default_root = v.to_string(),
            "scan.exclude" => c.scan.exclude = v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect(),
            "scan.stayOnFilesystem" => c.scan.stay_on_filesystem = flag()?,
            "scan.threads" => c.scan.threads = number()?,
            "scan.autoOnStart" => c.scan.auto_on_start = flag()?,
            "scan.autoMaxAgeHours" => c.scan.auto_max_age_hours = number()? as u64,
            "scan.onConnect" => c.scan.on_connect = flag()?,
            "ui.confirmBeforeScan" => c.ui.confirm_before_scan = flag()?,
            "output.sizeUnits" => {
                c.output.size_units = match v {
                    "decimal" => SizeUnits::Decimal,
                    "binary" => SizeUnits::Binary,
                    _ => return Err(format!("{}: expected decimal or binary", self.key)),
                }
            }
            _ => unreachable!("unknown field {}", self.key),
        }
        Ok(())
    }

    /// The next value of a bool or choice field.
    pub fn next(&self, c: &Config) -> Option<String> {
        let cur = self.get(c);
        match self.kind {
            FieldKind::Bool => Some((cur != "true").to_string()),
            FieldKind::Choice(opts) => {
                let i = opts.iter().position(|o| *o == cur).unwrap_or(0);
                Some(opts[(i + 1) % opts.len()].to_string())
            }
            FieldKind::Text | FieldKind::Number => None,
        }
    }

    /// The value as read in the settings list: what an empty or zero value
    /// means, on/off for switches.
    pub fn display(&self, c: &Config) -> String {
        let v = self.get(c);
        match (self.key, v.as_str()) {
            ("storage.path", "") => "~/.diskdrift/data (default)".into(),
            ("storage.keepLast" | "storage.keepWeekly" | "storage.keepMonthly", "0") => "off".into(),
            ("storage.keepLast", n) => format!("{n} scans"),
            ("storage.keepWeekly", n) => format!("{n} weeks"),
            ("storage.keepMonthly", n) => format!("{n} months"),
            ("scan.exclude", "") => "(nothing)".into(),
            ("scan.threads", "0") => "automatic".into(),
            ("scan.autoMaxAgeHours", "0") => "0 (at every start)".into(),
            ("scan.autoMaxAgeHours", h) => format!("{h} hours"),
            ("output.sizeUnits", "decimal") => "decimal (GB, as Finder)".into(),
            ("output.sizeUnits", "binary") => "binary (GiB)".into(),
            (_, "true") => "on".into(),
            (_, "false") => "off".into(),
            _ => v,
        }
    }

    /// What this session uses instead, when a command-line flag overrides it.
    pub fn session_value(&self, e: &Effective) -> Option<String> {
        if !self.overridden(e) {
            return None;
        }
        Some(match self.key {
            "storage.path" => e.storage_path.value.display().to_string(),
            "scan.threads" => e.threads.value.to_string(),
            "output.sizeUnits" => match e.size_units.value {
                SizeUnits::Decimal => "decimal".into(),
                SizeUnits::Binary => "binary".into(),
            },
            _ => return None,
        })
    }

    /// Whether a command-line flag overrides this setting for the session.
    pub fn overridden(&self, e: &Effective) -> bool {
        let source = match self.key {
            "storage.path" => e.storage_path.source,
            "scan.threads" => e.threads.source,
            "output.sizeUnits" => e.size_units.source,
            _ => return false,
        };
        source == Source::CommandLine
    }
}

/// Creates the user-only directory `dir` (and parents) if missing.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// `config init`: writes the template without overwriting an existing file.
pub fn init(path: &Path) -> Result<(), ConfigError> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        create_private_dir(dir).map_err(|source| ConfigError::Write { path: path.into(), source })?;
    }
    let mut text = serde_json::to_string_pretty(&Config::template()).expect("serializable");
    text.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::AlreadyExists => ConfigError::Exists(path.into()),
            _ => ConfigError::Write { path: path.into(), source },
        })?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|source| ConfigError::Write { path: path.into(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::TestDir;

    fn write(t: &TestDir, text: &str) -> PathBuf {
        let p = t.path().join("config.json");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn defaults_without_config_create_nothing() {
        let t = TestDir::new("cfg-default");
        let e = resolve_with(&Overrides::default(), t.path(), Path::new("/")).unwrap();
        assert!(!e.config_exists);
        assert_eq!(e.storage_path.value, t.path().join(".diskdrift/data"));
        assert_eq!(e.storage_path.source, Source::Default);
        assert_eq!(e.size_units.value, SizeUnits::Decimal);
        assert!(!t.path().join(".diskdrift").exists(), "resolution must not create files");
    }

    #[test]
    fn precedence_cli_over_config_over_default() {
        let t = TestDir::new("cfg-precedence");
        let cfg = write(&t, r#"{"schemaVersion":1,"storage":{"path":"store"},"scan":{"threads":3},"output":{"sizeUnits":"binary"}}"#);
        let ov = Overrides { config: Some(cfg.clone()), ..Default::default() };
        let e = resolve_with(&ov, t.path(), Path::new("/cwd")).unwrap();
        assert_eq!(e.storage_path.value, t.path().join("store"), "relative to the config file");
        assert_eq!(e.storage_path.source, Source::ConfigFile);
        assert_eq!(e.threads.value, 3);
        assert_eq!(e.size_units.value, SizeUnits::Binary);

        let ov = Overrides { config: Some(cfg), db: Some("rel/db".into()), threads: Some(8), ..Default::default() };
        let e = resolve_with(&ov, t.path(), Path::new("/cwd")).unwrap();
        assert_eq!(e.storage_path.value, Path::new("/cwd/rel/db"), "CLI paths are relative to cwd");
        assert_eq!(e.storage_path.source, Source::CommandLine);
        assert_eq!(e.threads.value, 8);
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/Users/x");
        assert_eq!(expand_path("~/.diskdrift/data", home, Path::new("/")), Path::new("/Users/x/.diskdrift/data"));
        assert_eq!(expand_path("/abs", home, Path::new("/b")), Path::new("/abs"));
        assert_eq!(expand_path("~", home, Path::new("/")), home);
    }

    #[test]
    fn invalid_config_names_the_field() {
        let t = TestDir::new("cfg-invalid");
        for (text, field) in [
            (r#"{"schemaVersion":1,"scan":{"threads":"many"}}"#, "scan.threads"),
            (r#"{"schemaVersion":1,"scan":{"exclud":[]}}"#, "scan"),
            (r#"{"schemaVersion":2}"#, "schemaVersion"),
            (r#"{"schemaVersion":1,"scan":{"followSymlinks":true}}"#, "scan.followSymlinks"),
            (r#"{"schemaVersion":1,"output":{"sizeUnits":"metric"}}"#, "output.sizeUnits"),
            ("{not json", "(document)"),
        ] {
            let p = write(&t, text);
            let err = resolve_with(&Overrides { config: Some(p.clone()), ..Default::default() }, t.path(), Path::new("/"))
                .unwrap_err();
            match err {
                ConfigError::Invalid { field: f, path, .. } => {
                    assert!(f.starts_with(field), "{text}: got field {f}");
                    assert_eq!(path, p);
                }
                other => panic!("{text}: unexpected {other}"),
            }
        }
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let t = TestDir::new("cfg-missing");
        let ov = Overrides { config: Some(t.path().join("nope.json")), ..Default::default() };
        assert!(matches!(resolve_with(&ov, t.path(), Path::new("/")), Err(ConfigError::Read { .. })));
    }

    #[test]
    fn fields_edit_save_and_reload() {
        let t = TestDir::new("cfg-fields");
        let p = t.path().join("app/config.json");
        let ov = Overrides { config: Some(p.clone()), ..Default::default() };
        let mut c = load_file(&p).unwrap();
        assert_eq!(c, Config::default(), "no file yet: defaults");
        for f in FIELDS {
            let v = f.get(&c);
            f.set(&mut c, &v).unwrap_or_else(|e| panic!("{}: {e}", f.key));
        }
        assert_eq!(c, Config::default(), "every field round-trips");
        let field = |k: &str| FIELDS.iter().find(|f| f.key == k).unwrap();
        field("scan.exclude").set(&mut c, "node_modules, /Volumes/Backup,").unwrap();
        let units = field("output.sizeUnits");
        let next = units.next(&c).unwrap();
        units.set(&mut c, &next).unwrap();
        assert!(field("scan.threads").set(&mut c, "many").is_err());
        save(&p, &c).unwrap();
        assert_eq!(load_file(&p).unwrap().scan.exclude, ["node_modules", "/Volumes/Backup"]);

        let e = resolve_with(&ov, t.path(), Path::new("/")).unwrap();
        assert_eq!(e.size_units.value, SizeUnits::Binary);
        let e2 = Effective { size_units: Sourced::new(SizeUnits::Decimal, Source::CommandLine), ..e };
        assert!(units.overridden(&e2));
        assert_eq!(units.display(&c), "binary (GiB)");
        assert_eq!(units.session_value(&e2).as_deref(), Some("decimal"));
        assert_eq!(field("scan.stayOnFilesystem").display(&c), "on");
        assert_eq!(field("scan.threads").display(&c), "automatic");
        assert_eq!(field("storage.path").display(&c), "~/.diskdrift/data (default)");
        assert_eq!(field("storage.path").session_value(&e2), None, "no flag for the store");
        assert_eq!(e2.reload().unwrap().size_units.value, SizeUnits::Decimal, "command line still wins");

        field("scan.defaultRoot").set(&mut c, "relative").unwrap();
        assert!(matches!(save(&p, &c), Err(ConfigError::Invalid { .. })), "invalid values are not written");
        assert_eq!(load_file(&p).unwrap().scan.default_root, "/");
    }

    #[test]
    fn init_writes_template_once() {
        use std::os::unix::fs::PermissionsExt;
        let t = TestDir::new("cfg-init");
        let p = t.path().join("app/config.json");
        init(&p).unwrap();
        let cfg = parse(&std::fs::read_to_string(&p).unwrap(), &p).unwrap();
        assert_eq!(cfg, Config::template());
        let mode = std::fs::metadata(t.path().join("app")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(matches!(init(&p), Err(ConfigError::Exists(_))));
    }
}
