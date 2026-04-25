use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CRATES_API: &str = "https://crates.io/api/v1/crates/nerve-tui";
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    last_check_unix: u64,
    last_known_version: String,
}

fn cache_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nerve").join("update_cache.json"))
}

fn read_cache() -> Cache {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_cache(cache: &Cache) -> Option<()> {
    let path = cache_path()?;
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
    let output = std::process::Command::new("curl")
        .args(["-s", "--max-time", "5", "-A", "nerve-update-check", CRATES_API])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("crate")?
        .get("max_version")?
        .as_str()
        .map(String::from)
}

/// Spawn a background thread that hits crates.io if the cached check is older
/// than the TTL. Always non-blocking and best-effort — network failures are
/// silently ignored. Result is persisted for the next launch to read.
pub fn maybe_check_in_background(enabled: bool) {
    if !enabled {
        return;
    }
    let cache = read_cache();
    if now_unix().saturating_sub(cache.last_check_unix) < CHECK_INTERVAL_SECS {
        return;
    }
    std::thread::spawn(|| {
        if let Some(latest) = fetch_latest_version() {
            let updated = Cache {
                last_check_unix: now_unix(),
                last_known_version: latest,
            };
            let _ = write_cache(&updated);
        }
    });
}

/// Read the cached latest version and return it iff strictly newer than the
/// running binary. None means "no banner" — either no cache yet, network down,
/// or we're already on the latest.
pub fn pending_update(current: &str) -> Option<String> {
    let cache = read_cache();
    if cache.last_known_version.is_empty() {
        return None;
    }
    if is_newer(&cache.last_known_version, current) {
        Some(cache.last_known_version)
    } else {
        None
    }
}

fn is_newer(latest: &str, current: &str) -> bool {
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
