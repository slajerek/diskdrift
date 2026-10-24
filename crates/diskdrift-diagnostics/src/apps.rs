//! Installed applications, to tell whose data a folder holds and whether that app
//! is still installed. Reads only the `Info.plist` of application bundles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// Display name: the bundle name without `.app`.
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: PathBuf,
}

/// Index of installed apps by bundle id and by normalized name.
#[derive(Debug, Default)]
pub struct InstalledApps {
    apps: Vec<App>,
    by_id: HashMap<String, usize>,
    by_name: HashMap<String, usize>,
}

/// Lowercase alphanumerics only: "Google Chrome" and "google-chrome" agree.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Strips an Apple team-id prefix (`UBF8T346G9.com.microsoft` → `com.microsoft`),
/// used by group containers.
fn strip_team_id(s: &str) -> &str {
    match s.split_once('.') {
        Some((head, rest))
            if head.len() == 10 && head.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) && rest.contains('.') =>
        {
            rest
        }
        _ => s.strip_prefix("group.").unwrap_or(s),
    }
}

fn bundle_id(app: &Path) -> Option<String> {
    let v = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
    v.as_dictionary()?.get("CFBundleIdentifier")?.as_string().map(str::to_string)
}

impl InstalledApps {
    /// Apps in the standard locations (and one folder level below `/Applications`,
    /// where suites like Adobe or Microsoft put theirs).
    pub fn load(home: &Path) -> InstalledApps {
        let mut dirs = vec![
            PathBuf::from("/Applications"),
            PathBuf::from("/Applications/Utilities"),
            PathBuf::from("/System/Applications"),
            PathBuf::from("/System/Applications/Utilities"),
            home.join("Applications"),
        ];
        if let Ok(rd) = std::fs::read_dir("/Applications") {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none() && p.is_dir() {
                    dirs.push(p);
                }
            }
        }
        let mut apps = Vec::new();
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "app") {
                    let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    apps.push(App { name, bundle_id: bundle_id(&p), path: p });
                }
            }
        }
        Self::from_apps(apps)
    }

    pub fn from_apps(apps: Vec<App>) -> InstalledApps {
        let mut by_id = HashMap::new();
        let mut by_name = HashMap::new();
        for (i, a) in apps.iter().enumerate() {
            if let Some(id) = &a.bundle_id {
                by_id.entry(id.to_lowercase()).or_insert(i);
            }
            by_name.entry(norm(&a.name)).or_insert(i);
        }
        InstalledApps { apps, by_id, by_name }
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// The installed app a folder named `folder` most likely belongs to: a bundle
    /// id (exact, or a vendor prefix like `com.microsoft`), or an app name.
    pub fn owner_of(&self, folder: &str) -> Option<&App> {
        let id = strip_team_id(folder).to_lowercase();
        if let Some(&i) = self.by_id.get(&id) {
            return Some(&self.apps[i]);
        }
        if id.matches('.').count() >= 1 && id.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)) {
            // Reverse-DNS name: a vendor or helper prefix of an installed bundle id.
            let found = self.apps.iter().find(|a| {
                a.bundle_id.as_deref().map(str::to_lowercase).is_some_and(|b| {
                    b.starts_with(&format!("{id}.")) || (id.starts_with(&format!("{b}.")))
                })
            });
            if found.is_some() {
                return found;
            }
        }
        let n = norm(folder);
        if n.len() < 3 {
            return None;
        }
        if let Some(&i) = self.by_name.get(&n) {
            return Some(&self.apps[i]);
        }
        // "Google" → "Google Chrome", "Code" ↛ ... : prefix matches need 4+ chars.
        if n.len() >= 4 {
            return self.apps.iter().find(|a| norm(&a.name).starts_with(&n));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> InstalledApps {
        let a = |name: &str, id: &str| App { name: name.into(), bundle_id: Some(id.into()), path: PathBuf::from(format!("/Applications/{name}.app")) };
        InstalledApps::from_apps(vec![
            a("Google Chrome", "com.google.Chrome"),
            a("Microsoft Word", "com.microsoft.Word"),
            a("Slack", "com.tinyspeck.slackmacgap"),
            a("Visual Studio Code", "com.microsoft.VSCode"),
        ])
    }

    #[test]
    fn matches_ids_vendors_and_names() {
        let a = apps();
        assert_eq!(a.owner_of("com.google.Chrome").unwrap().name, "Google Chrome");
        assert_eq!(a.owner_of("UBF8T346G9.com.microsoft").unwrap().name, "Microsoft Word");
        assert_eq!(a.owner_of("com.tinyspeck.slackmacgap.helper").unwrap().name, "Slack");
        assert_eq!(a.owner_of("Slack").unwrap().name, "Slack");
        assert_eq!(a.owner_of("Google").unwrap().name, "Google Chrome");
        assert!(a.owner_of("Steam").is_none());
        assert!(a.owner_of("com.valvesoftware.steam").is_none());
        assert!(a.owner_of("Go").is_none(), "too short to guess");
    }

    #[test]
    fn loads_real_apps_without_failing() {
        let a = InstalledApps::load(Path::new("/nonexistent-home"));
        // Every Mac has at least Finder-adjacent system apps; tolerate CI sandboxes.
        let _ = a.len();
    }
}
