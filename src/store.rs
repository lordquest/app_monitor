//! JSON persistence next to the executable (portable) with a per-user fallback.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::model::{Config, DownloadRecord, Track};
use crate::parse;
use crate::util;

/// One data file that was on disk but could not be used.
///
/// A damaged `tracks.json` used to fall back to the shipped defaults *silently*,
/// and the next save then overwrote whatever the user had written — a hand-edited
/// file, or one truncated by a crash during the write. Nothing user-authored is
/// thrown away any more: the unreadable original is moved aside first, and the UI
/// reports what happened.
pub struct LoadIssue {
    /// File name inside the data directory, e.g. `tracks.json`.
    pub file: &'static str,
    /// What went wrong, already phrased for the banner.
    pub what: String,
    /// What the session is using instead.
    pub fallback: &'static str,
    /// Where the preserved copy went; `None` when it could not be preserved, in
    /// which case the original is left untouched and not rebuilt.
    pub backup: Option<PathBuf>,
}

pub fn data_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("app_monitor_data");
                if std::fs::create_dir_all(&candidate).is_ok()
                    && std::fs::write(candidate.join(".write_test"), b"1").is_ok()
                {
                    let _ = std::fs::remove_file(candidate.join(".write_test"));
                    return candidate;
                }
            }
        }
        let fallback = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("app_monitor");
        let _ = std::fs::create_dir_all(&fallback);
        fallback
    })
}

/// Why a file could not be used: the reason, and where the preserved copy went.
type Damage = (String, Option<PathBuf>);

fn read_json<T: serde::de::DeserializeOwned>(name: &'static str) -> Result<Option<T>, Damage> {
    load_file(&data_dir().join(name), name)
}

/// Read one JSON file, quarantining it if the bytes on disk are not usable.
///
/// `name` only feeds the backup filename; the path is explicit so tests can point
/// at a scratch directory instead of the real portable data folder.
fn load_file<T: serde::de::DeserializeOwned>(path: &Path, name: &str) -> Result<Option<T>, Damage> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // No file is a normal state — first launch, or the user deleted it to start
        // over. Only *unreadable* content is an issue worth reporting.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err((format!("读取失败：{e}"), None)),
    };
    if text.trim().is_empty() {
        // A zero-byte file is what a crash during `write_json` leaves behind (the
        // rename is atomic, but a truncated write before it is not). Treat it as
        // damaged rather than as "no data": same quarantine, same message.
        let backup = quarantine(path, name);
        return Err(("文件是空的，可能是上次写入被中断".into(), backup));
    }
    match serde_json::from_str(&text) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            let backup = quarantine(path, name);
            Err((format!("内容不是合法的 JSON（{e}）"), backup))
        }
    }
}

/// Move an unreadable file aside so that the rebuilt one cannot overwrite it.
fn quarantine(path: &Path, name: &str) -> Option<PathBuf> {
    let stamp = backup_stamp();
    let target = path.with_file_name(format!("{name}.{stamp}.corrupt"));
    if std::fs::rename(path, &target).is_ok() {
        return Some(target);
    }
    // Rename fails when another process holds the file open (a second instance on
    // Windows); copying still keeps the bytes reachable, the original just stays.
    std::fs::copy(path, &target).ok().map(|_| target)
}

/// `2026-10-27-141421Z`, safe as a filename everywhere (no colon or space). The `Z`
/// is deliberate: the stamp is UTC, and a mislabelled local time would confuse
/// anyone trying to match a backup against "when it broke".
fn backup_stamp() -> String {
    // `fmt_date` stops at minutes; the seconds come from the raw stamp so that two
    // quarantines inside the same minute cannot collide.
    let ts = util::now_secs();
    let date = util::fmt_date(ts);
    let mut parts = date.split(' ');
    let day = parts.next().unwrap_or("1970-01-01");
    let hm = parts.next().unwrap_or("00:00").replace(':', "");
    format!("{day}-{hm}{:02}Z", ts.rem_euclid(60))
}

