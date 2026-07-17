use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AtomicWriteMode, SymlinkPolicy, copy_package_tree, write_atomic};

pub struct Session {
    id: String,
    dir: PathBuf,
}

#[derive(Clone)]
pub struct PluginSource {
    pub name: String,
    pub root: PathBuf,
    pub entrypoint: PathBuf,
}

pub struct ComposedSources {
    pub config: PathBuf,
    pub plugins: Vec<PluginSource>,
    pub skill_plugins: Vec<PluginSource>,
}

#[derive(Deserialize, Serialize)]
struct CompositionSnapshot {
    plugins: Vec<SnapshotPlugin>,
    #[serde(default)]
    skill_plugins: Vec<SnapshotPlugin>,
}

#[derive(Deserialize, Serialize)]
struct SnapshotPlugin {
    name: String,
    directory: String,
    entrypoint: String,
}

impl Session {
    pub fn create_composed(
        root: &Path,
        config: &Path,
        plugins: &[PluginSource],
        skill_plugins: &[PluginSource],
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let session = Self::at(root, &id)?;
        fs::create_dir_all(&session.dir)?;
        let snapshot = snapshot_composition(&session.dir, config, plugins, skill_plugins)?;
        crate::write_json_atomic(
            &session.dir.join("meta.json"),
            &serde_json::json!({
                "id": id,
                "created_at": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                "config": config.display().to_string(),
                "plugins": snapshot.plugins.iter().map(|plugin| &plugin.name).collect::<Vec<_>>(),
            }),
            AtomicWriteMode::Overwrite,
        )?;
        Ok(session)
    }

    pub fn open(root: &Path, id: &str) -> Result<Self> {
        let session = Self::at(root, id)?;
        if !session.dir.join("meta.json").is_file() {
            anyhow::bail!("session not found: {id}");
        }
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn append(&self, value: &impl Serialize) -> Result<()> {
        append(&self.dir.join("events.jsonl"), value)
    }

    pub fn save_state(&self, state: &str) -> Result<()> {
        write_atomic(
            &self.dir.join("state.json"),
            state.as_bytes(),
            AtomicWriteMode::Overwrite,
        )
    }

    pub fn load_state(&self) -> Result<String> {
        fs::read_to_string(self.dir.join("state.json")).context("session has no saved state")
    }

    pub fn replace_composition(
        &self,
        config: &Path,
        plugins: &[PluginSource],
        skill_plugins: &[PluginSource],
    ) -> Result<()> {
        snapshot_composition(&self.dir, config, plugins, skill_plugins).map(|_| ())
    }

    pub fn composed_sources(&self) -> Result<Option<ComposedSources>> {
        let path = self.dir.join("composition.json");
        if !path.is_file() {
            return Ok(None);
        }
        let snapshot: CompositionSnapshot = serde_json::from_slice(&fs::read(path)?)?;
        let plugins = snapshot_sources(&self.dir, snapshot.plugins)?;
        let skill_plugins = if snapshot.skill_plugins.is_empty() {
            plugins.clone()
        } else {
            snapshot_sources(&self.dir, snapshot.skill_plugins)?
        };
        Ok(Some(ComposedSources {
            config: self.dir.join("config.scm"),
            plugins,
            skill_plugins,
        }))
    }

    fn at(root: &Path, id: &str) -> Result<Self> {
        let id = Uuid::parse_str(id)?.to_string();
        Ok(Self {
            dir: root.join(id.as_str()),
            id,
        })
    }
}

fn snapshot_composition(
    root: &Path,
    config: &Path,
    plugins: &[PluginSource],
    skill_plugins: &[PluginSource],
) -> Result<CompositionSnapshot> {
    let plugins_dir = root.join("plugins");
    if plugins_dir.exists() {
        fs::remove_dir_all(&plugins_dir)?;
    }
    fs::copy(config, root.join("config.scm"))?;
    let mut snapshot = CompositionSnapshot {
        plugins: Vec::new(),
        skill_plugins: Vec::new(),
    };
    let mut packages = std::collections::BTreeMap::new();
    for plugin in plugins {
        packages.insert(plugin.name.as_str(), plugin);
    }
    for plugin in skill_plugins {
        packages.insert(plugin.name.as_str(), plugin);
    }
    for plugin in packages.into_values() {
        let directory = plugin.name.clone();
        copy_package_tree(
            &plugin.root,
            &plugins_dir.join(&directory),
            SymlinkPolicy::Reject,
        )?;
    }
    for plugin in plugins {
        snapshot.plugins.push(snapshot_plugin(plugin)?);
    }
    for plugin in skill_plugins {
        snapshot.skill_plugins.push(snapshot_plugin(plugin)?);
    }
    crate::write_json_atomic(
        &root.join("composition.json"),
        &snapshot,
        AtomicWriteMode::Overwrite,
    )?;
    Ok(snapshot)
}

fn snapshot_plugin(plugin: &PluginSource) -> Result<SnapshotPlugin> {
    let entrypoint = plugin
        .entrypoint
        .strip_prefix(&plugin.root)
        .context("plugin entrypoint is outside its package")?;
    Ok(SnapshotPlugin {
        name: plugin.name.clone(),
        directory: plugin.name.clone(),
        entrypoint: entrypoint.to_string_lossy().into(),
    })
}

fn snapshot_sources(root: &Path, plugins: Vec<SnapshotPlugin>) -> Result<Vec<PluginSource>> {
    plugins
        .into_iter()
        .map(|plugin| {
            let package = root.join("plugins").join(plugin.directory);
            let entrypoint = package.join(plugin.entrypoint);
            if !entrypoint.is_file() {
                anyhow::bail!("session plugin snapshot is incomplete: {}", plugin.name);
            }
            Ok(PluginSource {
                name: plugin.name,
                root: package,
                entrypoint,
            })
        })
        .collect()
}

pub fn append(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_resumes_state() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let session =
            Session::create_composed(root.path(), &root.path().join("config.scm"), &[], &[])
                .unwrap();
        session.save_state("{\"input\":[]}").unwrap();
        let resumed = Session::open(root.path(), session.id()).unwrap();
        assert_eq!(resumed.load_state().unwrap(), "{\"input\":[]}");
        assert_eq!(
            fs::read_to_string(resumed.composed_sources().unwrap().unwrap().config).unwrap(),
            "config"
        );
    }

