use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
}

#[derive(Deserialize, Serialize)]
struct CompositionSnapshot {
    plugins: Vec<SnapshotPlugin>,
}

#[derive(Deserialize, Serialize)]
struct SnapshotPlugin {
    name: String,
    directory: String,
    entrypoint: String,
}

impl Session {
    pub fn create_composed(root: &Path, config: &Path, plugins: &[PluginSource]) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let session = Self::at(root, &id)?;
        fs::create_dir_all(&session.dir)?;
        fs::copy(config, session.dir.join("config.scm"))?;
        let mut snapshot = CompositionSnapshot {
            plugins: Vec::new(),
        };
        for plugin in plugins {
            let directory = plugin.name.clone();
            let target = session.dir.join("plugins").join(&directory);
            copy_tree(&plugin.root, &target)?;
            let entrypoint = plugin
                .entrypoint
                .strip_prefix(&plugin.root)
                .context("plugin entrypoint is outside its package")?;
            snapshot.plugins.push(SnapshotPlugin {
                name: plugin.name.clone(),
                directory,
                entrypoint: entrypoint.to_string_lossy().into(),
            });
        }
        write_atomic(
            &session.dir.join("composition.json"),
            &serde_json::to_vec_pretty(&snapshot)?,
        )?;
        write_atomic(
            &session.dir.join("meta.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "id": id,
                "created_at": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                "config": config.display().to_string(),
                "plugins": snapshot.plugins.iter().map(|plugin| &plugin.name).collect::<Vec<_>>(),
            }))?,
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

    pub fn append(&self, value: &impl Serialize) -> Result<()> {
        append(&self.dir.join("events.jsonl"), value)
    }

    pub fn save_state(&self, state: &str) -> Result<()> {
        write_atomic(&self.dir.join("state.json"), state.as_bytes())
    }

    pub fn load_state(&self) -> Result<String> {
        fs::read_to_string(self.dir.join("state.json")).context("session has no saved state")
    }

    pub fn replace_composition(&self, config: &Path, plugins: &[PluginSource]) -> Result<()> {
        let plugins_dir = self.dir.join("plugins");
        if plugins_dir.exists() {
            fs::remove_dir_all(&plugins_dir)?;
        }
        fs::copy(config, self.dir.join("config.scm"))?;
        let snapshot = snapshot_plugins(&plugins_dir, plugins)?;
        write_atomic(
            &self.dir.join("composition.json"),
            &serde_json::to_vec_pretty(&snapshot)?,
        )
    }

    pub fn composed_sources(&self) -> Result<Option<ComposedSources>> {
        let path = self.dir.join("composition.json");
        if !path.is_file() {
            return Ok(None);
        }
        let snapshot: CompositionSnapshot = serde_json::from_slice(&fs::read(path)?)?;
        let plugins = snapshot
            .plugins
            .into_iter()
            .map(|plugin| {
                let root = self.dir.join("plugins").join(plugin.directory);
                let entrypoint = root.join(plugin.entrypoint);
                if !entrypoint.is_file() {
                    anyhow::bail!("session plugin snapshot is incomplete: {}", plugin.name);
                }
                Ok(PluginSource {
                    name: plugin.name,
                    root,
                    entrypoint,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(ComposedSources {
            config: self.dir.join("config.scm"),
            plugins,
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

fn snapshot_plugins(root: &Path, plugins: &[PluginSource]) -> Result<CompositionSnapshot> {
    let mut snapshot = CompositionSnapshot {
        plugins: Vec::new(),
    };
    for plugin in plugins {
        let directory = plugin.name.clone();
        copy_tree(&plugin.root, &root.join(&directory))?;
        let entrypoint = plugin
            .entrypoint
            .strip_prefix(&plugin.root)
            .context("plugin entrypoint is outside its package")?;
        snapshot.plugins.push(SnapshotPlugin {
            name: plugin.name.clone(),
            directory,
            entrypoint: entrypoint.to_string_lossy().into(),
        });
    }
    Ok(snapshot)
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if kind.is_symlink() {
            anyhow::bail!("plugin packages may not contain symlinks");
        } else if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
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

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content)?;
    temp.persist(path).map_err(|error| error.error)?;
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
            Session::create_composed(root.path(), &root.path().join("config.scm"), &[]).unwrap();
        session.save_state("{\"input\":[]}").unwrap();
        let resumed = Session::open(root.path(), session.id()).unwrap();
        assert_eq!(resumed.load_state().unwrap(), "{\"input\":[]}");
        assert_eq!(
            fs::read_to_string(resumed.composed_sources().unwrap().unwrap().config).unwrap(),
            "config"
        );
    }

    #[test]
    fn snapshots_complete_composition() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugin");
        fs::create_dir(&plugin).unwrap();
        fs::write(plugin.join("main.scm"), "plugin").unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let session = Session::create_composed(
            &root.path().join("sessions"),
            &root.path().join("config.scm"),
            &[PluginSource {
                name: "example".into(),
                root: plugin.clone(),
                entrypoint: plugin.join("main.scm"),
            }],
        )
        .unwrap();
        let sources = session.composed_sources().unwrap().unwrap();
        assert_eq!(fs::read_to_string(sources.config).unwrap(), "config");
        assert_eq!(
            fs::read_to_string(&sources.plugins[0].entrypoint).unwrap(),
            "plugin"
        );
    }
}
