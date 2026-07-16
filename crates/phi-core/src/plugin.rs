use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{AtomicWriteMode, SymlinkPolicy, copy_package_tree, home::PhiHome, write_json_atomic};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub entrypoint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OfficialPlugin {
    pub name: String,
    pub version: String,
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
    pub manifest: PluginManifest,
}

pub fn install(home: &PhiHome, url: &str, revision: &str, path: &str) -> Result<LockedPlugin> {
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
    let source = repository.join(path).canonicalize()?;
    if !source.starts_with(&repository) {
        bail!("plugin path escapes the repository");
    }
    let manifest = read_manifest(&source)?;
    validate_name(&manifest.name)?;
    validate_entrypoint(&source, &manifest)?;

    let target = install_root(home, &manifest.name, &commit);
    if !target.exists() {
        let parent = target.parent().context("plugin target has no parent")?;
        fs::create_dir_all(parent)?;
        let staged = tempfile::tempdir_in(parent)?;
        copy_package_tree(&source, staged.path(), SymlinkPolicy::Reject)?;
        let staged = staged.keep();
        fs::rename(staged, &target)?;
    }

    let locked = LockedPlugin {
        name: manifest.name,
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
        let source = repository.join(&plugin.path).canonicalize()?;
        if !source.starts_with(&repository) {
            bail!("official plugin path escapes the repository");
        }
        let target = install_root(home, &plugin.name, &commit);
        if !target.exists() {
            let parent = target.parent().context("plugin target has no parent")?;
            fs::create_dir_all(parent)?;
            let staged = tempfile::tempdir_in(parent)?;
            if source.is_dir() {
                copy_package_tree(&source, staged.path(), SymlinkPolicy::Reject)?;
            } else {
                fs::write(
                    staged.path().join("plugin.json"),
                    serde_json::to_vec_pretty(&PluginManifest {
                        name: plugin.name.clone(),
                        version: plugin.version.clone(),
                        entrypoint: "main.scm".into(),
                    })?,
                )?;
                fs::copy(&source, staged.path().join("main.scm"))?;
            }
            let manifest = read_manifest(staged.path())?;
            if manifest.name != plugin.name || manifest.version != plugin.version {
                bail!(
                    "official plugin manifest does not match catalog: {}",
                    plugin.name
                );
            }
            validate_entrypoint(staged.path(), &manifest)?;
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
    let manifest = read_manifest(&root)?;
    validate_entrypoint(&root, &manifest)?;
    Ok(InstalledPlugin {
        locked,
        root,
        manifest,
    })
}

pub fn read_lock(home: &PhiHome) -> Result<PluginLock> {
    let path = home.plugin_lock();
    if !path.exists() {
        return Ok(PluginLock::default());
    }
    serde_json::from_slice(&fs::read(&path)?).context("read plugin lock")
}

pub fn read_manifest(root: &Path) -> Result<PluginManifest> {
    serde_json::from_slice(&fs::read(root.join("plugin.json"))?).context("read plugin manifest")
}

pub fn install_root(home: &PhiHome, name: &str, commit: &str) -> PathBuf {
    home.plugins().join(name).join(commit)
}

fn write_lock(home: &PhiHome, lock: &PluginLock) -> Result<()> {
    write_json_atomic(&home.plugin_lock(), lock, AtomicWriteMode::Overwrite)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        bail!("invalid plugin name: {name}");
    }
    Ok(())
}

fn validate_entrypoint(root: &Path, manifest: &PluginManifest) -> Result<()> {
    let root = root.canonicalize()?;
    let entrypoint = root.join(&manifest.entrypoint).canonicalize()?;
    if !entrypoint.starts_with(&root) || !entrypoint.is_file() {
        bail!("plugin entrypoint escapes its package");
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

    #[test]
    fn installs_nested_git_plugin_at_exact_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let package = repository.join("plugins/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("plugin.json"),
            r#"{"name":"example","version":"0.1.0","entrypoint":"main.scm"}"#,
        )
        .unwrap();
        fs::write(package.join("main.scm"), "(define example #t)").unwrap();
        git(None, &["init", "--quiet", repository.to_str().unwrap()]).unwrap();
        git(Some(&repository), &["add", "."]).unwrap();
        git(
            Some(&repository),
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
        let commit = git_output(Some(&repository), &["rev-parse", "HEAD"]).unwrap();
        let home = PhiHome {
            root: temp.path().join("home"),
        };
        let locked = install(
            &home,
            repository.to_str().unwrap(),
            &commit,
            "plugins/example",
        )
        .unwrap();
        assert_eq!(locked.commit, commit);
        assert!(
            install_root(&home, "example", &commit)
                .join("main.scm")
                .is_file()
        );
        assert_eq!(read_lock(&home).unwrap().plugins, vec![locked]);
    }

    #[test]
    fn installs_latest_official_catalog_from_one_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        fs::create_dir_all(repository.join("policy/tools/package")).unwrap();
        fs::write(repository.join("policy/tool.scm"), "(define tool #t)").unwrap();
        fs::write(
            repository.join("policy/tools/package/plugin.json"),
            r#"{"name":"package","version":"0.2.0","entrypoint":"main.scm"}"#,
        )
        .unwrap();
        fs::write(
            repository.join("policy/tools/package/main.scm"),
            "(define package #t)",
        )
        .unwrap();
        let catalog = OfficialPluginCatalog {
            url: repository.display().to_string(),
            revision: "main".into(),
            plugins: vec![
                OfficialPlugin {
                    name: "single".into(),
                    version: "0.2.0".into(),
                    path: "policy/tool.scm".into(),
                },
                OfficialPlugin {
                    name: "package".into(),
                    version: "0.2.0".into(),
                    path: "policy/tools/package".into(),
                },
            ],
        };
        fs::write(
            repository.join("official-plugins.json"),
            serde_json::to_vec_pretty(&catalog).unwrap(),
        )
        .unwrap();
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
        git(Some(&repository), &["add", "."]).unwrap();
        git(
            Some(&repository),
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
        let home = PhiHome {
            root: temp.path().join("home"),
        };

        let installed_plugins = install_official_catalog(&home, &catalog).unwrap();

        assert_eq!(installed_plugins.len(), 2);
        assert_eq!(read_lock(&home).unwrap().plugins.len(), 2);
        assert_eq!(
            installed(&home, "single").unwrap().manifest.version,
            "0.2.0"
        );
        assert!(
            installed(&home, "package")
                .unwrap()
                .root
                .join("main.scm")
                .is_file()
        );
    }
}
