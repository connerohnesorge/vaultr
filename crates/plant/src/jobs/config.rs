use std::collections::HashMap;
use std::path::Path;

/// Config keys read from real env first, then the vault/.env fallback. The
/// private map never mutates process environment, so fallback values cannot
/// leak into job children.
pub(crate) struct Cfg(HashMap<String, String>);

impl Cfg {
    pub(crate) fn load(vault_sessions: &Path) -> Self {
        let mut map = HashMap::new();
        let text = vault_sessions
            .parent()
            .map(|vault| vault.join(".env"))
            .and_then(|file| std::fs::read_to_string(file).ok())
            .unwrap_or_default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        Self(map)
    }

    pub(crate) fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().or_else(|| self.0.get(key).cloned())
    }

    #[cfg(test)]
    pub(super) fn from_map(map: HashMap<String, String>) -> Self {
        Self(map)
    }
}
