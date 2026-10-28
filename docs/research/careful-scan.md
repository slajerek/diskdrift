# Research: careful scan of old, possibly failing disks

Status: **first round of experiments done (disk images, 2026-10-22)**; real
old disks still to test. Roadmap step 2 (`ROADMAP.md`, section 5).

## Outcome (2026-10-22, roadmap step 6)

Careful mode shipped as this note proposes (spec 3C): a mount approval callback
refuses the automatic mount of external volumes, DiskDrift mounts one
`rdonly,nobrowse` through `DADiskMountWithArguments`, scans it with one
thread, and unmounts and ejects the whole disk. It is Rust FFI in
`diskdrift-platform/src/macos/guard.rs`, not the Swift prototype. Dirty
volumes get the direct read-only mount as a command for the user to run with
`sudo`; DiskDrift never runs it. Decisions of section 5: the exception is in
CLAUDE.md and spec 3C; no root needed for clean volumes; no "image first"
yet. Copying a file off: `m` mounts read-only for Finder. Still to run, by
the owner: `careful_mode_test.py` on the images, then real disks (section 4,
"Still to test"). Whether Disk Arbitration reports image devices as
internal (careful mode leaves internal disks alone) is part of that test.

## Results in short

- **The danger is real.** A normal mount of a dirty volume changed the image
  on every filesystem tested: HFS+ 30 blocks against 9 when clean, exFAT 7
  against 0, FAT32 6 against 1. A crash-state HFS+ volume had its journal
  replayed (71 blocks). macOS mounting a plugged-in disk is not read-only.
