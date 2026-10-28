//! `~/.diskdrift/config.json` and effective-setting resolution (spec 3B).
//!
//! Precedence: CLI flag > config file > built-in default. Each effective value
//! remembers where it came from so `config show` can explain it.

use diskdrift_core::native::{NativePath, NativeRoot};
use diskdrift_core::units::SizeUnits;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const APP_DIR: &str = ".diskdrift";

fn native_path_value(value: serde_json::Value) -> Result<NativePath, String> {
    if let Some(s) = value.as_str() {
        return Ok(diskdrift_platform::native_path(std::ffi::OsStr::new(s)));
    }
    serde_json::from_value::<NativePath>(value).map_err(|e| e.to_string())
}

fn native_root_value(value: serde_json::Value) -> Result<NativeRoot, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}

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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StorageConfig {
    /// Snapshot store location. `~` is expanded; a relative path is resolved
    /// against the directory containing the config file.
    pub path: Option<String>,
    pub path_native: Option<NativePath>,
    /// Retention rules for `snapshots prune` and the TUI (0 = rule off; all
    /// off by default). Nothing is ever removed without a confirmation.
    pub keep_last: usize,
    pub keep_weekly: usize,
    pub keep_monthly: usize,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<Option<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    keep_last: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    keep_weekly: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    keep_monthly: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageWireFields {
    #[serde(default)]
    path: Option<serde_json::Value>,
    #[serde(default)]
    keep_last: usize,
    #[serde(default)]
    keep_weekly: usize,
    #[serde(default)]
    keep_monthly: usize,
}

impl<'de> Deserialize<'de> for StorageWire {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let present = value.as_object().is_some_and(|object| object.contains_key("path"));
        let fields = StorageWireFields::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            path: present.then_some(fields.path),
            keep_last: fields.keep_last,
            keep_weekly: fields.keep_weekly,
            keep_monthly: fields.keep_monthly,
        })
    }
}

