//! Installed-apps registry (store.json at WINapk home).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub pkg: String,
    pub label: String,
    pub version_name: Option<String>,
    pub version_code: Option<i64>,
    pub apk_path: String,
    pub icon: String,
    pub shortcut: Option<String>,
    pub installed_at: u64,
    pub aot_done: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub apps: Vec<AppRecord>,
}

impl Store {
    /// Corrupt file → rename to .bak (preserve evidence) and start
    /// fresh — a silent default would make the user's apps "vanish"
    /// with no trace of why.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(store) => store,
                Err(e) => {
                    let bak = path.with_extension("json.bak");
                    let _ = std::fs::rename(path, &bak);
                    winapk_core::warn!(
                        "store.json corrupt ({e}) — moved to {}",
                        bak.display()
                    );
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic save: tmp+rename so a crash mid-write can't leave a
    /// truncated store.json that reads back as "no apps installed".
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn upsert(&mut self, rec: AppRecord) {
        if let Some(x) = self.apps.iter_mut().find(|a| a.pkg == rec.pkg) {
            *x = rec;
        } else {
            self.apps.push(rec);
        }
    }

    pub fn find(&self, pkg: &str) -> Option<&AppRecord> {
        self.apps.iter().find(|a| a.pkg == pkg)
    }
}
