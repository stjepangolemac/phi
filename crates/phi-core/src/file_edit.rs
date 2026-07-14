use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EditTarget {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EditPreparation {
    pub plan: serde_json::Value,
    pub targets: Vec<EditTarget>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileSnapshot {
    pub path: String,
    pub exists: bool,
    pub content: String,
    pub revision: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum FileChange {
    Create {
        path: String,
        content: String,
    },
    Replace {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Move {
        path: String,
        destination: String,
        content: String,
    },
}

pub fn snapshots(workspace: &Path, targets: &[EditTarget]) -> Result<Vec<FileSnapshot>> {
    let root = fs::canonicalize(workspace)?;
    let mut seen = BTreeSet::new();
    targets
        .iter()
        .map(|target| {
            if !seen.insert(target.path.clone()) {
                bail!("duplicate file edit target: {}", target.path);
            }
            let path = resolve(&root, &target.path)?;
            if path.exists() {
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!("file edit target is not a regular file: {}", target.path);
                }
                let bytes = fs::read(&path)?;
                Ok(FileSnapshot {
                    path: target.path.clone(),
                    exists: true,
                    content: String::from_utf8(bytes.clone())
                        .with_context(|| format!("file is not UTF-8: {}", target.path))?,
                    revision: revision(&bytes),
                })
            } else {
                let parent = path.parent().context("file edit target has no parent")?;
                let parent = fs::canonicalize(parent)?;
                if !parent.starts_with(&root) {
                    bail!("file edit target is outside workspace: {}", target.path);
                }
                Ok(FileSnapshot {
                    path: target.path.clone(),
                    exists: false,
                    content: String::new(),
                    revision: String::new(),
                })
            }
        })
        .collect()
}

pub fn apply(workspace: &Path, snapshots: &[FileSnapshot], changes: &[FileChange]) -> Result<()> {
    let root = fs::canonicalize(workspace)?;
    let originals = snapshots
        .iter()
        .map(|snapshot| (snapshot.path.as_str(), snapshot))
        .collect::<BTreeMap<_, _>>();
    let mut changed = BTreeSet::new();

    for change in changes {
        let paths: Vec<&str> = match change {
            FileChange::Create { path, .. }
            | FileChange::Replace { path, .. }
            | FileChange::Delete { path } => vec![path],
            FileChange::Move {
                path, destination, ..
            } => vec![path, destination],
        };
        for path in paths {
            if !changed.insert(path) {
                bail!("file is changed more than once: {path}");
            }
            if !originals.contains_key(path) {
                bail!("file change was not declared during preparation: {path}");
            }
        }
    }

    for snapshot in snapshots {
        let path = resolve(&root, &snapshot.path)?;
        if snapshot.exists {
            let bytes = fs::read(&path)
                .with_context(|| format!("file changed after preparation: {}", snapshot.path))?;
            if revision(&bytes) != snapshot.revision {
                bail!("stale file revision: {}", snapshot.path);
            }
        } else if path.exists() {
            bail!("file was created after preparation: {}", snapshot.path);
        }
    }

    for change in changes {
        match change {
            FileChange::Create { path, .. } if originals[path.as_str()].exists => {
                bail!("create target already exists: {path}");
            }
            FileChange::Replace { path, .. } if !originals[path.as_str()].exists => {
                bail!("replace target does not exist: {path}");
            }
            FileChange::Delete { path } if !originals[path.as_str()].exists => {
                bail!("delete target does not exist: {path}");
            }
            FileChange::Move {
                path, destination, ..
            } if !originals[path.as_str()].exists || originals[destination.as_str()].exists => {
                bail!("move requires an existing source and absent destination");
            }
            _ => {}
        }
    }

    for change in changes {
        match change {
            FileChange::Create { path, content } => {
                persist(&resolve(&root, path)?, content, None, false)?;
            }
            FileChange::Replace { path, content } => {
                let target = resolve(&root, path)?;
                let permissions = fs::metadata(&target)?.permissions();
                persist(&target, content, Some(permissions), true)?;
            }
            FileChange::Delete { path } => {
                fs::remove_file(resolve(&root, path)?)?;
            }
            FileChange::Move {
                path,
                destination,
                content,
            } => {
                let source_path = resolve(&root, path)?;
                let permissions = fs::metadata(&source_path)?.permissions();
                persist(
                    &resolve(&root, destination)?,
                    content,
                    Some(permissions),
                    false,
                )?;
                fs::remove_file(source_path)?;
            }
        }
    }
    Ok(())
}

fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("invalid workspace-relative path: {}", relative.display());
    }
    Ok(root.join(relative))
}

fn persist(
    path: &Path,
    content: &str,
    permissions: Option<fs::Permissions>,
    overwrite: bool,
) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content.as_bytes())?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions)?;
    }
    if overwrite {
        temp.persist(path).map_err(|error| error.error)?;
    } else {
        temp.persist_noclobber(path).map_err(|error| error.error)?;
    }
    Ok(())
}

fn revision(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_declared_changes_and_rejects_stale_files() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("old.txt"), "old\n").unwrap();
        let targets = vec![
            EditTarget {
                path: "old.txt".into(),
            },
            EditTarget {
                path: "new.txt".into(),
            },
        ];
        let snapshots = snapshots(workspace.path(), &targets).unwrap();
        apply(
            workspace.path(),
            &snapshots,
            &[
                FileChange::Replace {
                    path: "old.txt".into(),
                    content: "changed\n".into(),
                },
                FileChange::Create {
                    path: "new.txt".into(),
                    content: "new\n".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("old.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("new.txt")).unwrap(),
            "new\n"
        );

        assert!(
            apply(workspace.path(), &snapshots, &[])
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            snapshots(
                workspace.path(),
                &[EditTarget {
                    path: "../outside".into()
                }]
            )
            .is_err()
        );
    }
}
