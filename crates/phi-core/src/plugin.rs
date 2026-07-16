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
}
