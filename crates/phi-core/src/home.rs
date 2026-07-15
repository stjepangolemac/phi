use std::path::PathBuf;

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

    pub fn builtins(&self) -> PathBuf {
        self.root.join("builtins").join(env!("CARGO_PKG_VERSION"))
    }

    pub fn builtin_skills(&self) -> PathBuf {
        self.builtins().join("skills")
    }
}
