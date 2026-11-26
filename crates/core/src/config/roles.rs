//! `modelRoleStorage`: where model selector role assignments are saved, and where a project reads them.

use crate::config::provider::ModelRoleStorage;
use crate::config::settings;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The config file role assignments live in for this storage choice.
pub fn file(cwd: &Path, storage: ModelRoleStorage) -> PathBuf {
    match storage {
        ModelRoleStorage::Global => crate::home().join("config.toml"),
        ModelRoleStorage::Project => cwd.join(".greencode/config.toml"),
    }
}

/// Sets (or, with `None`, removes) one role in the file `storage` names.
pub fn save(cwd: &Path, storage: ModelRoleStorage, role: &str, spec: Option<&str>) -> Result<PathBuf> {
    let path = file(cwd, storage);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let values: Vec<String> = spec.map(str::to_string).into_iter().collect();
    let key = format!("model_roles.{role}");
    let out = match spec {
        Some(_) => settings::set_text(&text, &key, &values)?,
        None => settings::reset_text(&text, &key)?.unwrap_or(text),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    settings::write_atomic(&path, &out)?;
    Ok(path)
}

/// Roles a project config assigns; empty when the file is missing or `storage` is global.
pub fn project_roles(cwd: &Path) -> std::collections::BTreeMap<String, String> {
    let path = file(cwd, ModelRoleStorage::Project);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<crate::config::Config>(&text).ok())
        .map(|cfg| cfg.model_roles)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_storage_writes_and_reads_project_roles() {
        let dir = std::env::temp_dir().join(format!("gc-roles-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = save(&dir, ModelRoleStorage::Project, "smol", Some("openai/gpt-6-mini")).unwrap();
        assert_eq!(saved, dir.join(".greencode/config.toml"));
        assert_eq!(project_roles(&dir)["smol"], "openai/gpt-6-mini");
        save(&dir, ModelRoleStorage::Project, "smol", None).unwrap();
        assert!(project_roles(&dir).is_empty());
        // A global assignment is not visible as a project role.
        assert!(project_roles(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
