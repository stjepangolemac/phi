use anyhow::{Result, bail};

pub struct Permissions {
    pub allow_shell: bool,
    pub allow_write: bool,
}

impl Permissions {
    pub fn authorize_tool(&self, name: &str) -> Result<()> {
        match name {
            "exec_command" | "write_stdin" | "terminate_process" if !self.allow_shell => {
                bail!("shell approval required: use --allow-shell")
            }
            "patch" if !self.allow_write => {
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
        assert!(permissions.authorize_tool("exec_command").is_err());
        assert!(permissions.authorize_tool("write_stdin").is_err());
        assert!(permissions.authorize_tool("terminate_process").is_err());
        assert!(permissions.authorize_tool("patch").is_err());
    }
}
