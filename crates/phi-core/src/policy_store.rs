use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

pub fn submit(store: &Path, source: &Path) -> Result<String> {
    let bytes = fs::read(source)?;
    let id = blake3::hash(&bytes).to_hex()[..12].to_owned();
    let candidates = store.join("candidates");
    fs::create_dir_all(&candidates)?;
    let target = candidates.join(format!("{id}.scm"));
    if !target.exists() {
        fs::write(target, bytes)?;
    }
    Ok(id)
}

pub fn activate(store: &Path, id: &str) -> Result<()> {
    if id.len() != 12 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid candidate id");
    }
    let candidate = store.join("candidates").join(format!("{id}.scm"));
    if !candidate.is_file() {
        bail!("candidate not found");
    }
    fs::create_dir_all(store)?;
    fs::write(store.join("active"), format!("{id}\n"))?;
    Ok(())
}

pub fn active(store: &Path, fallback: &Path) -> Result<PathBuf> {
    let marker = store.join("active");
    if !marker.exists() {
        return Ok(fallback.to_owned());
    }
    let id = fs::read_to_string(marker)?.trim().to_owned();
    let candidate = store.join("candidates").join(format!("{id}.scm"));
    if !candidate.is_file() {
        bail!("active policy candidate is missing");
    }
    candidate
        .canonicalize()
        .context("canonicalize active policy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_immutable_and_explicitly_activated() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("agent.scm");
        fs::write(&source, "(define x 1)").unwrap();
        let store = dir.path().join("store");
        let id = submit(&store, &source).unwrap();
        assert_eq!(active(&store, &source).unwrap(), source);
        activate(&store, &id).unwrap();
        assert!(
            active(&store, &source)
                .unwrap()
                .ends_with(format!("{id}.scm"))
        );
    }
}
