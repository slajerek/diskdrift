//! Careful mode (spec 3C): a Disk Arbitration mount approval callback that
//! refuses the automatic mount of external volumes, and the read-only mount,
//! unmount and eject the user asks for. Nothing here writes to a disk: a
//! refused volume is not mounted (so not repaired), and DiskDrift's own mount
//! is `rdonly`, which also skips Disk Arbitration's repair.
//!
//! All Disk Arbitration calls run on one thread with its own run loop; the
//! callbacks run there too. The policy lasts as long as that thread: dropping
//! the [`MountGuard`] (or the process ending, also by a crash) switches it off.

use super::cf::{self, CFAllocatorRef, CFDictionaryRef, CFStringRef, CFTypeRef, CFURLRef, Owned};
use super::disk::{
    DADiskCopyDescription, DADiskCopyWholeDisk, DADiskRef, DASessionCreate, DASessionRef, bsd_name, device_of, kDADiskDescriptionVolumeUUIDKey,
};
use crate::{DaError, DiskInfo, GuardEvent, HeldVolume};
use std::collections::HashSet;
use std::ffi::{CString, c_char, c_void};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

type DADissenterRef = CFTypeRef;
type CFRunLoopRef = CFTypeRef;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionScheduleWithRunLoop(session: DASessionRef, run_loop: CFRunLoopRef, mode: CFStringRef);
    fn DASessionUnscheduleFromRunLoop(session: DASessionRef, run_loop: CFRunLoopRef, mode: CFStringRef);
    fn DADiskCreateFromBSDName(allocator: CFAllocatorRef, session: DASessionRef, name: *const c_char) -> DADiskRef;
    fn DARegisterDiskMountApprovalCallback(
        session: DASessionRef,
        matching: CFDictionaryRef,
        callback: extern "C" fn(DADiskRef, *mut c_void) -> DADissenterRef,
        context: *mut c_void,
    );
    fn DARegisterDiskDisappearedCallback(session: DASessionRef, matching: CFDictionaryRef, callback: extern "C" fn(DADiskRef, *mut c_void), context: *mut c_void);
    fn DAUnregisterApprovalCallback(session: DASessionRef, callback: *mut c_void, context: *mut c_void);
    fn DAUnregisterCallback(session: DASessionRef, callback: *mut c_void, context: *mut c_void);
    fn DADiskMountWithArguments(
        disk: DADiskRef,
        path: CFURLRef,
        options: u32,
        callback: extern "C" fn(DADiskRef, DADissenterRef, *mut c_void),
        context: *mut c_void,
        arguments: *const CFStringRef,
    );
    fn DADiskUnmount(disk: DADiskRef, options: u32, callback: extern "C" fn(DADiskRef, DADissenterRef, *mut c_void), context: *mut c_void);
    fn DADiskEject(disk: DADiskRef, options: u32, callback: extern "C" fn(DADiskRef, DADissenterRef, *mut c_void), context: *mut c_void);
    fn DADissenterCreate(allocator: CFAllocatorRef, status: i32, string: CFStringRef) -> DADissenterRef;
    fn DADissenterGetStatus(dissenter: DADissenterRef) -> i32;
    fn DADissenterGetStatusString(dissenter: DADissenterRef) -> CFStringRef;

    static kDADiskDescriptionVolumeKindKey: CFStringRef;
    static kDADiskDescriptionVolumeNameKey: CFStringRef;
    static kDADiskDescriptionVolumeNetworkKey: CFStringRef;
    static kDADiskDescriptionVolumePathKey: CFStringRef;
    static kDADiskDescriptionMediaSizeKey: CFStringRef;
    static kDADiskDescriptionMediaWritableKey: CFStringRef;
    static kDADiskDescriptionDeviceInternalKey: CFStringRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: u8) -> i32;
    static kCFRunLoopDefaultMode: CFStringRef;
}

/// `kDAReturnNotPermitted` (DADissenter.h).
const DA_RETURN_NOT_PERMITTED: i32 = 0xF8DA_0008_u32 as i32;
const DA_DISK_MOUNT_OPTION_DEFAULT: u32 = 0;
const DA_DISK_UNMOUNT_OPTION_WHOLE: u32 = 1;
const DA_DISK_EJECT_OPTION_DEFAULT: u32 = 0;

/// Longest wait for Disk Arbitration to answer a mount, unmount or eject.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

enum Request {
    /// Mount read-only at the default place (`/Volumes/<name>`); `browse`:
    /// visible in Finder (to copy a file off), else `nobrowse`.
    Mount { bsd: String, browse: bool, reply: Sender<Result<PathBuf, DaError>> },
    /// Unmount every volume of the whole disk, then eject it.
    Eject { bsd: String, reply: Sender<Result<(), DaError>> },
    Stop,
}

/// State the callbacks see (they run on the guard thread only).
struct State {
    events: Box<dyn Fn(GuardEvent) + Send>,
    /// Disks DiskDrift is mounting itself: approved.
    ours: HashSet<String>,
    /// Disks whose automatic mount was refused.
    held: HashSet<String>,
}

