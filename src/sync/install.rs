//! What `nerve connect` installs on a device: the nerve mod for Claude
//! Code (or settings hooks where Claude predates mods), and a service
//! that keeps the agent running. Each step checks what is in place, so
//! re-running it is safe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// First Claude Code release with mods.
const MODS_SINCE: (u32, u32, u32) = (2, 1, 287);

const MOD_MARKETPLACE: &str = include_str!("../../mod/.claude-plugin/marketplace.json");
const MOD_PLUGIN: &str = include_str!("../../mod/nerve/.claude-plugin/plugin.json");
const MOD_HOOKS: &str = include_str!("../../mod/nerve/hooks/hooks.json");
const MOD_REGISTER: &str = include_str!("../../mod/nerve/hooks/register.js");

/// "2.1.295 (Claude Code)" → (2, 1, 295).
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s.split_whitespace().next()?;
    let mut p = v.split('.').map(|x| x.parse::<u32>().ok());
    Some((p.next()??, p.next()??, p.next()??))
}

fn claude_version() -> Option<(u32, u32, u32)> {
    let out = Command::new(super::bins::find("claude").ok()?)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// Connect Claude: the nerve mod, or settings hooks where Claude predates mods.
pub(super) fn claude(dir: &Path, say: &dyn Fn(bool, &str)) {
    let Some(v) = claude_version() else {
        say(false, "Claude Code not found on PATH: sessions can be watched once it's installed; run `nerve connect` again after.");
        return;
    };
    if v < MODS_SINCE {
        let msg = hooks(dir).unwrap_or_else(|e| e);
        say(
            false,
            &format!(
                "Claude Code {}.{}.{} predates mods (2.1.287); using settings hooks instead. {}",
                v.0,
                v.1,
                v.2,
                msg.replace('\n', " ")
            ),
        );
        return;
    }
    match install_mod(dir) {
        Ok(()) => {
            say(true, &format!("nerve mod installed for Claude Code {}.{}.{} (new sessions load it)", v.0, v.1, v.2))
        }
        Err(e) => say(false, &format!("couldn't install the nerve mod: {e}")),
    }
}

const MOD_FILES: [(&str, &str); 4] = [
    (".claude-plugin/marketplace.json", MOD_MARKETPLACE),
    ("nerve/.claude-plugin/plugin.json", MOD_PLUGIN),
    ("nerve/hooks/hooks.json", MOD_HOOKS),
    ("nerve/hooks/register.js", MOD_REGISTER),
];

fn write_mod_files(root: &Path) -> Result<(), String> {
    for (rel, body) in MOD_FILES {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
        std::fs::write(&p, body).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// After an upgrade, bring an installed mod up to this nerve's copy.
/// Claude loads a local plugin from its folder, so the files are enough;
/// sessions pick it up when they next start.
pub(crate) fn refresh_mod(dir: &Path) {
    let root = dir.join("mod");
    let stale = |(rel, body): &(&str, &str)| std::fs::read_to_string(root.join(rel)).map_or(true, |s| s != *body);
    if root.join("nerve/nerve.json").exists() && MOD_FILES.iter().any(stale) {
        let _ = write_mod_files(&root);
    }
}

/// Write the mod into `<sync>/mod` and register it as a local plugin
/// marketplace. The plugin reads its socket path from `nerve.json`.
fn install_mod(dir: &Path) -> Result<(), String> {
    let root = dir.join("mod");
    write_mod_files(&root)?;
    let sock = super::modhub::socket_path(dir);
    let cfg = serde_json::json!({ "socket": sock.to_string_lossy() });
    std::fs::write(root.join("nerve/nerve.json"), cfg.to_string()).map_err(|e| e.to_string())?;
    let exe = super::bins::find("claude")?;
    let claude = |args: &[&str]| -> Result<String, String> {
        let out = Command::new(&exe).args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            Ok(text)
        } else {
            Err(text.trim().to_string())
        }
    };
    let root_s = root.to_string_lossy().into_owned();
    if let Err(e) = claude(&["plugin", "marketplace", "add", &root_s]) {
        if !e.contains("already") {
            return Err(e);
        }
        let _ = claude(&["plugin", "marketplace", "update", "nerve-local"]);
    }
    match claude(&["plugin", "install", "nerve@nerve-local"]) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("already") => claude(&["plugin", "enable", "nerve@nerve-local"]).map(|_| ()).or(Ok(())),
        Err(e) => Err(e),
    }
}

fn exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

/// A per-config suffix, so a second nerve config (the demo, a test)
/// gets its own service instead of replacing the real one.
fn label_suffix() -> String {
    match std::env::var("NERVE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => format!(".{}", &super::chunks::id_of(d.as_bytes())[..8]),
        _ => String::new(),
    }
}

fn is_termux() -> bool {
    std::env::var("PREFIX").is_ok_and(|p| p.contains("com.termux"))
}

/// launchd keeps the agent up on macOS.
#[cfg(target_os = "macos")]
fn launchd(exe: &Path, log: &Path, path_env: &str, cfg_env: Option<&str>) -> Option<Result<String, String>> {
    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    Some((|| {
        let label = format!("dev.nerve.sync{}", label_suffix());
        let plist_dir = dirs::home_dir().ok_or("no home dir")?.join("Library/LaunchAgents");
        std::fs::create_dir_all(&plist_dir).map_err(|e| e.to_string())?;
        let plist = plist_dir.join(format!("{label}.plist"));
        let mut env = format!("<key>PATH</key><string>{}</string>", xml(path_env));
        if let Some(c) = &cfg_env {
            env.push_str(&format!("<key>NERVE_CONFIG_DIR</key><string>{}</string>", xml(c)));
        }
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key><array><string>{exe}</string><string>sync</string><string>agent</string></array>
    <key>EnvironmentVariables</key><dict>{env}</dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
    </dict></plist>
    "#,
            exe = xml(&exe.to_string_lossy()),
            log = xml(&log.to_string_lossy()),
        );
        std::fs::write(&plist, body).map_err(|e| e.to_string())?;
        let domain = format!("gui/{}", nix::unistd::getuid());
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &plist.to_string_lossy()])
            .stderr(Stdio::null())
            .status();
        let st = Command::new("launchctl")
            .args(["bootstrap", &domain, &plist.to_string_lossy()])
            .status()
            .map_err(|e| e.to_string())?;
        if !st.success() {
            return Err("launchctl bootstrap failed".into());
        }
        Ok(format!("background agent runs at login (launchd {label}; log {})", log.display()))
    })())
}

