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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SessionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_profile: Option<String>,
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
        Self::create_composed_with_id(
            root,
            &id,
            config,
            plugins,
            skill_plugins,
            &Default::default(),
        )
    }

    pub fn create_composed_with_metadata(
        root: &Path,
        config: &Path,
        plugins: &[PluginSource],
        skill_plugins: &[PluginSource],
        metadata: &SessionMetadata,
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        Self::create_composed_with_id(root, &id, config, plugins, skill_plugins, metadata)
    }

    pub fn create_composed_with_id(
        root: &Path,
        id: &str,
        config: &Path,
        plugins: &[PluginSource],
        skill_plugins: &[PluginSource],
        metadata: &SessionMetadata,
    ) -> Result<Self> {
        let session = Self::at(root, id)?;
        fs::create_dir_all(root)?;
        fs::create_dir(&session.dir)
            .with_context(|| format!("create session directory: {}", session.dir.display()))?;
        let result = (|| -> Result<()> {
            let snapshot = snapshot_composition(&session.dir, config, plugins, skill_plugins)?;
            crate::write_json_atomic(
                &session.dir.join("meta.json"),
                &serde_json::json!({
                    "id": id,
                    "created_at": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                    "config": config.display().to_string(),
                    "plugins": snapshot.plugins.iter().map(|plugin| &plugin.name).collect::<Vec<_>>(),
                    "workspace": metadata.workspace,
                    "parent_session_id": metadata.parent_session_id,
                    "workflow_task_id": metadata.workflow_task_id,
                    "agent_label": metadata.agent_label,
                    "branch": metadata.branch,
                    "worktree_path": metadata.worktree_path,
                    "model": metadata.model,
                    "reasoning": metadata.reasoning,
                    "service_tier": metadata.service_tier,
                    "timeout_ms": metadata.timeout_ms,
                    "capability_profile": metadata.capability_profile,
                }),
                AtomicWriteMode::Overwrite,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&session.dir);
            return Err(error);
        }
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

    pub fn create_plan(&self, name: &str, content: &str) -> Result<PathBuf> {
        let name = plan_slug(name)?;
        let plans = self.dir.join("plans");
        fs::create_dir_all(&plans)?;
        let mut number = next_plan_number(&plans)?;
        loop {
            let path = plans.join(format!("{number:04}-{name}.md"));
            let reservation = plans.join(format!("{number:04}-.allocating"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&reservation)
            {
                Ok(mut file) => {
                    if let Err(error) = (|| -> std::io::Result<()> {
                        file.write_all(content.as_bytes())?;
                        file.sync_all()?;
                        drop(file);
                        fs::rename(&reservation, &path)
                    })() {
                        let _ = fs::remove_file(&reservation);
                        return Err(error.into());
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    number = number.checked_add(1).context("plan number overflow")?;
                }
                Err(error) => return Err(error.into()),
            }
        }
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

fn plan_slug(name: &str) -> Result<String> {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
        if slug.len() >= 64 {
            break;
        }
    }
    if slug.is_empty() {
        anyhow::bail!("plan name must contain an ASCII letter or number");
    }
    Ok(slug)
}

fn next_plan_number(plans: &Path) -> Result<u64> {
    let mut maximum = 0;
    for entry in fs::read_dir(plans)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((prefix, _)) = name.split_once('-') else {
            continue;
        };
        if prefix.len() >= 4 && prefix.bytes().all(|byte| byte.is_ascii_digit()) {
            maximum = maximum.max(prefix.parse::<u64>()?);
        }
    }
    maximum.checked_add(1).context("plan number overflow")
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
    use std::{collections::BTreeSet, sync::Arc, thread};

    use super::*;

    fn empty_session(root: &Path) -> Session {
        fs::write(root.join("config.scm"), "config").unwrap();
        Session::create_composed(&root.join("sessions"), &root.join("config.scm"), &[], &[])
            .unwrap()
    }

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
    fn stores_session_relationship_metadata() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let workspace = root.path().join("workspace");
        let worktree = root.path().join("worktree");
        let session = Session::create_composed_with_id(
            &root.path().join("sessions"),
            id,
            &root.path().join("config.scm"),
            &[],
            &[],
            &SessionMetadata {
                workspace: Some(workspace.clone()),
                parent_session_id: Some("22222222-2222-4222-8222-222222222222".into()),
                workflow_task_id: Some("33333333-3333-4333-8333-333333333333".into()),
                agent_label: Some("review".into()),
                branch: Some("phi/task/review".into()),
                worktree_path: Some(worktree.clone()),
                model: Some("test/model".into()),
                reasoning: Some("high".into()),
                service_tier: Some("priority".into()),
                timeout_ms: Some(30_000),
                capability_profile: Some("read-only".into()),
            },
        )
        .unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(session.dir().join("meta.json")).unwrap()).unwrap();
        assert_eq!(metadata["id"], id);
        assert_eq!(metadata["workspace"], workspace.to_string_lossy().as_ref());
        assert_eq!(metadata["agent_label"], "review");
        assert_eq!(metadata["branch"], "phi/task/review");
        assert_eq!(metadata["model"], "test/model");
        assert_eq!(metadata["reasoning"], "high");
        assert_eq!(metadata["service_tier"], "priority");
        assert_eq!(metadata["timeout_ms"], 30_000);
        assert_eq!(metadata["capability_profile"], "read-only");
        assert_eq!(
            metadata["worktree_path"],
            worktree.to_string_lossy().as_ref()
        );
    }

    #[test]
    fn explicit_id_collision_preserves_existing_session() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("config.scm"), "config").unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let sessions = root.path().join("sessions");
        let first = Session::create_composed_with_id(
            &sessions,
            id,
            &root.path().join("config.scm"),
            &[],
            &[],
            &Default::default(),
        )
        .unwrap();
        fs::write(first.dir().join("sentinel"), "keep").unwrap();

        assert!(
            Session::create_composed_with_id(
                &sessions,
                id,
                &root.path().join("config.scm"),
                &[],
                &[],
                &Default::default(),
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(first.dir().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn creates_numbered_plans_concurrently() {
        let root = tempfile::tempdir().unwrap();
        let session = empty_session(root.path());
        let sessions = Arc::new(root.path().join("sessions"));
        let id = Arc::new(session.id().to_owned());
        let workers = (0..16)
            .map(|index| {
                let sessions = Arc::clone(&sessions);
                let id = Arc::clone(&id);
                thread::spawn(move || {
                    Session::open(&sessions, &id)
                        .unwrap()
                        .create_plan(&format!("Plan {index}"), &format!("content {index}"))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let paths = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), 16);
        let numbers = names
            .iter()
            .map(|name| name.split_once('-').unwrap().0.parse::<u64>().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(numbers, (1..=16).collect());
        assert!(paths.iter().all(|path| path.is_file()));
        assert!(
            fs::read_dir(session.dir().join("plans"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with("-.allocating"))
        );
    }

    #[test]
    fn plan_names_are_sanitized_and_must_have_ascii_alphanumerics() {
        let root = tempfile::tempdir().unwrap();
        let session = empty_session(root.path());
        let path = session
            .create_plan("  Session storage!  ", "content")
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "0001-session-storage.md");
        assert!(session.create_plan("---", "content").is_err());
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
