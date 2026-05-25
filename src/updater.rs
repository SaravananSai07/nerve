use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::log_warn;
use crate::paths::Paths;

const CRATES_API: &str = "https://crates.io/api/v1/crates/nerve-tui";
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    last_check_unix: u64,
    last_known_version: String,
}

fn read_cache(path: &PathBuf) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_cache(path: &PathBuf, cache: &Cache) -> Option<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let json = serde_json::to_string(cache).ok()?;
    std::fs::write(path, json).ok()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fetch_latest_version() -> Option<String> {
    // `--max-filesize 65536`: refuse a hostile / corrupted response that
    // attempts to feed us a multi-MB payload. The real crates.io response
    // is well under 10 KiB.
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "5",
            "--max-filesize",
            "65536",
            "-A",
            "nerve-update-check",
            CRATES_API,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let raw = json.get("crate")?.get("max_version")?.as_str()?;
    // Sanitise at the network boundary. A hostile crates.io response
    // (or TLS MITM) could plant ANSI / control bytes in a string
    // that later renders in the cards view via the update banner
    // and persists into prefs.toml via the `u` dismissal. Every
    // other ingestion point in the codebase strips at boundary;
    // this one shouldn't be the exception.
    let cleaned: String = crate::util::sanitize::strip_ansi(raw)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Spawn a background thread that hits crates.io if the cached check is older
/// than the TTL. Always non-blocking and best-effort — network failures are
/// silently ignored. Result is persisted for the next launch to read.
pub fn maybe_check_in_background(paths: &Paths, enabled: bool) {
    if !enabled {
        return;
    }
    let path = paths.update_cache_file();
    let cache = read_cache(&path);
    if now_unix().saturating_sub(cache.last_check_unix) < CHECK_INTERVAL_SECS {
        return;
    }
    std::thread::spawn(move || {
        if let Some(latest) = fetch_latest_version() {
            let updated = Cache {
                last_check_unix: now_unix(),
                last_known_version: latest,
            };
            if write_cache(&path, &updated).is_none() {
                log_warn!("updater: failed to persist update cache at {}", path.display());
            }
        }
    });
}

/// Read the cached latest version and return it iff strictly newer than the
/// running binary. None means "no banner" — either no cache yet, network down,
/// or we're already on the latest.
pub fn pending_update(paths: &Paths, current: &str) -> Option<String> {
    let cache = read_cache(&paths.update_cache_file());
    if cache.last_known_version.is_empty() {
        return None;
    }
    if is_newer(&cache.last_known_version, current) {
        Some(cache.last_known_version)
    } else {
        None
    }
}

/// True if `latest` parses as a strictly greater version than `current`.
/// Used by the banner-suppression path to ignore yanked-release
/// downgrades — if `pending_update` reverts to a version older than
/// the one a user explicitly dismissed, we don't bother them again.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn detects_newer_versions() {
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("0.4.0", "0.3.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn ignores_same_or_older() {
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.99", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3.1"));
    }
}
