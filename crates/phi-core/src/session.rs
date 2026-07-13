use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

pub struct Session {
    id: String,
    dir: PathBuf,
}

pub struct Sources {
    pub policy: PathBuf,
    pub provider: PathBuf,
    pub prompt: PathBuf,
    pub compaction: PathBuf,
}

impl Session {
    pub fn create(
        root: &Path,
        policy: &Path,
        provider: &Path,
        prompt: &Path,
        compaction: &Path,
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let session = Self::at(root, &id)?;
        fs::create_dir_all(&session.dir)?;
        fs::copy(policy, session.dir.join("policy.scm"))?;
        fs::copy(provider, session.dir.join("provider.scm"))?;
        fs::copy(prompt, session.dir.join("prompt.scm"))?;
        fs::copy(compaction, session.dir.join("compaction.scm"))?;
        write_atomic(
            &session.dir.join("meta.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "id": id,
                "created_at": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                "policy": policy.display().to_string(),
                "provider": provider.display().to_string(),
                "prompt": prompt.display().to_string(),
                "compaction": compaction.display().to_string(),
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

    pub fn sources(&self, prompt_fallback: &Path) -> Result<Sources> {
        let pinned_prompt = self.dir.join("prompt.scm");
        let sources = Sources {
            policy: self.dir.join("policy.scm"),
            provider: self.dir.join("provider.scm"),
            prompt: if pinned_prompt.is_file() {
                pinned_prompt
            } else {
                prompt_fallback.to_owned()
            },
            compaction: self.dir.join("compaction.scm"),
        };
        if !sources.policy.is_file() || !sources.provider.is_file() || !sources.compaction.is_file()
        {
            anyhow::bail!("session has no complete policy snapshot");
        }
        Ok(sources)
    }

    fn at(root: &Path, id: &str) -> Result<Self> {
        let id = Uuid::parse_str(id)?.to_string();
        Ok(Self {
            dir: root.join(id.as_str()),
            id,
        })
    }
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
        fs::write(root.path().join("agent.scm"), "agent").unwrap();
        fs::write(root.path().join("provider.scm"), "provider").unwrap();
        fs::write(root.path().join("prompt.scm"), "prompt").unwrap();
        fs::write(root.path().join("compaction.scm"), "compaction").unwrap();
        let session = Session::create(
            root.path(),
            &root.path().join("agent.scm"),
            &root.path().join("provider.scm"),
            &root.path().join("prompt.scm"),
            &root.path().join("compaction.scm"),
        )
        .unwrap();
        session.save_state("{\"input\":[]}").unwrap();
        let resumed = Session::open(root.path(), session.id()).unwrap();
        assert_eq!(resumed.load_state().unwrap(), "{\"input\":[]}");
        assert_eq!(
            fs::read_to_string(
                resumed
                    .sources(&root.path().join("prompt.scm"))
                    .unwrap()
                    .policy,
            )
            .unwrap(),
            "agent"
        );

        fs::remove_file(resumed.dir.join("prompt.scm")).unwrap();
        let fallback = root.path().join("prompt-fallback.scm");
        fs::write(&fallback, "fallback").unwrap();
        assert_eq!(resumed.sources(&fallback).unwrap().prompt, fallback);
    }
}
