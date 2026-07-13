use anyhow::{Result, bail};

pub struct Permissions {
    pub allow_shell: bool,
    pub allow_write: bool,
}

impl Permissions {
    pub fn authorize_tool(&self, name: &str) -> Result<()> {
        match name {
            "shell" if !self.allow_shell => bail!("shell approval required: use --allow-shell"),
            "replace_file" if !self.allow_write => {
                bail!("write approval required: use --allow-write")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_mutating_tools() {
        let permissions = Permissions {
            allow_shell: false,
            allow_write: false,
        };
        assert!(permissions.authorize_tool("read_file").is_ok());
        assert!(permissions.authorize_tool("shell").is_err());
        assert!(permissions.authorize_tool("replace_file").is_err());
    }
}