/// While it lives, the automatic mount of external volumes is refused and
/// reported as [`GuardEvent::Held`].
pub struct MountGuard {
    requests: Sender<Request>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MountGuard {
    pub fn start(events: Box<dyn Fn(GuardEvent) + Send>) -> std::io::Result<MountGuard> {
        let (requests, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("diskdrift-careful".into()).spawn(move || run(events, rx, ready_tx))?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(MountGuard { requests, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::other("Disk Arbitration did not start")),
        }
    }

    /// Mounts the volume `bsd` read-only; returns where.
    pub fn mount_read_only(&self, bsd: &str, browse: bool) -> Result<PathBuf, DaError> {
        let (reply, answer) = mpsc::channel();
        self.ask(Request::Mount { bsd: bsd.into(), browse, reply }, answer)
    }

    /// Unmounts every volume of the disk holding `bsd`, then ejects it.
    pub fn unmount_and_eject(&self, bsd: &str) -> Result<(), DaError> {
        let (reply, answer) = mpsc::channel();
        self.ask(Request::Eject { bsd: bsd.into(), reply }, answer)
    }

    fn ask<T>(&self, req: Request, answer: Receiver<Result<T, DaError>>) -> Result<T, DaError> {
        let gone = || DaError { status: 0, message: Some("careful mode is off".into()) };
        self.requests.send(req).map_err(|_| gone())?;
        answer.recv_timeout(ANSWER_TIMEOUT).unwrap_or_else(|_| Err(DaError { status: 0, message: Some("Disk Arbitration did not answer".into()) }))
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn dissenter_error(d: DADissenterRef) -> DaError {
    unsafe { DaError { status: DADissenterGetStatus(d) as u32, message: cf::string_of(DADissenterGetStatusString(d)) } }
}

/// What a volume is, from its Disk Arbitration description; `None` for one
/// careful mode leaves alone (internal, network, no filesystem).
fn held_volume(disk: DADiskRef) -> Option<HeldVolume> {
    unsafe {
        let desc = Owned::new(DADiskCopyDescription(disk))?;
        let get = |key: CFStringRef| cf::dict_get(desc.0, key);
        let fs_type = cf::string_of(get(kDADiskDescriptionVolumeKindKey))?;
        if cf::bool_of(get(kDADiskDescriptionVolumeNetworkKey)) == Some(true) || cf::bool_of(get(kDADiskDescriptionDeviceInternalKey)) == Some(true) {
            return None;
        }
        let device = device_of(disk, desc.0);
        if device.internal == Some(true) {
            return None;
        }
        Some(HeldVolume {
            bsd_name: bsd_name(disk)?,
            writable: cf::bool_of(get(kDADiskDescriptionMediaWritableKey)),
            info: DiskInfo {
                mount_point: PathBuf::new(),
                fs_type,
                volume_name: cf::string_of(get(kDADiskDescriptionVolumeNameKey)),
                volume_uuid: cf::string_of(get(kDADiskDescriptionVolumeUUIDKey)),
                capacity_bytes: cf::number_of(get(kDADiskDescriptionMediaSizeKey)),
                bsd_name: bsd_name(disk),
                device,
            },
        })
    }
}

extern "C" fn approve_mount(disk: DADiskRef, context: *mut c_void) -> DADissenterRef {
    // SAFETY: `context` is the State owned by the guard thread, which is the
    // only thread these callbacks run on.
    let state = unsafe { &mut *(context as *mut State) };
    let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bsd = bsd_name(disk)?;
        if state.ours.contains(&bsd) {
            return None;
        }
        let held = held_volume(disk)?;
        // Reported once; a second attempt (`diskutil mount`) is refused quietly.
        if state.held.insert(bsd) {
            (state.events)(GuardEvent::Held(Box::new(held)));
        }
        Some(())
    }));
    match decision {
        Ok(None) => std::ptr::null(),
        // Refused; a panic refuses too: when in doubt, nothing is mounted.
        Ok(Some(())) | Err(_) => {
            let why = cf::cfstr("DiskDrift careful mode: not mounted, nothing written");
            unsafe { DADissenterCreate(std::ptr::null(), DA_RETURN_NOT_PERMITTED, why.0) }
        }
    }
}

extern "C" fn disk_disappeared(disk: DADiskRef, context: *mut c_void) {
    let state = unsafe { &mut *(context as *mut State) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bsd = bsd_name(disk)?;
        let was_ours = state.ours.remove(&bsd);
        if state.held.remove(&bsd) || was_ours {
            (state.events)(GuardEvent::Gone(bsd));
        }
        Some(())
    }));
}

/// A mount waiting for its answer; owns the argument strings until then.
struct PendingMount {
    reply: Sender<Result<PathBuf, DaError>>,
    _args: Vec<Owned>,
}

