//! What a folder or file most likely is, and how much it matters: known app
//! folders, common cache/build/backup names, and the location it sits in.
//! Assessments are heuristics, labelled as such; they never claim certainty.

use crate::apps::InstalledApps;
use crate::suspects::Risk;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Where a folder sits, which decides how its children are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    AppSupport,
    Containers,
    GroupContainers,
    Caches,
    Logs,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assessment {
    /// Short description ("Chrome profiles", "cache", ...).
    pub what: String,
    /// `None`: nothing known about it.
    pub risk: Option<Risk>,
    pub advice: Option<String>,
    /// Installed app the folder belongs to, if one matches.
    pub owner: Option<String>,
    /// An app-data folder with no matching installed app: possibly left over from
    /// an uninstalled app, or owned by a command-line tool.
    pub no_installed_owner: bool,
}

/// A short verdict for lists.
pub fn verdict(risk: Option<Risk>) -> &'static str {
    match risk {
        Some(Risk::Low) => "regenerable",
        Some(Risk::Medium) => "review",
        Some(Risk::High) => "keep/user data",
        Some(Risk::SystemManaged) => "system",
        None => "unknown",
    }
}

struct Known {
    /// Lowercase folder name; a trailing `*` matches a prefix.
    name: &'static str,
    at: &'static [Location],
    what: &'static str,
    risk: Risk,
    advice: &'static str,
    /// Bundle id of the owning app ("" = match by folder name, "-" = not an
    /// app's folder).
    app: &'static str,
}

use Location::*;

macro_rules! k {
    ($name:literal, $at:expr, $what:literal, $risk:ident, $advice:literal) => {
        k!($name, $at, $what, $risk, $advice, "")
    };
    ($name:literal, $at:expr, $what:literal, $risk:ident, $advice:literal, $app:literal) => {
        Known { name: $name, at: $at, what: $what, risk: Risk::$risk, advice: $advice, app: $app }
    };
}

