use std::process::{Command, Output, Stdio};

/// Run an AppleScript with values passed via `argv`, never interpolated into
/// the script body. Reads `argv` inside the script with `on run argv` /
/// `item N of argv`. This is the only safe way to feed user-controlled
/// strings to `osascript(1)` — string interpolation invariably leaves
/// injection holes (newlines, comment markers, `¬` continuation, etc.).
pub fn run(script: &str, args: &[&str]) -> std::io::Result<Output> {
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script).arg("--");
    for a in args {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null()).output()
}

/// Spawn the same shape of invocation but detach (no output capture).
/// Used by the notification path which fires and forgets.
pub fn spawn(script: &str, args: &[&str]) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script).arg("--");
    for a in args {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}
