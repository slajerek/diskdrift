//! Running read-only system tools with a timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("{program}: could not start: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("{program}: timed out after {secs}s")]
    Timeout { program: String, secs: u64 },
    #[error("{program}: exited with status {status}: {stderr}")]
    Failed { program: String, status: i32, stderr: String },
}

/// Runs `program args` and returns stdout. The child is killed after `timeout`.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Vec<u8>, CommandError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CommandError::Spawn { program: program.into(), source })?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandError::Timeout { program: program.into(), secs: timeout.as_secs() });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(source) => return Err(CommandError::Spawn { program: program.into(), source }),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(CommandError::Failed {
            program: program.into(),
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&err).trim().to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[test]
    #[cfg(unix)]
    fn captures_output_and_failures() {
        assert_eq!(run("/bin/echo", &["hi"], Duration::from_secs(5)).unwrap(), b"hi\n");
        assert!(matches!(run("/usr/bin/false", &[], Duration::from_secs(5)), Err(CommandError::Failed { .. })));
        assert!(matches!(run("/bin/sleep", &["5"], Duration::from_millis(200)), Err(CommandError::Timeout { .. })));
        assert!(matches!(run("/nonexistent/tool", &[], Duration::from_secs(1)), Err(CommandError::Spawn { .. })));
    }
}
