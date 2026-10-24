//! Text for careful mode (spec 3C), shared by `diskdrift careful` and the TUI.

use crate::careful::{CarefulOutcome, Held};
use crate::core::units::{SizeUnits, format_size};

/// Shown when careful mode starts.
pub const ON: &str = "Careful mode is on: an external disk connected now is not mounted, so nothing is written to it \
(no repair, no journal replay, no Finder or Spotlight files). Disks mounted already are left as they are. \
For a disk that really matters, a hardware write blocker is the only real guarantee.";

/// Shown when it ends.
pub const OFF: &str = "Careful mode is off: macOS mounts disks normally again.";

/// One line on a held volume: what it is and what the store knows.
pub fn held(h: &Held, u: SizeUnits, now: i64) -> String {
    let v = &h.volume.info;
    let mut facts = vec![v.fs_type.clone()];
    if let Some(c) = v.capacity_bytes {
        facts.push(format_size(c, u));
    }
    let device: Vec<&str> = [v.device.bus.as_deref(), v.device.model.as_deref()].into_iter().flatten().collect();
    if !device.is_empty() {
        facts.push(device.join(" "));
    }
    if h.volume.writable == Some(false) {
        facts.push("write-protected".into());
    }
    let what = match (&h.disk, &h.doubt) {
        (_, Some(why)) => format!("doubtful: {why}"),
        (Some(d), None) => match &h.last_scan {
            Some(at) => format!("known disk {}, last scanned {}", d.display_name(), crate::text::search::scan_when(at, now)),
            None => format!("known disk {}, never scanned", d.display_name()),
        },
        (None, None) => "a new disk".into(),
    };
    format!("{} ({}) held back, not mounted: {} [{}]", h.name(), facts.join(", "), what, h.volume.bsd_name)
}

/// What a careful scan did, ending with whether the disk can be unplugged.
pub fn outcome(o: &CarefulOutcome, u: SizeUnits) -> String {
    let mut out = String::new();
    match &o.scan {
        Ok(s) => out.push_str(&crate::text::scan_outcome(s, u)),
        Err(e) => {
            out.push_str(&format!("The careful scan did not complete: {e}\n"));
        }
    }
    match &o.ejected {
        Ok(()) => out.push_str("Unmounted and ejected: safe to unplug.\n"),
        Err(e) => out.push_str(&format!("{e}\n")),
    }
    out
}
