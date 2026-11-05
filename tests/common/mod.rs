//! Shared by the integration tests; each one uses a different part.
#![allow(dead_code)]

use std::io::Read;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Wait for `child` to exit and collect its output. On timeout the child
/// is killed and `None` is returned.
pub fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<Finished> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (mut stdout, mut stderr) = (String::new(), String::new());
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                return Some(Finished { status, stdout, stderr });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
