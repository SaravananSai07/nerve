mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

// `--dump` and `--list` are non-TUI CLI paths used by scripts and humans
// before opening the dashboard. The first-scenario (live session) can't
// be reproduced in cargo's harness, but the empty-sessions and
// never-installed paths are exactly the ones most likely to silently
// regress: an errant `unwrap()` on the sessions-dir read, or a panic
// on an empty registry, would only surface from this file.

const TIMEOUT: Duration = Duration::from_secs(5);

fn run_cli(args: &[&str], home: &std::path::Path, config: &std::path::Path) -> (i32, String, String) {
    let bin = env!("CARGO_BIN_EXE_nerve");
    let child = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env("NERVE_CONFIG_DIR", config)
        // nerve also scans $CLAUDE_CONFIG_DIR; don't let the developer's
        // real sessions leak into the fixture.
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nerve");

    let done = common::wait_with_timeout(child, TIMEOUT)
        .unwrap_or_else(|| panic!("nerve {args:?} did not exit within {TIMEOUT:?}"));
    (done.status.code().unwrap_or(-1), done.stdout, done.stderr)
}

fn fixture_dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    (tmp, home, config)
}

/// On a clean success run, stderr must be empty. A regression that
/// adds a noisy eprintln to a code path still exiting 0 (e.g. a new
/// "could not read sessions dir: permission denied" warning) would
/// otherwise slip through with only the exit-code assertion.
fn assert_clean_success(code: i32, stderr: &str) {
    assert_eq!(code, 0, "exit code; stderr: {stderr}");
    assert!(stderr.is_empty(), "expected empty stderr, got: {stderr}");
}

#[test]
fn dump_with_empty_sessions_dir_returns_empty_json_array() {
    let (_tmp, home, config) = fixture_dirs();
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();

    let (code, stdout, stderr) = run_cli(&["--dump"], &home, &config);
    assert_clean_success(code, &stderr);

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("--dump must always emit valid JSON");
    assert!(json.is_array(), "--dump root must be an array, got: {stdout}");
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[test]
fn dump_with_missing_claude_dir_returns_empty_json_array() {
    let (_tmp, home, config) = fixture_dirs();
    // Intentionally do not create `~/.claude/`. First-run / never-installed
    // path: nerve must not crash, must still emit valid JSON.

    let (code, stdout, stderr) = run_cli(&["--dump"], &home, &config);
    assert_clean_success(code, &stderr);

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("--dump must always emit valid JSON");
    assert!(json.is_array());
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[test]
fn list_with_empty_sessions_dir_exits_cleanly() {
    let (_tmp, home, config) = fixture_dirs();
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();

    let (code, stdout, stderr) = run_cli(&["--list"], &home, &config);
    assert_clean_success(code, &stderr);
    assert!(!stdout.is_empty(), "--list should print at least an empty-state line, got nothing");
}

#[test]
fn list_with_missing_claude_dir_exits_cleanly() {
    let (_tmp, home, config) = fixture_dirs();
    // No `~/.claude/`.

    let (code, _stdout, stderr) = run_cli(&["--list"], &home, &config);
    assert_clean_success(code, &stderr);
}

// Only the first argument picks the command; anything else is an error
// rather than a silent TUI launch or a flag found mid-argv.
#[test]
fn unknown_command_is_refused() {
    let (_tmp, home, config) = fixture_dirs();
    let (code, _stdout, stderr) = run_cli(&["frobnicate", "--list"], &home, &config);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("unknown command"), "stderr: {stderr}");
}
