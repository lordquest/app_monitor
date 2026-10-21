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
    /// True when tracks came from the shipped default paste (first run).
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
        write_config(&self.config);
    }

    pub fn dir(&self) -> PathBuf {
        data_dir().clone()
    }
}
