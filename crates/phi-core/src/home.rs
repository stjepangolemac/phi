use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct PhiHome {
    pub root: PathBuf,
}

impl PhiHome {
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os("PHI_HOME") {
            Some(path) => PathBuf::from(path),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".phi")
            }
        };
        Ok(Self { root })
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn scheme_config(&self) -> PathBuf {
        self.root.join("config.scm")
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn plugin_lock(&self) -> PathBuf {
        self.root.join("plugins.lock.json")
    }

    pub fn plugins(&self) -> PathBuf {
        self.root.join("plugins")
    }

    pub fn skills(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn builtins(&self) -> PathBuf {
        let root = self.builtin_version_root();
        let Ok(current) = fs::read_to_string(root.join("current")) else {
            return root;
        };
        let current = current.trim();
        if current.is_empty()
            || !current
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return root;
        }
        let snapshot = root.join(current);
        if matches!(
            fs::symlink_metadata(&snapshot),
            Ok(metadata) if metadata.file_type().is_dir()
        ) {
            snapshot
        } else {
            root
        }
    }

    pub fn builtin_version_root(&self) -> PathBuf {
        self.root.join("builtins").join(env!("CARGO_PKG_VERSION"))
    }

    pub fn builtin_skills(&self) -> PathBuf {
        self.builtins().join("skills")
    }

    pub fn official_plugin_state(&self) -> PathBuf {
        self.builtins().join("official-plugins-state.json")
    }
}