/// Folders of specific apps, matched by name in app-data locations.
const KNOWN_APPS: &[Known] = &[
    k!("mobilesync", &[AppSupport], "iPhone/iPad backups", High,
       "Device backups. Manage them in Finder (select the device > Manage Backups); delete only backups you no longer need.", "-"),
    k!("google", &[AppSupport], "Google apps (Chrome profiles)", High,
       "Browser profiles: history, passwords, extensions, site data. Inside, 'Service Worker', 'File System' and 'IndexedDB' are site data; clear them in Chrome > Settings > Privacy > Delete browsing data.", "com.google.Chrome"),
    k!("firefox", &[AppSupport], "Firefox profiles", High,
       "Browser profiles: history, passwords, site data. Clear site data in Firefox settings.", "org.mozilla.firefox"),
    k!("bravesoftware", &[AppSupport], "Brave profiles", High,
       "Browser profiles; clear site data in the browser's settings.", "com.brave.Browser"),
    k!("microsoft edge", &[AppSupport], "Edge profiles", High,
       "Browser profiles; clear site data in the browser's settings.", "com.microsoft.edgemac"),
    k!("arc", &[AppSupport], "Arc profiles", High,
       "Browser profiles; clear site data in the browser's settings.", "company.thebrowser.Browser"),
    k!("slack", &[AppSupport], "Slack data", Medium,
       "Mostly caches and offline copies; messages live on the server. Slack > Help > Troubleshooting > Clear Cache.", "com.tinyspeck.slackmacgap"),
    k!("discord", &[AppSupport], "Discord data", Medium,
       "Mostly caches ('Cache', 'Code Cache'); regenerated after sign-in.", "com.hnc.Discord"),
    k!("microsoft teams*", &[AppSupport], "Teams data", Medium,
       "Mostly caches; messages live on the server."),
    k!("code", &[AppSupport], "VS Code data", Medium,
       "'CachedData', 'CachedExtensionVSIXs', 'logs' and 'Cache' regenerate; 'User' holds settings and workspace state.", "com.microsoft.VSCode"),
    k!("cursor", &[AppSupport], "Cursor data", Medium,
       "'CachedData', 'logs' and caches regenerate; 'User' holds settings and workspace state.", "com.todesktop.230313mzl4w4u92"),
    k!("jetbrains", &[AppSupport], "JetBrains IDE settings", Medium,
       "One folder per IDE version; folders of versions you no longer use can go (IDE: Help > Delete Leftover IDE Directories)."),
    k!("steam", &[AppSupport], "Steam game library", Medium,
       "Installed games in 'steamapps' can be re-downloaded; uninstall games inside Steam. Save games may be here too.", "com.valvesoftware.steam"),
    k!("epic", &[AppSupport], "Epic Games data", Medium,
       "Launcher data; games are uninstalled inside the launcher."),
    k!("battle.net", &[AppSupport], "Battle.net data", Medium,
       "Launcher data; games are uninstalled inside the launcher."),
    k!("minecraft", &[AppSupport], "Minecraft worlds and versions", High,
       "Contains your saved worlds ('saves'); 'versions' and 'assets' are re-downloadable."),
    k!("spotify", &[AppSupport], "Spotify offline cache", Low,
       "Streaming and offline cache ('PersistentCache'); set a cache size limit in Spotify settings.", "com.spotify.client"),
    k!("telegram desktop", &[AppSupport], "Telegram media cache", Medium,
       "Downloaded media; chats live on the server. Settings > Data and Storage > Storage Usage."),
    k!("whatsapp", &[AppSupport], "WhatsApp data", High,
       "Message history and media.", "net.whatsapp.WhatsApp"),
    k!("zoom.us", &[AppSupport], "Zoom data", Medium,
       "App data; local recordings are usually in ~/Documents/Zoom.", "us.zoom.xos"),
    k!("logic", &[AppSupport], "Logic Pro sound library", Medium,
       "Instrument and loop content; manage it in Logic Pro > Sound Library > Manage.", "com.apple.logic10"),
    k!("garageband", &[AppSupport], "GarageBand sound library", Medium,
       "Instrument and loop content; re-downloadable from the app.", "com.apple.garageband10"),
    k!("lm studio", &[AppSupport], "LM Studio models", Medium,
       "Downloaded model weights; manage them in LM Studio's 'My Models'."),
    k!("claude", &[AppSupport], "Claude desktop data", Medium,
       "App data; large 'vm_bundles' are runtime images the app downloads again when needed.", "com.anthropic.claudefordesktop"),
    k!("docker", &[AppSupport], "Docker data", Medium,
       "Docker Desktop state; use `docker system df` and prune what you do not need.", "com.docker.docker"),
    k!("adobe", &[AppSupport], "Adobe app data", Medium,
       "Settings, fonts and content of Adobe apps; caches inside ('*Cache*') regenerate."),
    k!("rambox", &[AppSupport], "Rambox web sessions", Medium,
       "Each added service keeps its own browser data ('Partitions'); caches inside regenerate, chats live on the services' servers.", "com.grupovrs.ramboxce"),
    k!("em client", &[AppSupport], "eM Client mail store", High,
       "Local copy of your mail, calendars and attachments; manage it inside eM Client.", "com.emclient.mail.client"),
    k!("vivaldi", &[AppSupport], "Vivaldi profiles", High,
       "Browser profiles; clear site data in the browser's settings.", "com.vivaldi.Vivaldi"),
    k!("opera software", &[AppSupport], "Opera profiles", High,
       "Browser profiles; clear site data in the browser's settings.", "com.operasoftware.Opera"),
    k!("crashreporter", &[AppSupport], "crash reports", Low,
       "Crash reports sent or waiting to be sent.", "-"),
    k!("addressbook", &[AppSupport], "Contacts database", High,
       "Your contacts. Managed by the Contacts app.", "-"),
    k!("clouddocs", &[AppSupport], "iCloud Drive state", SystemManaged,
       "Managed by macOS. Free space with iCloud Drive's 'Optimize Mac Storage' instead.", "-"),
    k!("fileprovider", &[AppSupport], "cloud-file provider state", SystemManaged,
       "Managed by macOS for iCloud/Dropbox/OneDrive-style providers.", "-"),
    k!("knowledge", &[AppSupport], "macOS usage database", SystemManaged,
       "Managed by macOS.", "-"),
    k!("com.docker.docker", &[Containers], "Docker Desktop VM", Medium,
       "Sparse virtual disk. Check `docker system df`, prune, or shrink the disk in Docker Desktop settings."),
    k!("com.utmapp.utm", &[Containers], "UTM virtual machines", High,
       "Virtual machine images."),
    k!("com.apple.mail", &[Containers], "Mail storage", High,
       "Mail and attachments; manage inside Mail.", "-"),
    k!("net.whatsapp.whatsapp", &[Containers, GroupContainers], "WhatsApp data", High,
       "Message history and media."),
    k!("ru.keepcoder.telegram", &[Containers, GroupContainers], "Telegram media cache", Medium,
       "Downloaded media; chats live on the server. Settings > Data and Storage > Storage Usage."),
    k!("6n38vws5bx.ru.keepcoder.telegram", &[GroupContainers], "Telegram media cache", Medium,
       "Downloaded media; chats live on the server. Settings > Data and Storage > Storage Usage."),
    k!("com.tinyspeck.slackmacgap", &[Containers], "Slack data", Medium,
       "Mostly caches; messages live on the server."),
    k!("ubf8t346g9.office", &[GroupContainers], "Microsoft Office shared data", Medium,
       "Office caches and offline files; OneDrive files are managed in OneDrive."),
    k!("group.com.apple.corespeech", &[GroupContainers], "Siri speech models", SystemManaged,
       "Managed by macOS.", "-"),
];

