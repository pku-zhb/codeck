use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::client::state_dir;
use crate::model::PreviewVerbosity;

const STATE_VERSION: u32 = 3;

#[derive(Debug, Default, Serialize, Deserialize)]
struct LifecycleData {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    initialized: bool,
    #[serde(default)]
    tracked_sessions: BTreeSet<String>,
    #[serde(default)]
    pinned_sessions: BTreeSet<String>,
    #[serde(default)]
    preview_verbosity: PreviewVerbosity,
}

pub struct LifecycleStore {
    path: PathBuf,
    data: LifecycleData,
    dirty: bool,
}

impl LifecycleStore {
    pub fn load_default() -> Result<Self> {
        Self::load(state_dir()?.join("lifecycle.json"))
    }

    fn load(path: PathBuf) -> Result<Self> {
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<LifecycleData>(&bytes)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LifecycleData {
                version: STATE_VERSION,
                ..LifecycleData::default()
            },
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        Ok(Self {
            path,
            data,
            dirty: false,
        })
    }

    pub fn is_initialized(&self) -> bool {
        self.data.initialized
    }

    pub fn contains(&self, thread_id: &str) -> bool {
        self.data.tracked_sessions.contains(thread_id)
    }

    pub fn is_pinned(&self, thread_id: &str) -> bool {
        self.data.pinned_sessions.contains(thread_id)
    }

    pub fn preview_verbosity(&self) -> PreviewVerbosity {
        self.data.preview_verbosity
    }

    pub fn set_preview_verbosity(&mut self, verbosity: PreviewVerbosity) {
        if self.data.preview_verbosity != verbosity {
            self.data.preview_verbosity = verbosity;
            self.dirty = true;
        }
    }

    pub fn track(&mut self, thread_id: impl Into<String>) {
        if self.data.tracked_sessions.insert(thread_id.into()) {
            self.dirty = true;
        }
    }

    pub fn dismiss(&mut self, thread_id: &str) {
        let removed_tracked = self.data.tracked_sessions.remove(thread_id);
        let removed_pinned = self.data.pinned_sessions.remove(thread_id);
        if removed_tracked || removed_pinned {
            self.dirty = true;
        }
    }

    pub fn toggle_pin(&mut self, thread_id: &str) -> bool {
        if self.data.pinned_sessions.remove(thread_id) {
            self.dirty = true;
            false
        } else {
            self.data.pinned_sessions.insert(thread_id.to_string());
            self.data.tracked_sessions.insert(thread_id.to_string());
            self.dirty = true;
            true
        }
    }

    pub fn finish_initial_scan(&mut self, seen: &BTreeSet<String>) {
        if !self.data.initialized {
            self.data.initialized = true;
            self.dirty = true;
        }
        let before = self.data.tracked_sessions.len();
        self.data
            .tracked_sessions
            .retain(|thread_id| seen.contains(thread_id));
        let pinned_before = self.data.pinned_sessions.len();
        self.data
            .pinned_sessions
            .retain(|thread_id| seen.contains(thread_id));
        self.dirty |= before != self.data.tracked_sessions.len()
            || pinned_before != self.data.pinned_sessions.len();
    }

    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        self.data.version = STATE_VERSION;
        let bytes = serde_json::to_vec_pretty(&self.data).context("encode lifecycle state")?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }

    #[cfg(test)]
    pub fn for_test(path: PathBuf) -> Self {
        Self::load(path).expect("test lifecycle state")
    }
}

fn state_version() -> u32 {
    STATE_VERSION
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lifecycle.json");
    path.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_state_persists_tracking_and_dismissal() {
        let path = std::env::temp_dir().join(format!(
            "codeck-lifecycle-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_file(&path);

        let mut store = LifecycleStore::load(path.clone()).expect("load empty state");
        assert!(!store.is_initialized());
        store.track("active-thread");
        assert!(store.toggle_pin("active-thread"));
        store.finish_initial_scan(&BTreeSet::from(["active-thread".to_string()]));
        store.save().expect("save state");

        let mut restored = LifecycleStore::load(path.clone()).expect("restore state");
        assert!(restored.is_initialized());
        assert!(restored.contains("active-thread"));
        assert!(restored.is_pinned("active-thread"));
        restored.dismiss("active-thread");
        restored.save().expect("save dismissal");

        let restored = LifecycleStore::load(path.clone()).expect("restore dismissal");
        assert!(!restored.contains("active-thread"));
        assert!(!restored.is_pinned("active-thread"));
        fs::remove_file(path).expect("remove state");
    }
}
