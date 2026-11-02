#[cfg(windows)]
include!("support/windows_propagation_windows.rs");

#[cfg(not(windows))]
#[test]
fn windows_propagation_requires_a_windows_runtime() {
    eprintln!(
        "Windows propagation tests unavailable on this runtime; CI must run them on windows-latest"
    );
}
