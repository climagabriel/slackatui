//! Local conversation visibility preferences and their menu; no Slack mutations.
use crate::archive::{Conv, Kind};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const CATEGORIES: [&str; 6] = [
    "Public channels",
    "Private channels",
    "Direct messages",
    "Group DMs",
    "Archived conversations",
    "Muted",
];
const KEYS: [&str; 6] = ["public", "private", "dm", "group", "archived", "muted"];

pub fn category(c: &Conv) -> usize {
    if c.archived {
        return 4;
    }
    match c.kind {
        Kind::Channel => 0,
        Kind::Private => 1,
        Kind::Im => 2,
        Kind::Mpim => 3,
    }
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Settings {
    pub hidden: BTreeSet<String>,
    pub overrides: BTreeMap<String, bool>,
}

impl Settings {
    pub fn visible(&self, c: &Conv) -> bool {
        self.visible_id(&c.id, category(c), c.muted)
    }
    pub fn visible_id(&self, id: &str, category: usize, muted: bool) -> bool {
        self.overrides.get(id).copied().unwrap_or(
            !self.hidden.contains(KEYS[category]) && !(muted && self.hidden.contains("muted")),
        )
    }
    pub fn toggle_category(&mut self, category: usize) {
        if !self.hidden.remove(KEYS[category]) {
            self.hidden.insert(KEYS[category].into());
        }
    }
    pub fn cycle(&mut self, id: &str) {
        match self.overrides.get(id) {
            None => {
                self.overrides.insert(id.into(), true);
            }
            Some(true) => {
                self.overrides.insert(id.into(), false);
            }
            Some(false) => {
                self.overrides.remove(id);
            }
        }
    }
    pub fn load(path: Option<&Path>, workspace: &str) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let doc = read(path)?;
        let Some(v) = doc.get(workspace) else {
            return Ok(Self::default());
        };
        let hidden = v["hidden_categories"]
            .as_array()
            .ok_or("invalid hidden_categories")?;
        let overrides = v["overrides"].as_object().ok_or("invalid overrides")?;
        let mut settings = Self::default();
        for value in hidden {
            let key = value
                .as_str()
                .filter(|k| KEYS.contains(k))
                .ok_or("invalid category")?;
            settings.hidden.insert(key.into());
        }
        for (id, value) in overrides {
            settings.overrides.insert(
                id.clone(),
                value.as_bool().ok_or("invalid visibility override")?,
            );
        }
        Ok(settings)
    }
    pub fn save(&self, path: Option<&Path>, workspace: &str) -> Result<(), String> {
        let Some(path) = path else {
            return Ok(());
        };
        let mut doc = read(path)?;
        doc.as_object_mut()
            .ok_or("invalid settings object")?
            .insert(
                workspace.into(),
                json!({"hidden_categories":self.hidden, "overrides":self.overrides}),
            );
        save(path, &doc).map_err(|e| e.to_string())
    }
}

fn read(path: &Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let doc: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if !doc.is_object() {
                return Err("invalid settings object".into());
            }
            Ok(doc)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.to_string()),
    }
}

fn save(path: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let part = path.with_extension(format!("{}.{}.tmp", std::process::id(), stamp));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&part)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        std::fs::rename(&part, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}

pub struct Entry {
    pub id: String,
    pub name: String,
    pub category: usize,
    pub muted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_muted_and_individual_exceptions() {
        let mut s = Settings::default();
        for cat in 0..5 {
            assert!(s.visible_id("C1", cat, true));
        }
        s.toggle_category(5);
        assert!(!s.visible_id("C1", 0, true));
        assert!(s.visible_id("C1", 0, false));
        s.toggle_category(0);
        assert!(!s.visible_id("C1", 0, false));
        s.cycle("C1");
        assert!(s.visible_id("C1", 0, true));
        s.cycle("C1");
        assert!(!s.visible_id("C1", 1, false));
        s.cycle("C1");
        assert!(s.visible_id("C1", 1, false));
        assert!(!s.visible_id("C2", 0, false));
    }

    #[test]
    fn persists_per_workspace_and_preserves_corrupt_files() {
        use std::os::unix::fs::PermissionsExt;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("slack-pane-test-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        let mut a = Settings::default();
        a.toggle_category(5);
        a.cycle("C1");
        a.save(Some(&path), "workspace-a").unwrap();
        Settings::default()
            .save(Some(&path), "workspace-b")
            .unwrap();
        assert_eq!(Settings::load(Some(&path), "workspace-a").unwrap(), a);
        assert_eq!(
            Settings::load(Some(&path), "workspace-b").unwrap(),
            Settings::default()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, "broken").unwrap();
        assert!(a.save(Some(&path), "workspace-a").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
pub struct Menu {
    pub settings: Settings,
    pub entries: Vec<Entry>,
    pub query: String,
    pub cursor: usize,
}
impl Menu {
    pub fn new(settings: Settings, convs: &[Conv]) -> Self {
        let mut entries = BTreeMap::new();
        for c in convs {
            if !entries.contains_key(&c.id) || !c.live_only {
                entries.insert(
                    c.id.clone(),
                    Entry {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        category: category(c),
                        muted: c.muted,
                    },
                );
            }
        }
        let mut entries: Vec<_> = entries.into_values().collect();
        entries.sort_by_key(|e| e.name.to_lowercase());
        Self {
            settings,
            entries,
            query: String::new(),
            cursor: 0,
        }
    }
    pub fn matching(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }
    pub fn rows(&self) -> Vec<String> {
        let mut rows = vec!["Reset to default — show everything".into()];
        for (i, label) in CATEGORIES.iter().enumerate() {
            rows.push(format!(
                "[{}] {label}",
                if self.settings.hidden.contains(KEYS[i]) {
                    " "
                } else {
                    "x"
                }
            ));
        }
        for i in self.matching() {
            let e = &self.entries[i];
            let choice = match self.settings.overrides.get(&e.id) {
                None => "category",
                Some(true) => "show",
                Some(false) => "hide",
            };
            rows.push(format!(
                "[{}] {:8} {} · {}{}",
                if self.settings.visible_id(&e.id, e.category, e.muted) {
                    "x"
                } else {
                    " "
                },
                choice,
                e.name,
                e.id,
                if e.muted { " · muted" } else { "" }
            ));
        }
        rows
    }
    pub fn toggle(&mut self) {
        match self.cursor {
            0 => self.settings = Settings::default(),
            1..=6 => self.settings.toggle_category(self.cursor - 1),
            _ => {
                if let Some(&i) = self.matching().get(self.cursor - 7) {
                    self.settings.cycle(&self.entries[i].id);
                }
            }
        }
    }
}
