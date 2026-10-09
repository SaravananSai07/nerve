use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// End-to-end check that two concurrent nerve invocations cannot both reach
/// the TUI: the second exits 0 with an "already running" message printed to
/// stderr. The lockfile check fires before `TerminalGuard::install`, so this
/// test works in cargo's non-tty environment.
#[test]
fn second_instance_refuses_to_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    // Hand-roll the lockfile content with the current process's pid. The
    // second invocation will see it, fail to acquire, read the pid, and
    // print the "already running as PID X" message. This sidesteps the
    // need to actually spawn a long-running first instance (which would
    // need a real tty to survive past TerminalGuard::install).
    let lock_path = config_dir.join("nerve.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open lockfile");

    // Hold an exclusive flock on it so the child can't acquire it.
    use nix::fcntl::{Flock, FlockArg};
    let mut held = match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
        Ok(l) => l,
        Err((_, e)) => panic!("could not hold test lock: {e}"),
    };
    writeln!(*held, "{}", std::process::id()).unwrap();
    held.sync_data().ok();

    let bin = env!("CARGO_BIN_EXE_nerve");
    let output = Command::new(bin)
        .env("NERVE_CONFIG_DIR", &config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The binary doesn't need a deadline; it short-circuits on lockfile.
        // We still kill after a generous timeout to avoid hanging the test.
        .spawn()
        .expect("spawn nerve");

    let pid = output.id();
    let timeout = Duration::from_secs(5);
    let result = wait_with_timeout(output, timeout);

    drop(held);

    let (status, stderr) = result.unwrap_or_else(|| {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        panic!("nerve did not exit within {timeout:?}");
    });

    assert!(
        status.success(),
        "nerve should exit 0 when another instance is running, got {status:?}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("already running"),
        "expected 'already running' in stderr, got:\n{stderr}"
    );
    let my_pid = std::process::id().to_string();
    assert!(
        stderr.contains(&my_pid),
        "expected the holder pid {my_pid} in stderr, got:\n{stderr}"
    );
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<(std::process::ExitStatus, String)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut stderr);
                }
                return Some((status, stderr));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}
