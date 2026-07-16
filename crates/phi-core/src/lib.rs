use std::{collections::HashSet, fs, io::Write, path::Path};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub mod capability;
pub mod file_edit;
pub mod home;
pub mod http;
pub mod permissions;
pub mod plugin;
pub mod process;
pub mod session;
pub mod skill;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicWriteMode {
    Overwrite,
    NoClobber,
}

pub fn write_atomic(path: &Path, content: &[u8], mode: AtomicWriteMode) -> Result<()> {
    write_atomic_with_permissions(path, content, mode, None)
}

pub fn write_atomic_with_permissions(
    path: &Path,
    content: &[u8],
    mode: AtomicWriteMode,
    permissions: Option<fs::Permissions>,
) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let permissions = match (permissions, mode) {
        (Some(permissions), _) => Some(permissions),
        (None, AtomicWriteMode::Overwrite) => match fs::metadata(path) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        },
        (None, AtomicWriteMode::NoClobber) => None,
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(content)?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    match mode {
        AtomicWriteMode::Overwrite => temporary.persist(path),
        AtomicWriteMode::NoClobber => temporary.persist_noclobber(path),
    }
    .map_err(|error| error.error)?;
    Ok(())
}

pub fn write_json_atomic(path: &Path, value: &impl Serialize, mode: AtomicWriteMode) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?, mode)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkPolicy {
    Reject,
    Follow,
    Preserve,
}

pub fn copy_package_tree(source: &Path, target: &Path, symlinks: SymlinkPolicy) -> Result<()> {
    let mut ancestors = HashSet::new();
    copy_package_directory(source, target, symlinks, &mut ancestors)
}

fn copy_package_directory(
    source: &Path,
    target: &Path,
    symlinks: SymlinkPolicy,
    ancestors: &mut HashSet<std::path::PathBuf>,
) -> Result<()> {
    let canonical = source
        .canonicalize()
        .with_context(|| format!("resolve package directory {}", source.display()))?;
    if !ancestors.insert(canonical.clone()) {
        bail!(
            "package tree contains a symlink cycle at {}",
            source.display()
        );
    }

    let result = (|| {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source = entry.path();
            let target = target.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                copy_package_directory(&source, &target, symlinks, ancestors)?;
            } else if kind.is_file() {
                fs::copy(&source, &target)?;
            } else if kind.is_symlink() {
                copy_package_symlink(&source, &target, symlinks, ancestors)?;
            } else {
                bail!(
                    "package tree contains an unsupported file: {}",
                    source.display()
                );
            }
        }
        Ok(())
    })();

    ancestors.remove(&canonical);
    result
}

fn copy_package_symlink(
    source: &Path,
    target: &Path,
    symlinks: SymlinkPolicy,
    ancestors: &mut HashSet<std::path::PathBuf>,
) -> Result<()> {
    match symlinks {
        SymlinkPolicy::Reject => bail!(
            "package trees may not contain symlinks: {}",
            source.display()
        ),
        SymlinkPolicy::Follow => {
            let metadata = fs::metadata(source)
                .with_context(|| format!("follow package symlink {}", source.display()))?;
            if metadata.is_dir() {
                copy_package_directory(source, target, symlinks, ancestors)
            } else if metadata.is_file() {
                fs::copy(source, target)?;
                Ok(())
            } else {
                bail!(
                    "package symlink has an unsupported target: {}",
                    source.display()
                )
            }
        }
        SymlinkPolicy::Preserve => preserve_symlink(source, target),
    }
}

#[cfg(unix)]
fn preserve_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    Ok(())
}

#[cfg(windows)]
fn preserve_symlink(source: &Path, target: &Path) -> Result<()> {
    let link = fs::read_link(source)?;
    let metadata = fs::metadata(source)
        .with_context(|| format!("inspect package symlink {}", source.display()))?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(link, target)?;
    } else {
        std::os::windows::fs::symlink_file(link, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn atomic_write_replaces_complete_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, "old").unwrap();

        write_json_atomic(
            &path,
            &serde_json::json!({ "value": "new" }),
            AtomicWriteMode::Overwrite,
        )
        .unwrap();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).unwrap(),
            serde_json::json!({ "value": "new" })
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_replaced_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("secret.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        write_atomic(&path, b"new", AtomicWriteMode::Overwrite).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn atomic_write_noclobber_preserves_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.txt");
        write_atomic(&path, b"original", AtomicWriteMode::NoClobber).unwrap();

        assert!(write_atomic(&path, b"replacement", AtomicWriteMode::NoClobber).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn concurrent_atomic_writers_publish_one_complete_value() {
        let temp = tempfile::tempdir().unwrap();
        let path = Arc::new(temp.path().join("state.json"));
        let barrier = Arc::new(Barrier::new(8));
        let contents = (0..8)
            .map(|index| format!("writer-{index}:{}", "x".repeat(64 * 1024)))
            .collect::<Vec<_>>();
        let writers = contents
            .iter()
            .cloned()
            .map(|content| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    write_atomic(&path, content.as_bytes(), AtomicWriteMode::Overwrite).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().unwrap();
        }

        let result = fs::read_to_string(path.as_ref()).unwrap();
        assert!(contents.contains(&result));
    }

    #[test]
    fn copies_nested_directories_and_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir_all(source.join("nested/deeper")).unwrap();
        fs::write(source.join("root.txt"), "root").unwrap();
        fs::write(source.join("nested/deeper/file.txt"), "nested").unwrap();

        copy_package_tree(&source, &target, SymlinkPolicy::Reject).unwrap();

        assert_eq!(fs::read_to_string(target.join("root.txt")).unwrap(), "root");
        assert_eq!(
            fs::read_to_string(target.join("nested/deeper/file.txt")).unwrap(),
            "nested"
        );
    }

    #[cfg(unix)]
    #[test]
    fn applies_each_symlink_policy() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), "content").unwrap();
        std::os::unix::fs::symlink("file.txt", source.join("link.txt")).unwrap();

        let rejected = temp.path().join("rejected");
        let error = copy_package_tree(&source, &rejected, SymlinkPolicy::Reject).unwrap_err();
        assert!(error.to_string().contains("may not contain symlinks"));

        let followed = temp.path().join("followed");
        copy_package_tree(&source, &followed, SymlinkPolicy::Follow).unwrap();
        assert_eq!(
            fs::read_to_string(followed.join("link.txt")).unwrap(),
            "content"
        );
        assert!(
            !fs::symlink_metadata(followed.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let preserved = temp.path().join("preserved");
        copy_package_tree(&source, &preserved, SymlinkPolicy::Preserve).unwrap();
        assert_eq!(
            fs::read_link(preserved.join("link.txt")).unwrap(),
            Path::new("file.txt")
        );
    }
}
