#[cfg(windows)]
include!("support/windows_baseline_windows.rs");

#[cfg(not(windows))]
#[test]
fn windows_native_baseline_requires_a_windows_runtime() {
    eprintln!(
        "Windows native baseline unavailable on this runtime; CI must run it on windows-latest"
    );
}