    #[test]
    fn creation_and_replacement_snapshot_equivalent_compositions() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugin");
        fs::create_dir(&plugin).unwrap();
        fs::write(plugin.join("plugin.scm"), "plugin").unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let plugins = [PluginSource {
            name: "example".into(),
            root: plugin.clone(),
            entrypoint: plugin.join("plugin.scm"),
        }];
        let created = Session::create_composed(
            &root.path().join("sessions"),
            &root.path().join("config.scm"),
            &plugins,
            &plugins,
        )
        .unwrap();
        let replaced = Session::create_composed(
            &root.path().join("sessions"),
            &root.path().join("config.scm"),
            &[],
            &[],
        )
        .unwrap();
        replaced
            .replace_composition(&root.path().join("config.scm"), &plugins, &plugins)
            .unwrap();

        let created_sources = created.composed_sources().unwrap().unwrap();
        let replaced_sources = replaced.composed_sources().unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(created_sources.config).unwrap(),
            fs::read_to_string(replaced_sources.config).unwrap()
        );
        assert_eq!(
            created_sources.plugins.len(),
            replaced_sources.plugins.len()
        );
        for (created, replaced) in created_sources
            .plugins
            .iter()
            .zip(&replaced_sources.plugins)
        {
            assert_eq!(created.name, replaced.name);
            assert_eq!(
                created.entrypoint.strip_prefix(&created.root).unwrap(),
                replaced.entrypoint.strip_prefix(&replaced.root).unwrap()
            );
            assert_eq!(
                fs::read_to_string(&created.entrypoint).unwrap(),
                fs::read_to_string(&replaced.entrypoint).unwrap()
            );
        }
        assert_eq!(
            fs::read(created.dir().join("composition.json")).unwrap(),
            fs::read(replaced.dir().join("composition.json")).unwrap()
        );
    }

    #[test]
    fn creation_and_replacement_reject_entrypoints_outside_the_package() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugin");
        fs::create_dir(&plugin).unwrap();
        fs::write(root.path().join("outside.scm"), "outside").unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let plugins = [PluginSource {
            name: "example".into(),
            root: plugin,
            entrypoint: root.path().join("outside.scm"),
        }];

        let creation_error = Session::create_composed(
            &root.path().join("created"),
            &root.path().join("config.scm"),
            &plugins,
            &plugins,
        )
        .err()
        .unwrap();
        assert!(
            creation_error
                .to_string()
                .contains("plugin entrypoint is outside its package")
        );

        let replaced = Session::create_composed(
            &root.path().join("replaced"),
            &root.path().join("config.scm"),
            &[],
            &[],
        )
        .unwrap();
        let replacement_error = replaced
            .replace_composition(&root.path().join("config.scm"), &plugins, &plugins)
            .unwrap_err();
        assert!(
            replacement_error
                .to_string()
                .contains("plugin entrypoint is outside its package")
        );
    }
}
