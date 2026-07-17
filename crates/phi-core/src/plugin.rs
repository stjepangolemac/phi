use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{AtomicWriteMode, SymlinkPolicy, copy_package_tree, home::PhiHome, write_json_atomic};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OfficialPlugin {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OfficialPluginCatalog {
    pub url: String,
    pub revision: String,
    pub plugins: Vec<OfficialPlugin>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OfficialPluginState {
    #[serde(default)]
    pub commit: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PluginUpdate {
    pub name: String,
    pub current: String,
    pub latest: String,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PluginUpdateReport {
    pub updates: Vec<PluginUpdate>,
    pub warnings: Vec<String>,
}

const OFFICIAL_CATALOG: &str = include_str!("../../../official-plugins.json");

pub fn official_catalog() -> Result<OfficialPluginCatalog> {
    serde_json::from_str(OFFICIAL_CATALOG).context("read bundled official plugin catalog")
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LockedPlugin {
    pub name: String,
    pub url: String,
    pub requested_rev: String,
    pub commit: String,
    #[serde(default = "root_path")]
    pub path: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PluginLock {
    #[serde(default)]
    pub plugins: Vec<LockedPlugin>,
}

pub struct InstalledPlugin {
    pub locked: LockedPlugin,
    pub root: PathBuf,
    pub entrypoint: PathBuf,
}

pub fn install(
    home: &PhiHome,
    name: &str,
    url: &str,
    revision: &str,
    path: &str,
) -> Result<LockedPlugin> {
    validate_name(name)?;
    if url.is_empty() || revision.is_empty() || url.starts_with('-') || revision.starts_with('-') {
        bail!("plugin URL and revision are required");
    }
    fs::create_dir_all(&home.root)?;
    let checkout = tempfile::tempdir_in(&home.root)?;
    git(
        None,
        &["clone", "--quiet", url, checkout.path().to_str().unwrap()],
    )?;
    git(
        Some(checkout.path()),
        &["checkout", "--quiet", "--detach", revision],
    )?;
    let commit = git_output(Some(checkout.path()), &["rev-parse", "HEAD"])?;
    let repository = checkout.path().canonicalize()?;
    let requested_source = repository.join(path);
    if requested_source
        .symlink_metadata()?
        .file_type()
        .is_symlink()
    {
        bail!("plugin package may not be a symlink");
    }
    let source = requested_source.canonicalize()?;
    if !source.starts_with(&repository) {
        bail!("plugin path escapes the repository");
    }
    validate_package(&source)?;

    let target = install_root(home, name, &commit);
    if !target.exists() {
        let parent = target.parent().context("plugin target has no parent")?;
        fs::create_dir_all(parent)?;
        let staged = tempfile::tempdir_in(parent)?;
        copy_package_tree(&source, staged.path(), SymlinkPolicy::Reject)?;
        validate_package(staged.path())?;
        let staged = staged.keep();
        fs::rename(staged, &target)?;
    }

    let locked = LockedPlugin {
        name: name.into(),
        url: url.into(),
        requested_rev: revision.into(),
        commit,
        path: path.into(),
    };
    let mut lock = read_lock(home)?;
    lock.plugins.retain(|plugin| plugin.name != locked.name);
    lock.plugins.push(locked.clone());
    lock.plugins
        .sort_by(|left, right| left.name.cmp(&right.name));
    write_lock(home, &lock)?;
    Ok(locked)
}

pub fn check_updates(home: &PhiHome) -> Result<PluginUpdateReport> {
    let catalog = official_catalog()?;
    let lock = read_lock(home)?;
    let state = read_official_state(home)?;
    let mut report = PluginUpdateReport::default();
    match remote_revision(&catalog.url, &catalog.revision) {
        Ok(latest) => {
            for plugin in &catalog.plugins {
                let current = lock
                    .plugins
                    .iter()
                    .find(|locked| locked.name == plugin.name)
                    .map(|locked| locked.commit.clone())
                    .unwrap_or_else(|| state.commit.clone());
                if current != latest {
                    report.updates.push(PluginUpdate {
                        name: plugin.name.clone(),
                        current,
                        latest: latest.clone(),
                    });
                }
            }
        }
        Err(error) => report.warnings.push(format!("official plugins: {error}")),
    }
    for plugin in lock.plugins.iter().filter(|locked| {
        !catalog
            .plugins
            .iter()
            .any(|official| official.name == locked.name)
    }) {
        match remote_revision(&plugin.url, &plugin.requested_rev) {
            Ok(latest) if latest != plugin.commit => report.updates.push(PluginUpdate {
                name: plugin.name.clone(),
                current: plugin.commit.clone(),
                latest,
            }),
            Ok(_) => {}
            Err(error) => report.warnings.push(format!("{}: {error}", plugin.name)),
        }
    }
    report
        .updates
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(report)
}

pub fn update_all(home: &PhiHome) -> Result<Vec<LockedPlugin>> {
    let catalog = official_catalog()?;
    let official_names = catalog
        .plugins
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let third_party = read_lock(home)?
        .plugins
        .into_iter()
        .filter(|plugin| !official_names.contains(plugin.name.as_str()))
        .collect::<Vec<_>>();
    let mut updated = install_official_catalog(home, &catalog)?;
    for plugin in third_party {
        updated.push(install(
            home,
            &plugin.name,
            &plugin.url,
            &plugin.requested_rev,
            &plugin.path,
        )?);
    }
    updated.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(updated)
}

fn install_official_catalog(
    home: &PhiHome,
    source_catalog: &OfficialPluginCatalog,
) -> Result<Vec<LockedPlugin>> {
    fs::create_dir_all(&home.root)?;
    let checkout = tempfile::tempdir_in(&home.root)?;
    git(
        None,
        &[
            "clone",
            "--quiet",
            &source_catalog.url,
            checkout.path().to_str().unwrap(),
        ],
    )?;
    git(
        Some(checkout.path()),
        &["checkout", "--quiet", "--detach", &source_catalog.revision],
    )?;
    let commit = git_output(Some(checkout.path()), &["rev-parse", "HEAD"])?;
    let catalog: OfficialPluginCatalog =
        serde_json::from_slice(&fs::read(checkout.path().join("official-plugins.json"))?)
            .context("read latest official plugin catalog")?;
    let repository = checkout.path().canonicalize()?;
    let mut locked = Vec::new();
    for plugin in &catalog.plugins {
        validate_name(&plugin.name)?;
        let requested_source = repository.join(&plugin.path);
        validate_package(&requested_source)?;
        let source = requested_source.canonicalize()?;
        if !source.starts_with(&repository) {
            bail!("official plugin path escapes the repository");
        }
        let target = install_root(home, &plugin.name, &commit);
        if !target.exists() {
            let parent = target.parent().context("plugin target has no parent")?;
            fs::create_dir_all(parent)?;
            let staged = tempfile::tempdir_in(parent)?;
            copy_package_tree(&source, staged.path(), SymlinkPolicy::Reject)?;
            validate_package(staged.path())?;
            fs::rename(staged.keep(), &target)?;
        }
        locked.push(LockedPlugin {
            name: plugin.name.clone(),
            url: catalog.url.clone(),
            requested_rev: catalog.revision.clone(),
            commit: commit.clone(),
            path: plugin.path.clone(),
        });
    }
    let names = source_catalog
        .plugins
        .iter()
        .chain(catalog.plugins.iter())
        .map(|plugin| plugin.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut lock = read_lock(home)?;
    lock.plugins
        .retain(|plugin| !names.contains(plugin.name.as_str()));
    lock.plugins.extend(locked.clone());
    lock.plugins
        .sort_by(|left, right| left.name.cmp(&right.name));
    write_lock(home, &lock)?;
    Ok(locked)
}

pub fn read_official_state(home: &PhiHome) -> Result<OfficialPluginState> {
    if !home.official_plugin_state().is_file() {
        return Ok(OfficialPluginState::default());
    }
    serde_json::from_slice(&fs::read(home.official_plugin_state())?)
        .context("read official plugin state")
}

pub fn write_official_state(home: &PhiHome, commit: &str) -> Result<()> {
    write_json_atomic(
        &home.official_plugin_state(),
        &OfficialPluginState {
            commit: commit.into(),
        },
        AtomicWriteMode::Overwrite,
    )
}

fn remote_revision(url: &str, revision: &str) -> Result<String> {
    if revision.is_empty() || revision.starts_with('-') {
        bail!("plugin revision is invalid");
    }
    if revision.len() == 40
        && revision
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Ok(revision.to_owned());
    }
    let peeled = format!("{revision}^{{}}");
    let output = git_output(None, &["ls-remote", url, revision, &peeled])?;
    output
        .lines()
        .find(|line| line.ends_with("^{}"))
        .or_else(|| output.lines().next())
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .with_context(|| format!("revision not found: {revision}"))
}

pub fn remove(home: &PhiHome, name: &str) -> Result<()> {
    let mut lock = read_lock(home)?;
    let previous = lock.plugins.len();
    lock.plugins.retain(|plugin| plugin.name != name);
    if lock.plugins.len() == previous {
        bail!("plugin not installed: {name}");
    }
    write_lock(home, &lock)?;
    let root = home.plugins().join(name);
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

pub fn installed(home: &PhiHome, name: &str) -> Result<InstalledPlugin> {
    let locked = read_lock(home)?
        .plugins
        .into_iter()
        .find(|plugin| plugin.name == name)
        .with_context(|| format!("plugin not installed: {name}"))?;
    let root = install_root(home, name, &locked.commit);
    let entrypoint = validate_package(&root)?;
    Ok(InstalledPlugin {
        locked,
        root,
        entrypoint,
    })
}

pub fn read_lock(home: &PhiHome) -> Result<PluginLock> {
    let path = home.plugin_lock();
    if !path.exists() {
        return Ok(PluginLock::default());
    }
    serde_json::from_slice(&fs::read(&path)?).context("read plugin lock")
}

pub fn install_root(home: &PhiHome, name: &str, commit: &str) -> PathBuf {
    home.plugins().join(name).join(commit)
}

fn write_lock(home: &PhiHome, lock: &PluginLock) -> Result<()> {
    write_json_atomic(&home.plugin_lock(), lock, AtomicWriteMode::Overwrite)
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        bail!("invalid plugin name: {name}");
    }
    Ok(())
}

pub fn validate_package(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("plugin package is missing: {}", root.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("plugin package is not a directory: {}", root.display());
    }
    let entrypoint = root.join("plugin.scm");
    let metadata = fs::symlink_metadata(&entrypoint)
        .with_context(|| format!("plugin entrypoint is missing: {}", entrypoint.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!(
            "plugin entrypoint is not a regular file: {}",
            entrypoint.display()
        );
    }
    validate_package_tree(root)?;
    Ok(entrypoint)
}

fn validate_package_tree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!("plugin package may not contain symlinks");
        }
        if metadata.is_dir() {
            validate_package_tree(&entry.path())?;
        } else if !metadata.is_file() {
            bail!("plugin package may contain only files and directories");
        }
    }
    Ok(())
}

fn git(directory: Option<&Path>, args: &[&str]) -> Result<()> {
    let output = command(directory, args).output()?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_output(directory: Option<&Path>, args: &[&str]) -> Result<String> {
    let output = command(directory, args).output()?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn command(directory: Option<&Path>, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args(args);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command
}

fn root_path() -> String {
    ".".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_repository(repository: &Path) -> String {
        git(
            None,
            &[
                "init",
                "--quiet",
                "-b",
                "main",
                repository.to_str().unwrap(),
            ],
        )
        .unwrap();
        git(Some(repository), &["add", "."]).unwrap();
        git(
            Some(repository),
            &[
                "-c",
                "user.name=Phi",
                "-c",
                "user.email=phi@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        git_output(Some(repository), &["rev-parse", "HEAD"]).unwrap()
    }

    fn package(root: &Path, source: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("plugin.scm"), source).unwrap();
    }

    #[test]
    fn installs_named_nested_git_plugin_at_exact_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        package(&repository.join("plugins/example"), "(define example #t)");
        let commit = commit_repository(&repository);
        let home = PhiHome {
            root: temp.path().join("home"),
        };

        let locked = install(
            &home,
            "example",
            repository.to_str().unwrap(),
            &commit,
            "plugins/example",
        )
        .unwrap();

        assert_eq!(locked.name, "example");
        assert_eq!(locked.commit, commit);
        assert!(installed(&home, "example").unwrap().entrypoint.is_file());
        assert_eq!(read_lock(&home).unwrap().plugins, vec![locked]);
    }

    #[test]
    fn rejects_invalid_explicit_name_before_cloning() {
        let temp = tempfile::tempdir().unwrap();
        let home = PhiHome {
            root: temp.path().join("home"),
        };
        let error = install(&home, "../bad", "does-not-exist", "main", ".").unwrap_err();
        assert!(error.to_string().contains("invalid plugin name"));
        assert!(!home.root.exists());
    }

    #[test]
    fn validates_fixed_package_entrypoint() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        fs::create_dir(&missing).unwrap();
        assert!(
            validate_package(&missing)
                .unwrap_err()
                .to_string()
                .contains("entrypoint is missing")
        );

        let directory = temp.path().join("directory");
        fs::create_dir_all(directory.join("plugin.scm")).unwrap();
        assert!(
            validate_package(&directory)
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );

        let valid = temp.path().join("valid");
        package(&valid, "(define valid #t)");
        assert_eq!(validate_package(&valid).unwrap(), valid.join("plugin.scm"));
    }

    #[test]
    fn install_rejects_package_escape() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        package(&repository, "(define repository #t)");
        package(&temp.path().join("outside"), "(define outside #t)");
        let commit = commit_repository(&repository);
        let home = PhiHome {
            root: temp.path().join("home"),
        };

        let error = install(
            &home,
            "example",
            repository.to_str().unwrap(),
            &commit,
            temp.path().join("outside").to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("escapes the repository"));
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinks_in_package() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let plugin = repository.join("plugins/example");
        package(&plugin, "(define example #t)");
        fs::write(repository.join("support.txt"), "support").unwrap();
        symlink("../../support.txt", plugin.join("support.txt")).unwrap();
        let commit = commit_repository(&repository);
        let home = PhiHome {
            root: temp.path().join("home"),
        };

        let error = install(
            &home,
            "example",
            repository.to_str().unwrap(),
            &commit,
            "plugins/example",
        )
        .unwrap_err();
        assert!(error.to_string().contains("may not contain symlinks"));
    }

    #[test]
    fn installs_latest_official_directory_packages_from_one_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        package(&repository.join("plugins/single"), "(define single #t)");
        package(&repository.join("plugins/package"), "(define package #t)");
        let catalog = OfficialPluginCatalog {
            url: repository.display().to_string(),
            revision: "main".into(),
            plugins: vec![
                OfficialPlugin {
                    name: "single".into(),
                    path: "plugins/single".into(),
                },
                OfficialPlugin {
                    name: "package".into(),
                    path: "plugins/package".into(),
                },
            ],
        };
        fs::write(
            repository.join("official-plugins.json"),
            serde_json::to_vec_pretty(&catalog).unwrap(),
        )
        .unwrap();
        commit_repository(&repository);
        let home = PhiHome {
            root: temp.path().join("home"),
        };

        let installed_plugins = install_official_catalog(&home, &catalog).unwrap();

        assert_eq!(installed_plugins.len(), 2);
        assert_eq!(read_lock(&home).unwrap().plugins.len(), 2);
        assert!(installed(&home, "single").unwrap().entrypoint.is_file());
        assert!(installed(&home, "package").unwrap().entrypoint.is_file());
        assert_eq!(
            fs::read_dir(installed(&home, "single").unwrap().root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("plugin.scm")]
        );
    }
}