/// Names that mean the same thing wherever they appear.
const GENERIC: &[(&str, &str, Risk, &str)] = &[
    ("cache", "cache", Risk::Low, "Regenerated by the app that owns it."),
    ("caches", "cache", Risk::Low, "Regenerated by the app that owns it."),
    ("code cache", "script cache", Risk::Low, "Compiled-script cache of an Electron/Chromium app; regenerated."),
    ("gpucache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("shadercache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("grshadercache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("dawncache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("dawngraphitecache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("dawnwebgpucache", "GPU cache", Risk::Low, "Shader cache; regenerated."),
    ("cachestorage", "web cache", Risk::Low, "Cache of web content; regenerated."),
    ("cacheddata", "cache", Risk::Low, "Regenerated by the app."),
    ("cachedextensionvsixs", "extension downloads", Risk::Low, "Downloaded extension packages; re-downloaded when needed."),
    ("component_crx_cache", "component downloads", Risk::Low, "Downloaded browser components; regenerated."),
    ("service worker", "web-app data", Risk::Medium, "Offline data of websites and web apps; cleared with the browser's site data."),
    ("indexeddb", "web-app data", Risk::Medium, "Data stored by websites; cleared with the browser's site data."),
    ("local storage", "web-app data", Risk::Medium, "Data stored by websites; cleared with the browser's site data."),
    ("file system", "web-app data", Risk::Medium, "Files stored by websites; cleared with the browser's site data."),
    ("crashpad", "crash reports", Risk::Low, "Crash dumps; safe to clear."),
    ("crash reports", "crash reports", Risk::Low, "Crash dumps; safe to clear."),
    ("logs", "logs", Risk::Low, "Log files; review large ones."),
    ("log", "logs", Risk::Low, "Log files; review large ones."),
    ("tmp", "temporary files", Risk::Low, "Temporary files; remove with the owning app closed."),
    ("temp", "temporary files", Risk::Low, "Temporary files; remove with the owning app closed."),
    ("deriveddata", "build intermediates", Risk::Low, "Regenerated by the next build."),
    ("node_modules", "JavaScript dependencies", Risk::Low, "Recreated by `npm install`."),
    ("__pycache__", "Python bytecode", Risk::Low, "Regenerated automatically."),
    (".venv", "Python environment", Risk::Low, "Recreatable from the project's requirements."),
    ("venv", "Python environment", Risk::Low, "Recreatable from the project's requirements."),
    ("pods", "CocoaPods dependencies", Risk::Low, "Recreated by `pod install`."),
    (".gradle", "Gradle cache", Risk::Low, "Regenerated by the next build."),
    ("backup", "backups", Risk::High, "Backups: keep unless you have another copy."),
    ("backups", "backups", Risk::High, "Backups: keep unless you have another copy."),
    ("models", "downloaded models", Risk::Medium, "Model weights; re-downloadable, but large to fetch again."),
    ("steamapps", "installed games", Risk::Medium, "Games can be re-downloaded; uninstall them in Steam."),
    ("vm_bundles", "VM images", Risk::Medium, "Virtual machine images."),
    ("vms", "VM images", Risk::Medium, "Virtual machine images."),
    ("partitions", "web sessions", Risk::Medium, "Browser data of an Electron app's embedded sessions; caches inside regenerate."),
    ("steam.appbundle", "Steam client files", Risk::Low, "The Steam app itself; Steam downloads it again."),
    ("appcache", "app cache", Risk::Low, "Regenerated by the app."),
    ("depotcache", "download cache", Risk::Low, "Steam download manifests; regenerated."),
    ("userdata", "user settings and saves", Risk::High, "Per-user settings and save data; keep."),
    ("widevinecdm", "DRM module", Risk::Low, "Video DRM component; downloaded again when needed."),
    ("extensions", "browser/app extensions", Risk::Medium, "Installed extensions; remove unused ones inside the app."),
    ("saved application state", "window state", Risk::Low, "Restored-window state; recreated."),
];

/// File types, by extension.
const EXTENSIONS: &[(&str, &str, Risk, &str)] = &[
    ("dmg", "disk image / installer", Risk::Low, "Installer images are usually not needed after installing."),
    ("pkg", "installer package", Risk::Low, "Usually not needed after installing."),
    ("ipsw", "iOS firmware", Risk::Low, "Downloaded again when needed."),
    ("iso", "disk image", Risk::Medium, "Disk image; keep if you still need it."),
    ("xip", "Xcode archive", Risk::Low, "Xcode installer archive; not needed after extracting."),
    ("zip", "archive", Risk::Medium, "Archive; check whether its contents exist elsewhere."),
    ("log", "log file", Risk::Low, "Log file."),
    ("vmdk", "VM disk", Risk::High, "Virtual machine disk."),
    ("qcow2", "VM disk", Risk::High, "Virtual machine disk."),
    ("vdi", "VM disk", Risk::High, "Virtual machine disk."),
    ("sparsebundle", "disk image", Risk::High, "Disk image (possibly a backup)."),
    ("sparseimage", "disk image", Risk::High, "Disk image."),
    ("photoslibrary", "Photos library", Risk::High, "Photo library; use Photos' 'Optimize Mac Storage'."),
    ("xcarchive", "Xcode archive", Risk::High, "Archived app build; needed to symbolicate crash reports."),
    ("pvm", "Parallels virtual machine", Risk::High, "A whole VM; remove it in Parallels Control Center, or use 'Reclaim disk space' to shrink it."),
    ("hdd", "VM disk", Risk::High, "Virtual machine disk; shrink it from the VM's own settings."),
    ("utm", "UTM virtual machine", Risk::High, "A whole VM; remove it in UTM."),
    ("vmwarevm", "VMware virtual machine", Risk::High, "A whole VM; remove it in VMware."),
    ("gguf", "model weights", Risk::Medium, "Local model file; re-downloadable."),
    ("safetensors", "model weights", Risk::Medium, "Local model file; re-downloadable."),
    ("mov", "video", Risk::High, "Your video; review manually."),
    ("mp4", "video", Risk::High, "Video; review manually."),
];

pub struct Classifier {
    home: PathBuf,
    apps: InstalledApps,
}

impl Classifier {
    pub fn new(home: &Path, apps: InstalledApps) -> Classifier {
        Classifier { home: home.to_path_buf(), apps }
    }

    pub fn installed_apps(&self) -> usize {
        self.apps.len()
    }

    /// Loads the installed-app index from the standard locations.
    pub fn load(home: &Path) -> Classifier {
        Self::new(home, InstalledApps::load(home))
    }

    fn location(&self, dir: &Path) -> Location {
        let lib = self.home.join("Library");
        match dir.strip_prefix(&lib).ok().and_then(|r| r.to_str()) {
            Some("Application Support") => AppSupport,
            Some("Containers") => Containers,
            Some("Group Containers") => GroupContainers,
            Some("Caches") => Caches,
            Some("Logs") => Logs,
            _ if dir == Path::new("/Library/Caches") => Caches,
            _ if dir == Path::new("/Library/Logs") => Logs,
            _ if dir == Path::new("/Library/Application Support") => AppSupport,
            _ => Other,
        }
    }

    /// Assesses `path` (a direct child of the folder being analysed).
    pub fn assess(&self, path: &Path, is_dir: bool) -> Assessment {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lower = name.to_lowercase();
        let loc = path.parent().map(|p| self.location(p)).unwrap_or(Other);
        let apple = lower.starts_with("com.apple.") || lower.starts_with("group.com.apple.");
        let app_location = matches!(loc, AppSupport | Containers | GroupContainers | Caches);
        let mut a = Assessment { what: String::new(), risk: None, advice: None, owner: None, no_installed_owner: false };
        // Whose folder it is: only for direct children of app-data locations.
        let set_owner = |a: &mut Assessment, app_id: &str| {
            if !app_location || apple || app_id == "-" {
                return;
            }
            let by_id = (!app_id.is_empty()).then(|| self.apps.owner_of(app_id)).flatten();
            a.owner = by_id.or_else(|| self.apps.owner_of(&name)).map(|x| x.name.clone());
            a.no_installed_owner = a.owner.is_none() && !self.apps.is_empty();
        };

        let known = KNOWN_APPS.iter().find(|k| {
            k.at.contains(&loc)
                && match k.name.strip_suffix('*') {
                    Some(prefix) => lower.starts_with(prefix),
                    None => lower == k.name,
                }
        });
        if let Some(k) = known {
            a.what = k.what.into();
            a.risk = Some(k.risk);
            a.advice = Some(k.advice.into());
            set_owner(&mut a, k.app);
            return a;
        }
        if is_dir && let Some((_, what, risk, advice)) = GENERIC.iter().find(|(n, ..)| *n == lower) {
            a.what = (*what).into();
            a.risk = Some(*risk);
            a.advice = Some((*advice).into());
            return a;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
        if let Some((_, what, risk, advice)) = ext.and_then(|e| EXTENSIONS.iter().find(|(x, ..)| *x == e)) {
            a.what = (*what).into();
            a.risk = Some(*risk);
            a.advice = Some((*advice).into());
            return a;
        }
        set_owner(&mut a, "");
        let whose = a.owner.clone().unwrap_or_else(|| name.clone());
        match loc {
            Caches => {
                a.what = format!("cache of {whose}");
                a.risk = Some(if apple { Risk::SystemManaged } else { Risk::Low });
                a.advice = Some(if apple {
                    "Cache of a macOS component; macOS clears it when needed.".into()
                } else {
                    "Apps rebuild their caches; remove with the app closed, or use the app's own setting.".into()
                });
            }
            Logs => {
                a.what = "logs".into();
                a.risk = Some(Risk::Low);
                a.advice = Some("Log files; review large ones.".into());
            }
            AppSupport | Containers | GroupContainers if apple => {
                a.what = "data of a macOS component".into();
                a.risk = Some(Risk::SystemManaged);
                a.advice = Some("Managed by macOS; manage it in the owning Apple app or System Settings > General > Storage.".into());
            }
            AppSupport | Containers | GroupContainers => {
                a.what = format!("app data of {whose}");
                a.risk = Some(Risk::Medium);
                a.advice = Some("Open it to see what is inside; clean up from within the app.".into());
            }
            Other => {}
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::App;

    fn classifier() -> Classifier {
        let apps = InstalledApps::from_apps(vec![App {
            name: "Slack".into(),
            bundle_id: Some("com.tinyspeck.slackmacgap".into()),
            path: "/Applications/Slack.app".into(),
        }]);
        Classifier::new(Path::new("/Users/u"), apps)
    }

    #[test]
    fn known_generic_location_and_owner() {
        let c = classifier();
        let a = c.assess(Path::new("/Users/u/Library/Application Support/MobileSync"), true);
        assert_eq!(a.risk, Some(Risk::High));
        let a = c.assess(Path::new("/Users/u/Library/Application Support/Slack"), true);
        assert_eq!(a.owner.as_deref(), Some("Slack"));
        assert!(!a.no_installed_owner);
        let a = c.assess(Path::new("/Users/u/Library/Application Support/OldTool"), true);
        assert!(a.no_installed_owner);
        assert_eq!(a.risk, Some(Risk::Medium));
        let a = c.assess(Path::new("/Users/u/Library/Application Support/com.apple.sharedfilelist"), true);
        assert_eq!(a.risk, Some(Risk::SystemManaged));
        assert!(!a.no_installed_owner);
        let a = c.assess(Path::new("/Users/u/Library/Caches/com.tinyspeck.slackmacgap"), true);
        assert_eq!(a.risk, Some(Risk::Low));
        assert_eq!(a.owner.as_deref(), Some("Slack"));
        let a = c.assess(Path::new("/Users/u/Library/Application Support/Slack/Code Cache"), true);
        assert_eq!(a.risk, Some(Risk::Low), "generic names apply at any depth");
        let a = c.assess(Path::new("/Users/u/Downloads/Xcode.xip"), false);
        assert_eq!(a.what, "Xcode archive");
        let a = c.assess(Path::new("/Users/u/Documents/thing"), true);
        assert_eq!(a.risk, None);
        assert!(!a.no_installed_owner);
    }
}