- **Without root, the guard works.** DiskDrift refuses the automatic mount
  through a Disk Arbitration approval callback (the system reports "failed to
  mount: careful mode"), then mounts the volume itself with `rdonly,nobrowse`.
  Result: images **byte-for-byte unchanged** on HFS+, APFS, exFAT and FAT32,
  the scan is complete, and the test file is found. The quick check and probe
  that run before the approval also changed nothing.
- **Dirty or crashed volumes cannot be mounted read-only through Disk
  Arbitration.** Dirty exFAT/FAT32 return EDIRTY (`0xc05c`), and HFS+ with a
  non-empty journal is refused (`0xc001`). A client cannot pass `force`
  (`diskutil -mountOptions force` and `DADiskMountWithArguments(..., "force")`
  both fail). Nothing is written in these cases; the disk is just not
  readable that way.
- **Mounting directly with the filesystem's mount tool works on those** and
  leaves them unchanged: `mount_hfs -j -o rdonly` (ignore the journal),
  `mount_exfat -o rdonly`, `mount_msdos -o rdonly`. This worked **as the user**
  on image devices, because `hdiutil` gives the user ownership of
  `/dev/diskN`. For a real USB disk the device node is probably owned by root
  (`/dev/disk0` here: `root`, `0640`), so this path probably needs root (a
  privileged helper, or the user typing `sudo`). **To test on real hardware.**
- A dirty HFS+ volume with an empty journal (only the "unmounted" bit clear)
  mounts read-only through Disk Arbitration without trouble.

So careful mode can be: guard automatic mounts → mount read-only through Disk
Arbitration → if refused as dirty, explain and offer the direct read-only mount
with administrator rights → scan → unmount and eject. Nothing writes to the disk
at any step.

Goal: connect an old disk, write nothing to it at all, scan names and sizes
once, eject it, and keep it powered for as short a time as possible.

This file separates what is **established** (from Apple's source code or man
pages on this machine), what is **reported** (forums, articles) and what is
**still to be tested**.

## 1. What can write to a disk that is merely connected

| Writer | What | When | Stopped by a read-only mount? |
|---|---|---|---|
| Disk Arbitration repair | `fsck_<fs> -y` on the device | before an automatic mount, if the quick check says the volume is dirty | **Yes, a read-only mount skips the repair (see 2.1). An automatic read-write mount does not.** |
| HFS+ journal replay | rewrites catalog/extents from the journal | when a dirty journaled HFS+ volume is mounted read-write | Yes (`mount_hfs -j` also ignores the journal) |
| Finder | `.DS_Store`; on non-Apple filesystems `._*` AppleDouble | when a folder is opened in Finder | Yes |
| Spotlight | `.Spotlight-V100` | after mount | Yes |
| FSEvents | `.fseventsd` | after mount | Yes |
| Trash, temp files | `.Trashes`, `.TemporaryItems` | on use | Yes |
| Mount itself | last-mounted time, clean/dirty flags (HFS+, FAT, exFAT), APFS checkpoints | at mount and unmount | Should be; **to test, especially APFS** |
| Quota check | `quotacheck -g -u` | after mount, if quotas are enabled | No writes on a read-only mount (to test) |

`defaults write com.apple.desktopservices DSDontWriteUSBStores -bool true`
only stops Finder's `.DS_Store` on USB disks, so it is not a solution.

## 2. Disk Arbitration (established from source)

Source: `apple-oss-distributions/DiskArbitration`, files
`diskarbitrationd/DAMount.c` and `DAFileSystem.c`.

### 2.1 Repair before mount

In `DAMountWithArguments()`:

- An automatic mount starts with `check = true`. A requested mount sets
  `check = false` when the arguments contain `kDAFileSystemMountArgumentNoWrite`
  (read-only), and `true` otherwise. `NoWrite` is also added when the medium
  reports itself as not writable.
- `check` is kept only if the volume is flagged `kDADiskStateRequireRepair`,
  which is set when the quick check finds it dirty.
- If `check` is true, `DAFileSystemRepair` runs `fsck_<fs> -y` (with `-X` for
  hfs and apfs) on the device. **That writes to the disk.**
- A read-only mount of a dirty volume without `force` fails with `EDIRTY`.
  With `kDAFileSystemMountArgumentForce` it mounts without repair.
- An automatic mount whose repair fails is retried with `force` and `NoWrite`,
  that is, read-only without repair.
- The preference `kDAMountPreferenceAlwaysRepair` (default false; "used for
  testing") would force a repair even on a read-only mount.

Consequence: **if a dirty old disk is plugged in and macOS mounts it
automatically, `fsck -y` runs first and repairs (writes) it.** This is exactly
what careful mode must prevent. The automatic mount has to be refused before
it starts, and the disk mounted read-only (with `force` if dirty).

### 2.2 The checks before mount

- Probe: `<fs>.util -p <device> removable readonly`, then `-k` for the UUID.
  This reads the device.
- Quick check: `fsck_<fs> -q <fd>`, which reports clean or dirty. The man pages
  of `fsck_hfs`, `fsck_exfat` and `fsck_msdos` describe `-q` as a quick check;
  whether it can write is **to test**. The device is opened `O_RDONLY` only on
  iOS in this code; on macOS no read-only open is shown.
- The order of the quick check against our mount approval callback is **to
  test**: the quick check may already run before a dissenter can stop the
  mount.

### 2.3 Public API (macOS SDK headers on this machine)

- `DARegisterDiskAppearedCallback`, `DARegisterDiskMountApprovalCallback`
  (return a `DADissenter` to refuse), `DADiskMountWithArguments` (arguments are
  strings, e.g. `rdonly`, `nobrowse`), `DADiskUnmount`, `DADiskEject`,
  `DADiskClaim` (exclusive claim of a disk).
- Mount options: `kDADiskMountOptionDefault`, `Whole`, `NoFollow`. There is
  no "no repair" option; read-only (`NoWrite`) has that effect.
- Disk description keys include `kDADiskDescriptionVolumeUUIDKey` and
  `kDADiskDescriptionMediaWritableKey`, and also the model, bus and
  filesystem type. That is enough to recognise a known disk before mounting
  it.
- The string values of `kDAFileSystemMountArgumentForce` and `NoWrite` are
  not in the public headers. They are most likely `force` and `rdonly`
  (`rdonly` is what Disk Arbitrator passes); **to test**.

### 2.4 Root

- Refusing a mount and mounting a removable disk through Disk Arbitration
  should work without root in a non-sandboxed app; **to test**.
- A sandboxed app is refused the external mount without root or a private
  entitlement ([forum](https://developer.apple.com/forums/thread/775835)).
  DiskDrift is not sandboxed.
- `diskutil mount readOnly <dev>` uses Disk Arbitration as the user. The man
  page says it is equivalent to `-o rdonly`, so even root cannot write while it
  is mounted. Whether it works on a dirty volume (EDIRTY, no `force`) is **to
  test**.
- `mount_hfs -j -o rdonly` (ignore the journal) and `mount_exfat -o rdonly`
  bypass Disk Arbitration but need root.

## 3. Reported, not verified

- On macOS Tahoe, Disk Arbitration ignores `ro` and `noauto` in `/etc/fstab`
  for exFAT and APFS. A mount approval callback still blocks automatic mounts
  ([forum](https://developer.apple.com/forums/thread/844839)).
- Disk Arbitrator, an old (about 2017) open-source app, does exactly this:
  refuse the automatic mount, then mount read-only, with an option to ignore
  the journal. Its README says it is not a write blocker in the forensic sense
  ([mirror](https://github.com/ilovezfs/disk-arbitrator)).
- On a failing disk, a hardware write blocker is the only real guarantee;
  imaging first (`ddrescue`) is the forensic norm, but it reads the whole disk.

## 4. Experiments

Run on 2026-10-22 with the owner's approval, on 64 MB raw read-write images
(`hdiutil create -format UDRW`, so any write to the device changes the image
file). Each run starts from a fresh copy and compares 4 KiB blocks before and
after. The scripts are in
[`experiments/careful-scan/`](../../experiments/careful-scan/README.md)
(`run_all.sh` repeats everything). Every attach and mount was undone, and no
real disk was involved. The scan is the real `diskdrift scan` against a
scratch store.

| Test | HFS+ (journaled) | APFS | exFAT | FAT32 |
|---|---|---|---|---|
| Control: normal mount, scan, 20 s, unmount | changed (9 blocks) | changed (28) | unchanged | changed (1) |
| `diskutil mount readOnly nobrowse`, scan, 20 s | **unchanged**, scan OK | **unchanged**, scan OK | **unchanged**, scan OK | **unchanged**, scan OK |
| Dirty, normal mount (what plugging in does) | changed (30) | – | changed (7) | changed (6) |
| Dirty, `diskutil mount readOnly` | unchanged, scan OK | – | refused, unchanged | refused, unchanged |
| Dirty, `diskutil ... -mountOptions force` | refused | – | refused | refused |
| Dirty, `DADiskMountWithArguments` `rdonly` (+`force`) | OK (`force`: refused) | – | refused EDIRTY | refused EDIRTY |
| Guard: approval callback refuses the read-write request, we mount `rdonly` | **unchanged**, scan OK | **unchanged**, scan OK | **unchanged**, scan OK | **unchanged**, scan OK |
| Guard, dirty | **unchanged**, scan OK | – | refused EDIRTY, unchanged | refused EDIRTY, unchanged |
| Crash state (journal not empty), normal mount | journal replayed (71 blocks) | | | |
| Crash state, read-only (diskutil or guard) | refused (`0xc001`), unchanged | | | |
| Direct `mount_<fs> -o rdonly` as user (HFS+ `-j`) | crash state: **unchanged**, scan OK | | dirty: **unchanged**, scan OK | dirty: **unchanged**, scan OK |

How the states were made: "dirty" means the HFS+ volume-header unmounted bit
cleared, the exFAT VolumeDirty flag set, or the FAT32 clean-shutdown bit in
FAT[1] cleared. "Crash state" is a copy of the image file taken while files
were being written, the same as cutting power (journal start ≠ end, unmounted
bit clear). `hdiutil detach -force` did not produce it: it still flushes.

Not seen directly: on this macOS exFAT and FAT32 are FSKit filesystems (the
mount line says `fskit`), so their repair runs inside a system extension, not
as an `fsck_*` process. The extra changed blocks on dirty volumes are
consistent with a repair but were not observed as one.

### Still to test

1. Real USB disks (HFS+, FAT32, exFAT; an old one that was unplugged without
   ejecting): device ownership, whether the guard sees the automatic mount the
   same way, and the direct mount with `sudo`.
2. Two approval callbacks at once (another app doing the same).
3. How much the probe and the quick check read before mounting (`fs_usage`),
   for very weak disks.
4. NTFS (read-only on macOS anyway) and old HFS (not plus), which macOS no
   longer mounts.
5. APFS crash state (APFS has checkpoints, not a dirty bit).

## 4a. Original plan

The plan as written before the run, kept for reference.

All on **disk image files in the session scratchpad**, never on a real disk.
Each attach and mount is temporary and is undone at the end of the test.

For HFS+ (journaled), APFS, exFAT and FAT32, with a raw read-write image (so
any write changes the image file):

1. **Control**: attach and mount normally, scan, eject. The image hash
   changes, which proves the test can see writes.
2. **Read-only**: `hdiutil attach -nomount -noautofsck`, then
   `diskutil mount readOnly nobrowse -mountPoint <scratch dir>`, scan with
   DiskDrift, wait 60 s (Spotlight, FSEvents), unmount, detach. Compare the
   SHA-256 of the image and count changed bytes with `cmp -l`.
3. **Dirty volume**: mark the image dirty by editing its bytes (HFS+ volume
   header unmounted bit, exFAT VolumeDirty flag, FAT32 clean-shutdown bit).
   - (a) Automatic attach: does Disk Arbitration run `fsck -y`, and does the
     image change?
   - (b) `diskutil mount readOnly`: EDIRTY or mounted?
   - (c) Mounting through `DADiskMountWithArguments` with `rdonly` and `force`.
4. **Approval callback**: a small prototype (Swift or Rust FFI, in the
   scratchpad) registers a mount approval callback that refuses the test
   image, then mounts it read-only itself. Check that nothing ran `fsck -y` and
   the image is unchanged. Also check whether the quick check `-q` changed
   anything.
5. **Time and reads**: how much Disk Arbitration reads before mounting (probe,
   quick check), measured with `fs_usage` or I/O counters, to see how gentle
   "connect and scan" really is.

Real old disks (owner's own tests, later): the same procedure on a few real
HFS+/FAT32 disks, ideally behind a hardware write blocker for comparison.

## 5. Decisions this research feeds

- The exception to the CLAUDE.md "no state-changing `diskutil`" rule:
  refusing mounts, mounting read-only and ejecting, only in careful mode and
  only for disks the user chose.
- Whether careful mode is possible without root: **yes for clean volumes**
  (tested on images); dirty or crashed ones need the direct read-only mount,
  probably with administrator rights on real disks.
- Whether to offer "image first" for disks that fail to read.
