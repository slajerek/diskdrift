//! Prints what DiskDrift learns about the disk holding each path (read-only):
//! `cargo run -p diskdrift-platform --example disk_info -- / /Volumes/Backup`
fn main() {
    for p in std::env::args().skip(1) {
        match diskdrift_platform::disk_info(std::path::Path::new(&p)) {
            Ok(i) => println!("{p}: {i:#?}"),
            Err(e) => println!("{p}: {e}"),
        }
    }
}