impl Serialize for StorageConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let path = match (&self.path, &self.path_native) {
            (_, Some(path)) => Some(Some(serde_json::to_value(path).map_err(serde::ser::Error::custom)?)),
            (Some(path), None) => Some(Some(serde_json::Value::String(path.clone()))),
            (None, None) => None,
        };
        StorageWire { path, keep_last: self.keep_last, keep_weekly: self.keep_weekly, keep_monthly: self.keep_monthly }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StorageConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = StorageWire::deserialize(deserializer)?;
        let path_value = match wire.path {
            Some(Some(value)) => Some(value),
            Some(None) => return Err(serde::de::Error::custom("storage.path must not be null")),
            None => None,
        };
        let path_native = path_value.clone().filter(|v| !v.is_string()).map(native_path_value).transpose().map_err(serde::de::Error::custom)?;
        Ok(Self {
            path: path_native.as_ref().map(NativePath::display).or_else(|| path_value.as_ref().and_then(serde_json::Value::as_str).map(str::to_owned)),
            path_native,
            keep_last: wire.keep_last,
            keep_weekly: wire.keep_weekly,
            keep_monthly: wire.keep_monthly,
        })
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanConfig {
    pub default_root: String,
    pub default_root_native: NativeRoot,
    default_root_native_tagged: bool,
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

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScanWire {
    #[serde(default)] default_root: Option<serde_json::Value>,
    #[serde(default)] threads: Option<usize>,
    #[serde(default)] stay_on_filesystem: Option<bool>,
    #[serde(default)] follow_symlinks: Option<bool>,
    #[serde(default)] exclude: Option<Vec<String>>,
    #[serde(default)] auto_on_start: Option<bool>,
    #[serde(default)] auto_max_age_hours: Option<u64>,
    #[serde(default)] on_connect: Option<bool>,
}

impl Serialize for ScanConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let default_root = if self.default_root_native_tagged {
            serde_json::to_value(&self.default_root_native).map_err(serde::ser::Error::custom)?
        } else { serde_json::Value::String(self.default_root.clone()) };
        ScanWire { default_root: Some(default_root), threads: Some(self.threads), stay_on_filesystem: Some(self.stay_on_filesystem), follow_symlinks: Some(self.follow_symlinks), exclude: Some(self.exclude.clone()), auto_on_start: Some(self.auto_on_start), auto_max_age_hours: Some(self.auto_max_age_hours), on_connect: Some(self.on_connect) }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ScanConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ScanWire::deserialize(deserializer)?;
        let defaults = Self::default();
        let (default_root, default_root_native, default_root_native_tagged) = match wire.default_root {
            Some(value) if value.is_string() => {
                let display = value.as_str().expect("checked string").to_owned();
                let home = crate::config::home_dir().map_err(|e| serde::de::Error::custom(e.to_string()))?;
                let expanded = expand_path(&display, &home, &diskdrift_platform::system_volume_root());
                let native = diskdrift_platform::native_root(expanded.as_os_str()).map_err(serde::de::Error::custom)?;
                (display, native, false)
            }
            Some(value) => {
                let native = native_root_value(value).map_err(serde::de::Error::custom)?;
                (native.path.display(), native, true)
            }
            None => (defaults.default_root.clone(), defaults.default_root_native.clone(), false),
        };
        Ok(Self { default_root, default_root_native, default_root_native_tagged, threads: wire.threads.unwrap_or(defaults.threads), stay_on_filesystem: wire.stay_on_filesystem.unwrap_or(defaults.stay_on_filesystem), follow_symlinks: wire.follow_symlinks.unwrap_or(defaults.follow_symlinks), exclude: wire.exclude.unwrap_or(defaults.exclude), auto_on_start: wire.auto_on_start.unwrap_or(defaults.auto_on_start), auto_max_age_hours: wire.auto_max_age_hours.unwrap_or(defaults.auto_max_age_hours), on_connect: wire.on_connect.unwrap_or(defaults.on_connect) })
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        let root = diskdrift_platform::system_volume_root();
        let native_root = diskdrift_platform::native_root(root.as_os_str()).unwrap_or_else(|_| NativeRoot::posix(b"/".to_vec()));
        ScanConfig {
            default_root: root.display().to_string(),
            default_root_native: native_root,
            default_root_native_tagged: false,
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
    pub storage_path_native: Sourced<NativePath>,
    pub default_root: Sourced<PathBuf>,
    pub default_root_native: Sourced<NativeRoot>,
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

fn effective_native_path(path: &NativePath, display: &str, home: &Path, base: &Path) -> (NativePath, PathBuf) {
    if display == "~" || display.starts_with("~/") || !is_absolute_display(display) {
        let expanded = expand_path(display, home, base);
        return (diskdrift_platform::native_path(expanded.as_os_str()), expanded);
    }
    let compatible = diskdrift_platform::path_from_native(path).unwrap_or_else(|_| PathBuf::from(display));
    (path.clone(), compatible)
}

fn is_absolute_display(display: &str) -> bool {
    Path::new(display).is_absolute()
        || (display.len() >= 3
            && display.as_bytes()[0].is_ascii_alphabetic()
            && display.as_bytes()[1] == b':'
            && matches!(display.as_bytes()[2], b'/' | b'\\'))
        || display.starts_with(r"\\")
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
    if !cfg.scan.default_root_native_tagged {
        let root = if cfg.scan.default_root == "~" || cfg.scan.default_root.starts_with("~/") {
            expand_path(&cfg.scan.default_root, &home_dir()?, &diskdrift_platform::system_volume_root())
        } else {
            PathBuf::from(&cfg.scan.default_root)
        };
        diskdrift_platform::native_root(root.as_os_str()).map_err(|e| invalid("scan.defaultRoot", e.to_string()))?;
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| diskdrift_platform::system_volume_root());
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
    let system_root = diskdrift_platform::system_volume_root();
    let cfg_dir = config_path.value.parent().unwrap_or(system_root.as_path()).to_path_buf();
    let file = |present: bool| if present { Source::ConfigFile } else { Source::Default };
    let defaults = Config::default();
    let c = cfg.as_ref().unwrap_or(&defaults);

    let (storage_path_native, storage_path) = if let Some(db) = &ov.db {
        let display = db.to_string_lossy().into_owned();
        let path = diskdrift_platform::native_path(db.as_os_str());
        let (exact, expanded) = effective_native_path(&path, &display, home, cwd);
        (Sourced::new(exact, Source::CommandLine), Sourced::new(expanded, Source::CommandLine))
    } else if let Some(p) = cfg.as_ref().and_then(|c| c.storage.path.as_deref()) {
        let fallback = diskdrift_platform::native_path(std::ffi::OsStr::new(p));
        let exact = cfg.as_ref().and_then(|c| c.storage.path_native.as_ref()).unwrap_or(&fallback);
        let (exact, expanded) = effective_native_path(exact, p, home, &cfg_dir);
        (Sourced::new(exact, Source::ConfigFile), Sourced::new(expanded, Source::ConfigFile))
    } else {
        let path = app_dir.join("data");
        (Sourced::new(diskdrift_platform::native_path(path.as_os_str()), Source::Default), Sourced::new(path, Source::Default))
    };
    let root_source = file(exists && c.scan.default_root != defaults.scan.default_root);
    let (default_root_native, default_root) = {
        let (exact, path) = effective_native_path(&c.scan.default_root_native.path, &c.scan.default_root, home, &cfg_dir);
        let root = NativeRoot { path: exact, root_kind: c.scan.default_root_native.root_kind, verbatim: c.scan.default_root_native.verbatim };
        (Sourced::new(root, root_source), Sourced::new(path, root_source))
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
        default_root,
        default_root_native,
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
        storage_path_native,
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
        let cwd = std::env::current_dir().unwrap_or_else(|_| diskdrift_platform::system_volume_root());
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
            "storage.path" => {
                c.storage.path = (!v.is_empty()).then(|| v.to_string());
                c.storage.path_native = None;
            }
            "storage.keepLast" => c.storage.keep_last = number()?,
            "storage.keepWeekly" => c.storage.keep_weekly = number()?,
            "storage.keepMonthly" => c.storage.keep_monthly = number()?,
            "scan.defaultRoot" => {
                c.scan.default_root = v.to_string();
                c.scan.default_root_native_tagged = false;
            }
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
#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Creates the directory tree on targets without Unix permission APIs.
#[cfg(not(unix))]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).create(dir)
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
    fn create_private_dir_creates_nested_directory() {
        let t = TestDir::new("cfg-private-dir");
        let dir = t.path().join("one/two");

        create_private_dir(&dir).unwrap();

        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
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
    fn legacy_string_roots_expand_before_native_validation() {
        let t = TestDir::new("cfg-legacy-tilde-root");
        for root in ["~", "~/Projects"] {
            let cfg = write(&t, &format!(r#"{{"schemaVersion":1,"scan":{{"defaultRoot":"{root}"}}}}"#));
            let effective = resolve_with(
                &Overrides { config: Some(cfg), ..Default::default() },
                Path::new("/Users/x"),
                Path::new("/"),
            )
            .unwrap();
            assert_eq!(effective.default_root.value, expand_path(root, Path::new("/Users/x"), t.path()));
        }
    }

    #[test]
    #[cfg(unix)]
    fn legacy_string_roots_stay_strings_when_saved() {
        let t = TestDir::new("cfg-legacy-root-save");
        for root in ["/", "/tmp"] {
            let p = write(&t, &format!(r#"{{"schemaVersion":1,"scan":{{"defaultRoot":"{root}"}}}}"#));
            let cfg = load_file(&p).unwrap();
            save(&p, &cfg).unwrap();
            let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            assert_eq!(value["scan"]["defaultRoot"], root);
        }
    }

    #[test]
    fn legacy_string_roots_reject_empty_and_relative_values() {
        for root in ["", "relative"] {
            let err = parse(
                &format!(r#"{{"schemaVersion":1,"scan":{{"defaultRoot":"{root}"}}}}"#),
                Path::new("config.json"),
            )
            .unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{root:?}: {err:?}");
        }
    }

    #[test]
    fn storage_path_rejects_a_native_root_object() {
        let err = parse(
            r#"{"schemaVersion":1,"storage":{"path":{"path":"/tmp","pathEncoding":"unix_bytes","rootKind":"posix"}}}"#,
            Path::new("config.json"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn storage_path_rejects_explicit_null() {
        for path in ["null", "42"] {
            let err = parse(&format!(r#"{{"schemaVersion":1,"storage":{{"path":{path}}}}}"#), Path::new("config.json")).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{path}: {err:?}");
        }
    }

    #[test]
    fn posix_native_root_rejects_present_verbatim() {
        let err = parse(
            r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"/tmp","pathEncoding":"unix_bytes","rootKind":"posix","verbatim":false}}}"#,
            Path::new("config.json"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn scan_default_root_rejects_a_native_path_object() {
        let err = parse(
            r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"/tmp","pathEncoding":"unix_bytes"}}}"#,
            Path::new("config.json"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn tagged_native_config_schemas_reject_unknown_fields() {
        for (text, field) in [
            (r#"{"schemaVersion":1,"storage":{"path":{"path":"/tmp","pathEncoding":"unix_bytes","extra":true}}}"#, "storage.path"),
            (r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"/","pathEncoding":"unix_bytes","rootKind":"posix","extra":true}}}"#, "scan.defaultRoot"),
        ] {
            let err = parse(text, Path::new("config.json")).unwrap_err();
            assert!(matches!(&err, ConfigError::Invalid { .. }), "{field}: {err:?}");
        }
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
    fn exact_tagged_paths_are_accepted_in_config() {
        let cfg = parse(
            r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"/tmp/�","pathEncoding":"unix_bytes","pathBytes":"L3RtcC//","rootKind":"posix"}}}"#,
            Path::new("config.json"),
        )
        .unwrap();
        assert_eq!(cfg.scan.default_root, "/tmp/�");
    }

    #[test]
    #[cfg(unix)]
    fn invalid_unix_bytes_survive_config_load_and_save() {
        let t = TestDir::new("cfg-exact-unix");
        let p = write(&t, r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"/tmp/�","pathEncoding":"unix_bytes","pathBytes":"L3RtcC//","rootKind":"posix"}}}"#);
        let cfg = load_file(&p).unwrap();
        let effective = resolve_with(&Overrides { config: Some(p.clone()), ..Default::default() }, t.path(), Path::new("/")).unwrap();
        assert_eq!(effective.default_root_native.value.path, NativePath::unix(b"/tmp/\xff".to_vec()));
        assert_eq!(effective.default_root.value.as_os_str().as_encoded_bytes(), b"/tmp/\xff");
        save(&p, &cfg).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(value["scan"]["defaultRoot"]["pathBytes"], "L3RtcC//");
        assert_eq!(load_file(&p).unwrap(), cfg);
    }

    #[test]
    fn lone_surrogates_survive_windows_config_load_and_save() {
        let t = TestDir::new("cfg-exact-windows");
        let p = write(&t, r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"C:\\�","pathEncoding":"windows_utf16le","pathWide":"QwA6AFwAANg=","rootKind":"drive","verbatim":false}}}"#);
        let cfg = load_file(&p).unwrap();
        save(&p, &cfg).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(value["scan"]["defaultRoot"]["pathWide"], "QwA6AFwAANg=");
        assert_eq!(load_file(&p).unwrap(), cfg);
    }

    #[test]
    fn windows_config_roots_are_validated_without_posix_assumptions() {
        let cfg = parse(
            r#"{"schemaVersion":1,"scan":{"defaultRoot":{"path":"\\\\server\\share\\","pathEncoding":"windows_utf16le","pathWide":"XABcAHMAZQByAHYAZQByAFwAcwBoAGEAcgBlAFwA","rootKind":"unc","verbatim":false}}}"#,
            Path::new("config.json"),
        ).unwrap();
        assert_eq!(cfg.scan.default_root, r#"\\server\share\"#);
    }

    #[test]
    fn editing_storage_path_clears_stale_native_value() {
        let field = FIELDS.iter().find(|f| f.key == "storage.path").unwrap();
        let mut cfg = Config::default();
        cfg.storage.path = Some(r#"C:\old"#.into());
        cfg.storage.path_native = Some(NativePath::windows(r#"C:\old"#.encode_utf16().collect()));

        field.set(&mut cfg, r#"C:\new"#).unwrap();

        assert_eq!(cfg.storage.path.as_deref(), Some(r#"C:\new"#));
        assert_eq!(cfg.storage.path_native, None);
    }

    #[test]
    fn windows_drive_storage_paths_are_not_resolved_relative_to_config() {
        let t = TestDir::new("cfg-windows-drive-path");
        let p = write(&t, r#"{"schemaVersion":1,"storage":{"path":"C:\\store"}}"#);
        let effective = resolve_with(&Overrides { config: Some(p), ..Default::default() }, t.path(), Path::new("/cwd")).unwrap();

        assert_eq!(effective.storage_path.value, PathBuf::from(r#"C:\store"#));
    }

    #[test]
    fn default_root_comes_from_the_platform_adapter() {
        let root = diskdrift_platform::system_volume_root();
        let cfg = Config::default();

        assert_eq!(cfg.scan.default_root, root.display().to_string());
        assert_eq!(cfg.scan.default_root_native.path, diskdrift_platform::native_path(root.as_os_str()));
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
        #[cfg(unix)]
        assert_eq!(load_file(&p).unwrap().scan.default_root, "/");
    }

    #[test]
    fn init_writes_template_once() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let t = TestDir::new("cfg-init");
        let p = t.path().join("app/config.json");
        init(&p).unwrap();
        let cfg = parse(&std::fs::read_to_string(&p).unwrap(), &p).unwrap();
        assert_eq!(cfg, Config::template());
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(t.path().join("app")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        assert!(matches!(init(&p), Err(ConfigError::Exists(_))));
    }
}