/// No launchd off macOS: the other managers get their turn.
#[cfg(not(target_os = "macos"))]
fn launchd(_: &Path, _: &Path, _: &str, _: Option<&str>) -> Option<Result<String, String>> {
    None
}

/// Keep the agent running: launchd (macOS), a systemd user service
/// (Linux), or a detached process (Termux, containers).
pub(super) fn service(dir: &Path) -> Result<String, String> {
    let exe = exe()?;
    let log = dir.join("agent.log");
    // Services start with a bare PATH; without ~/.local/bin a server's agent
    // can't find claude for hand-offs and started sessions.
    let path_env = super::bins::service_path(&std::env::var("PATH").unwrap_or_default(), &super::bins::extra_dirs());
    let cfg_env = std::env::var("NERVE_CONFIG_DIR").ok().filter(|s| !s.is_empty());
    if let Some(done) = launchd(&exe, &log, &path_env, cfg_env.as_deref()) {
        return done;
    }
    let systemd = !is_termux()
        && Command::new("systemctl")
            .args(["--user", "show-environment"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if systemd {
        let name = format!("nerve-sync{}", label_suffix());
        let unit_dir = dirs::config_dir().ok_or("no config dir")?.join("systemd/user");
        std::fs::create_dir_all(&unit_dir).map_err(|e| e.to_string())?;
        let mut env = format!("Environment=PATH={path_env}\n");
        if let Some(c) = &cfg_env {
            env.push_str(&format!("Environment=NERVE_CONFIG_DIR={c}\n"));
        }
        let unit = format!(
            "[Unit]\nDescription=nerve sync agent\nAfter=network-online.target\n\n[Service]\nExecStart={} sync agent\n{env}Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        );
        std::fs::write(unit_dir.join(format!("{name}.service")), unit).map_err(|e| e.to_string())?;
        let sc = |a: &[&str]| Command::new("systemctl").arg("--user").args(a).status().is_ok_and(|s| s.success());
        if !(sc(&["daemon-reload"])
            && sc(&["enable", &format!("{name}.service")])
            && sc(&["restart", &format!("{name}.service")]))
        {
            return Err("systemctl --user failed".into());
        }
        // Without lingering, user services stop at logout: a headless server
        // would lose its agent when the SSH session ends.
        let user = std::env::var("USER").unwrap_or_default();
        let linger = Command::new("loginctl")
            .args(["enable-linger", &user])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        return Ok(format!(
            "background agent runs as systemd user service {name}{}",
            if linger {
                " (also while logged out)"
            } else {
                "; run `sudo loginctl enable-linger $USER` to keep it up while logged out"
            }
        ));
    }
    // No service manager (Termux, containers): start it detached now.
    if super::agent::agent_alive(dir) {
        return Ok("background agent already running".into());
    }
    let logf = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args(["sync", "agent"]).stdin(Stdio::null()).stdout(logf.try_clone().map_err(|e| e.to_string())?).stderr(logf);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // It outlives this call; a thread waits on it so it never lingers as a zombie.
    std::thread::Builder::new()
        .name("nerve-agent-wait".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|e| e.to_string())?;
    let mut how = format!("background agent started (log {})", log.display());
    if is_termux() {
        let _ = Command::new("termux-wake-lock").status();
        if let Some(boot) = dirs::home_dir().map(|h| h.join(".termux/boot")) {
            if std::fs::create_dir_all(&boot).is_ok() {
                let script = format!(
                    "#!/data/data/com.termux/files/usr/bin/sh\ntermux-wake-lock\nexec {} sync agent >> {} 2>&1\n",
                    exe.display(),
                    log.display()
                );
                let p = boot.join("nerve-sync");
                if std::fs::write(&p, script).is_ok() {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
                    how.push_str("; with the Termux:Boot app it also starts at boot");
                }
            }
        }
    }
    Ok(how)
}

/// Claude settings that route permission prompts and questions through
/// the mailbox. Used with `claude --settings <file>` or merged into
/// ~/.claude/settings.json by hand.
pub(crate) fn hooks(dir: &Path) -> Result<String, String> {
    let fleet = super::fleet::set_up(dir)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cmd = format!("'{}' sync hook", exe.display());
    let t = fleet.hook_timeout_secs + 15;
    let settings = serde_json::json!({
        "hooks": {
            "PermissionRequest": [{"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeout": t}]}],
            "PreToolUse": [{"matcher": "AskUserQuestion", "hooks": [{"type": "command", "command": cmd, "timeout": t}]}]
        }
    });
    let path = dir.join("claude-settings.json");
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap_or_default()).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {p}\nStart sessions with:  claude --settings {p}\nor merge its \"hooks\" into ~/.claude/settings.json to cover every session.",
        p = path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_versions_compare() {
        assert_eq!(parse_version("2.1.295 (Claude Code)"), Some((2, 1, 295)));
        assert!(parse_version("2.1.286 (Claude Code)").unwrap() < MODS_SINCE);
        assert!(parse_version("2.2.0").unwrap() >= MODS_SINCE);
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn embedded_mod_is_consistent() {
        let m: serde_json::Value = serde_json::from_str(MOD_MARKETPLACE).unwrap();
        assert_eq!(m["name"], "nerve-local");
        assert_eq!(m["plugins"][0]["name"], "nerve");
        let h: serde_json::Value = serde_json::from_str(MOD_HOOKS).unwrap();
        assert_eq!(h["modules"][0], "./register.js");
        assert!(MOD_REGISTER.contains("/v1/poll") && MOD_REGISTER.contains("/v1/request"));
    }
}