fn write_json<T: serde::Serialize + ?Sized>(name: &str, value: &T) {
    let path = data_dir().join(name);
    let tmp = path.with_extension("json.tmp");
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // config.json can hold a GitHub token; keep the file owner-only instead of
        // whatever the ambient umask allows.
        opts.mode(0o600);
    }
    use std::io::Write;
    if let Ok(mut f) = opts.open(&tmp) {
        if f.write_all(text.as_bytes()).is_ok() {
            drop(f);
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

// Free functions that borrow the caller's data. `App` used to build a throwaway
// `Store` (cloning every track, release and changelog) just to persist, which cost
// a full deep copy per save — and `drain` saves once per resolved item.
pub fn write_tracks(tracks: &[Track]) {
    write_json("tracks.json", tracks);
}

pub fn write_history(history: &[DownloadRecord]) {
    write_json("history.json", history);
}

pub fn write_config(config: &Config) {
    write_json("config.json", config);
}

pub struct Store {
    pub config: Config,
    pub tracks: Vec<Track>,
    pub history: Vec<DownloadRecord>,
    /// True when tracks came from the shipped default paste (first run, or a
    /// `tracks.json` that had to be rebuilt from scratch).
    pub seeded: bool,
    /// Files that were damaged on disk and what replaced them. Empty on a healthy
    /// start; the UI shows a banner for anything in here.
    pub issues: Vec<LoadIssue>,
}

impl Store {
    pub fn load() -> Self {
        let mut issues: Vec<LoadIssue> = Vec::new();

        // `write_default` is false when the damaged file could not be moved aside:
        // rebuilding it would overwrite the only copy of what the user had.
        let (config, write_default_config) = match read_json::<Config>("config.json") {
            Ok(Some(c)) => (c, false),
            // No config at all: materialise one so the portable folder is
            // self-explaining.
            Ok(None) => (Config::default(), true),
            Err((what, backup)) => {
                let safe = backup.is_some();
                issues.push(LoadIssue {
                    file: "config.json",
                    what,
                    fallback: "默认设置",
                    backup,
                });
                (Config::default(), safe)
            }
        };

        let (tracks, seeded) = match read_json::<Vec<Track>>("tracks.json") {
            Ok(Some(t)) if !t.is_empty() => (t, false),
            Ok(_) => (parse::parse_input(parse::DEFAULT_INPUT).tracks, true),
            Err((what, backup)) => {
                let safe = backup.is_some();
                issues.push(LoadIssue {
                    file: "tracks.json",
                    what,
                    fallback: "内置的默认追踪清单",
                    backup,
                });
                (parse::parse_input(parse::DEFAULT_INPUT).tracks, safe)
            }
        };

        let mut history = match read_json::<Vec<DownloadRecord>>("history.json") {
            Ok(h) => h.unwrap_or_default(),
            Err((what, backup)) => {
                issues.push(LoadIssue {
                    file: "history.json",
                    what,
                    fallback: "空历史",
                    backup,
                });
                Vec::new()
            }
        };
        history.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        history.truncate(500);

        let store = Self {
            config,
            tracks,
            history,
            seeded,
            issues,
        };
        if write_default_config {
            store.save_config();
        }
        store
    }

    pub fn save_config(&self) {
        write_config(&self.config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch dir per test: never touch the real `app_monitor_data`.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("am_store_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn healthy_file_loads_without_backup() {
        let dir = scratch("healthy");
        let path = dir.join("tracks.json");
        std::fs::write(&path, r#"[{"a":1}]"#).unwrap();
        let got = load_file::<serde_json::Value>(&path, "tracks.json").unwrap();
        assert!(got.is_some());
        assert!(std::fs::read_dir(&dir).unwrap().count() == 1, "不该产生备份文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_normal() {
        let dir = scratch("missing");
        let got = load_file::<serde_json::Value>(&dir.join("nope.json"), "nope.json");
        assert!(matches!(got, Ok(None)), "文件不存在属于正常状态，不是问题");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关键回归：坏掉的 tracks.json 必须被改名保留，而不是让后续保存把它盖掉。
    #[test]
    fn broken_json_is_quarantined_not_discarded() {
        let dir = scratch("broken");
        let path = dir.join("tracks.json");
        let original = "[{\"id\":\"keep-me\",,,}]";
        std::fs::write(&path, original).unwrap();

        let err = load_file::<serde_json::Value>(&path, "tracks.json").unwrap_err();
        assert!(err.0.contains("不是合法的 JSON"), "原因要说清楚: {}", err.0);
        let backup = err.1.expect("必须留下备份");
        assert!(!path.exists(), "原路径要腾空，避免下次保存覆盖");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        assert!(backup.to_string_lossy().ends_with(".corrupt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_file_is_quarantined_too() {
        let dir = scratch("empty");
        let path = dir.join("config.json");
        std::fs::write(&path, b"").unwrap();
        let err = load_file::<serde_json::Value>(&path, "config.json").unwrap_err();
        assert!(err.0.contains("空的"), "{}", err.0);
        assert!(err.1.is_some(), "零字节文件也要留档，方便查是谁写坏的");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_name_is_windows_safe() {
        let stamp = backup_stamp();
        assert!(!stamp.contains(':') && !stamp.contains(' '), "{stamp}");
        assert!(stamp.len() >= 17 && stamp.ends_with('Z'), "要带上日期时间和 UTC 标记: {stamp}");
    }
}
