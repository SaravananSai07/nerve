//! `nerve-tui`: the command's name before 0.5.1. Kept as an alias so a
//! `cargo install` upgrade replaces it instead of leaving a stale copy
//! behind (cargo doesn't remove binaries a package stopped providing).
//! It execs the `nerve` installed next to it.

use std::os::unix::process::CommandExt;

fn main() {
    let nerve = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("nerve"))
        .filter(|p| p.is_file());
    let Some(nerve) = nerve else {
        eprintln!("nerve-tui: the command is now `nerve`, but it isn't installed next to this alias.");
        eprintln!("Reinstall with: cargo install nerve-tui --locked --force");
        std::process::exit(1);
    };
    // `exec` keeps the pid and the controlling tty, so the TUI, signals and
    // the single-instance lock behave exactly as if `nerve` was run.
    let err = std::process::Command::new(&nerve)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("nerve-tui: failed to run {}: {err}", nerve.display());
    std::process::exit(1);
}