extern "C" fn mounted(disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void) {
    let p = unsafe { Box::from_raw(context as *mut PendingMount) };
    let r = if dissenter.is_null() {
        unsafe { Owned::new(DADiskCopyDescription(disk)) }
            .and_then(|d| cf::path_of_url(cf::dict_get(d.0, unsafe { kDADiskDescriptionVolumePathKey })))
            .ok_or(DaError { status: 0, message: Some("mounted, but Disk Arbitration does not say where".into()) })
    } else {
        Err(dissenter_error(dissenter))
    };
    let _ = p.reply.send(r);
}

struct PendingEject {
    reply: Sender<Result<(), DaError>>,
}

extern "C" fn unmounted(disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void) {
    if !dissenter.is_null() {
        let p = unsafe { Box::from_raw(context as *mut PendingEject) };
        let _ = p.reply.send(Err(dissenter_error(dissenter)));
        return;
    }
    // The same context goes on to the eject.
    unsafe { DADiskEject(disk, DA_DISK_EJECT_OPTION_DEFAULT, ejected, context) };
}

extern "C" fn ejected(_disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void) {
    let p = unsafe { Box::from_raw(context as *mut PendingEject) };
    let _ = p.reply.send(if dissenter.is_null() { Ok(()) } else { Err(dissenter_error(dissenter)) });
}

fn disk_named(session: DASessionRef, bsd: &str) -> Result<Owned, DaError> {
    let name = CString::new(bsd).map_err(|_| DaError { status: 0, message: Some("bad device name".into()) })?;
    Owned::new(unsafe { DADiskCreateFromBSDName(std::ptr::null(), session, name.as_ptr()) })
        .ok_or_else(|| DaError { status: 0, message: Some(format!("no disk {bsd}")) })
}

fn run(events: Box<dyn Fn(GuardEvent) + Send>, requests: Receiver<Request>, ready: Sender<std::io::Result<()>>) {
    let Some(session) = Owned::new(unsafe { DASessionCreate(std::ptr::null()) }) else {
        let _ = ready.send(Err(std::io::Error::other("cannot open a Disk Arbitration session")));
        return;
    };
    let state = Box::into_raw(Box::new(State { events, ours: HashSet::new(), held: HashSet::new() }));
    let ctx = state as *mut c_void;
    unsafe {
        let run_loop = CFRunLoopGetCurrent();
        DASessionScheduleWithRunLoop(session.0, run_loop, kCFRunLoopDefaultMode);
        DARegisterDiskMountApprovalCallback(session.0, std::ptr::null(), approve_mount, ctx);
        DARegisterDiskDisappearedCallback(session.0, std::ptr::null(), disk_disappeared, ctx);
        let _ = ready.send(Ok(()));
        loop {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 0);
            match requests.try_recv() {
                Ok(Request::Mount { bsd, browse, reply }) => match disk_named(session.0, &bsd) {
                    Ok(disk) => {
                        (*state).ours.insert(bsd);
                        let mut args = vec![cf::cfstr("rdonly")];
                        if !browse {
                            args.push(cf::cfstr("nobrowse"));
                        }
                        let mut argv: Vec<CFStringRef> = args.iter().map(|a| a.0).collect();
                        argv.push(std::ptr::null());
                        let pending = Box::into_raw(Box::new(PendingMount { reply, _args: args }));
                        DADiskMountWithArguments(disk.0, std::ptr::null(), DA_DISK_MOUNT_OPTION_DEFAULT, mounted, pending as *mut c_void, argv.as_ptr());
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                },
                Ok(Request::Eject { bsd, reply }) => match disk_named(session.0, &bsd).and_then(|d| {
                    Owned::new(DADiskCopyWholeDisk(d.0)).ok_or_else(|| DaError { status: 0, message: Some(format!("no whole disk for {bsd}")) })
                }) {
                    Ok(whole) => {
                        let pending = Box::into_raw(Box::new(PendingEject { reply }));
                        DADiskUnmount(whole.0, DA_DISK_UNMOUNT_OPTION_WHOLE, unmounted, pending as *mut c_void);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                },
                Ok(Request::Stop) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        DAUnregisterApprovalCallback(session.0, approve_mount as *mut c_void, ctx);
        DAUnregisterCallback(session.0, disk_disappeared as *mut c_void, ctx);
        DASessionUnscheduleFromRunLoop(session.0, run_loop, kCFRunLoopDefaultMode);
        drop(session);
        drop(Box::from_raw(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Starting and stopping registers and removes the callback; with no disk
    /// connected meanwhile, nothing is refused or reported.
    #[test]
    fn the_guard_starts_and_stops() {
        let (tx, rx) = mpsc::channel();
        let guard = MountGuard::start(Box::new(move |e| {
            let _ = tx.send(format!("{e:?}"));
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // Disk Arbitration answers (with a refusal), so requests reach it and come back.
        assert!(guard.mount_read_only("disk-that-does-not-exist", false).is_err());
        drop(guard);
        assert!(rx.try_recv().is_err(), "no events");
    }
}
