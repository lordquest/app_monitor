//! JSON persistence next to the executable (portable) with a per-user fallback.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::model::{Config, DownloadRecord, Track};
use crate::parse;

fn data_dir() -> &'static PathBuf {
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

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let path = data_dir().join(name);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json<T: serde::Serialize>(name: &str, value: &T) {
    let path = data_dir().join(name);
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(value) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub struct Store {
    pub config: Config,
    pub tracks: Vec<Track>,
    pub history: Vec<DownloadRecord>,
    /// True when tracks came from the shipped default paste (first run).
    #[allow(dead_code)]
    pub seeded: bool,
}

impl Store {
    pub fn load() -> Self {
        let raw_config = read_json::<Config>("config.json");
        let first_run = raw_config.is_none();
        let config = raw_config.unwrap_or_default();
        let (tracks, seeded) = match read_json::<Vec<Track>>("tracks.json") {
            Some(t) if !t.is_empty() => (t, false),
            _ => (parse::parse_input(parse::DEFAULT_INPUT).tracks, true),
        };
        let mut history = read_json::<Vec<DownloadRecord>>("history.json").unwrap_or_default();
        history.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        history.truncate(500);
        let store = Self {
            config,
            tracks,
            history,
            seeded,
        };
        // Materialise the defaults so the portable data folder is self-explaining.
        if first_run {
            store.save_config();
        }
        store
    }

    pub fn save_config(&self) {
        write_json("config.json", &self.config);
    }

    pub fn save_tracks(&self) {
        write_json("tracks.json", &self.tracks);
    }

    pub fn save_history(&self) {
        write_json("history.json", &self.history);
    }

    pub fn dir(&self) -> PathBuf {
        data_dir().clone()
    }
}
