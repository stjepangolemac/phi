use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use phi_protocol::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capability::Tool;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillSpec {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug)]
struct Skill {
    spec: SkillSpec,
    root: PathBuf,
}

pub fn discover(
    system_root: &Path,
    personal_root: &Path,
    workspace: &Path,
    plugin_roots: &[PathBuf],
) -> Result<Vec<SkillSpec>> {
    Ok(index(system_root, personal_root, workspace, plugin_roots)?
        .into_values()
        .map(|skill| skill.spec)
        .collect())
}

pub struct LoadSkill {
    pub system_root: PathBuf,
    pub personal_root: PathBuf,
    pub plugin_roots: Vec<PathBuf>,
}

impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".into(),
            description: "Load an installed skill resource.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["name", "path"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, workspace: &Path, arguments: Value) -> Result<Value> {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .context("load_skill requires a name")?;
        let relative = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("SKILL.md");
        let skills = index(
            &self.system_root,
            &self.personal_root,
            workspace,
            &self.plugin_roots,
        )?;
        let skill = skills
            .get(name)
            .with_context(|| format!("skill not found: {name}"))?;
        let path = fs::canonicalize(skill.root.join(relative))?;
        if !path.starts_with(&skill.root) {
            bail!("path is outside skill: {name}");
        }
        if !path.is_file() {
            bail!("skill resource is not a file: {relative}");
        }
        Ok(json!({
            "name": name,
            "path": path.strip_prefix(&skill.root)?.display().to_string(),
            "content": fs::read_to_string(path)?,
        }))
    }

    fn parallel_safe(&self) -> bool {
        true
    }
}

fn index(
    system_root: &Path,
    personal_root: &Path,
    workspace: &Path,
    plugin_roots: &[PathBuf],
) -> Result<BTreeMap<String, Skill>> {
    let mut skills = BTreeMap::new();
    for root in plugin_roots {
        add_skill(&mut skills, root)?;
    }
    add_root(&mut skills, personal_root)?;
    add_root(&mut skills, &workspace.join(".phi/skills"))?;
    add_root(&mut skills, system_root)?;
    Ok(skills)
}

fn add_skill(skills: &mut BTreeMap<String, Skill>, root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let source = root.join("SKILL.md");
    if !source.is_file() {
        bail!("registered skill is missing SKILL.md: {}", root.display());
    }
    let spec = parse_metadata(&fs::read_to_string(&source)?)
        .with_context(|| format!("read skill metadata from {}", source.display()))?;
    if skills.contains_key(&spec.name) {
        bail!("duplicate registered plugin skill: {}", spec.name);
    }
    skills.insert(spec.name.clone(), Skill { spec, root });
    Ok(())
}

fn add_root(skills: &mut BTreeMap<String, Skill>, root: &Path) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let root = fs::canonicalize(root)?;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_root = fs::canonicalize(entry.path())?;
        if !skill_root.starts_with(&root) {
            bail!("skill directory is outside its root");
        }
        let source = skill_root.join("SKILL.md");
        if !source.is_file() {
            continue;
        }
        let spec = parse_metadata(&fs::read_to_string(&source)?)
            .with_context(|| format!("read skill metadata from {}", source.display()))?;
        skills.insert(
            spec.name.clone(),
            Skill {
                spec,
                root: skill_root,
            },
        );
    }
    Ok(())
}

fn parse_metadata(source: &str) -> Result<SkillSpec> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("---") {
        bail!("SKILL.md must start with YAML frontmatter");
    }
    let mut name = None;
    let mut description = None;
    let mut found_end = false;
    let mut index = 1;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "---" {
            found_end = true;
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        match key.trim() {
            "name" => name = Some(yaml_scalar(value.trim())?),
            "description" if value.trim().starts_with('>') || value.trim().starts_with('|') => {
                let folded = value.trim().starts_with('>');
                let mut parts = Vec::new();
                index += 1;
                while index < lines.len()
                    && (lines[index].trim().is_empty()
                        || lines[index].starts_with(' ')
                        || lines[index].starts_with('\t'))
                {
                    parts.push(lines[index].trim());
                    index += 1;
                }
                description = Some(parts.join(if folded { " " } else { "\n" }));
                continue;
            }
            "description" => description = Some(yaml_scalar(value.trim())?),
            _ => {}
        }
        index += 1;
    }
    if !found_end {
        bail!("SKILL.md frontmatter is not closed");
    }
    let name = name
        .filter(|value| !value.is_empty())
        .context("skill name is required")?;
    if !name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        bail!("skill name must contain only lowercase letters, digits, and hyphens");
    }
    let description = description
        .filter(|value| !value.is_empty())
        .context("skill description is required")?;
    Ok(SkillSpec { name, description })
}

fn yaml_scalar(value: &str) -> Result<String> {
    if value.starts_with('"') {
        return serde_json::from_str(value).context("invalid quoted YAML string");
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Registry;

    fn skill(root: &Path, directory: &str, name: &str, description: &str) {
        let root = root.join(directory);
        fs::create_dir_all(root.join("references")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nInstructions."),
        )
        .unwrap();
        fs::write(root.join("references/details.md"), "Details.").unwrap();
    }

    #[test]
    fn workspace_skills_override_personal_skills() {
        let temp = tempfile::tempdir().unwrap();
        let personal = temp.path().join("personal");
        let system = temp.path().join("system");
        let workspace = temp.path().join("workspace");
        skill(&personal, "review", "review", "Personal review.");
        skill(
            &workspace.join(".phi/skills"),
            "review",
            "review",
            "Workspace review.",
        );

        assert_eq!(
            discover(&system, &personal, &workspace, &[]).unwrap(),
            vec![SkillSpec {
                name: "review".into(),
                description: "Workspace review.".into(),
            }]
        );

        let mut registry = Registry::default();
        registry.register_hidden(LoadSkill {
            system_root: system,
            personal_root: personal,
            plugin_roots: Vec::new(),
        });
        let result = registry
            .execute(
                &workspace,
                "load_skill",
                json!({ "name": "review", "path": "references/details.md" }),
            )
            .unwrap();
        assert_eq!(result["content"], "Details.");
        assert!(registry.specs().is_empty());
    }

    #[test]
    fn skill_reads_cannot_escape_the_skill_directory() {
        let temp = tempfile::tempdir().unwrap();
        let personal = temp.path().join("personal");
        let system = temp.path().join("system");
        let workspace = temp.path().join("workspace");
        skill(&personal, "review", "review", "Review code.");
        fs::write(personal.join("secret"), "nope").unwrap();
        let tool = LoadSkill {
            system_root: system,
            personal_root: personal,
            plugin_roots: Vec::new(),
        };
        assert!(
            tool.execute(&workspace, json!({ "name": "review", "path": "../secret" }),)
                .unwrap_err()
                .to_string()
                .contains("outside skill")
        );
    }

    #[test]
    fn system_skills_cannot_be_shadowed() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("system");
        let personal = temp.path().join("personal");
        let workspace = temp.path().join("workspace");
        skill(&system, "phi-harness", "phi-harness", "System manual.");
        skill(
            &workspace.join(".phi/skills"),
            "phi-harness",
            "phi-harness",
            "Shadow manual.",
        );

        assert_eq!(
            discover(&system, &personal, &workspace, &[]).unwrap(),
            vec![SkillSpec {
                name: "phi-harness".into(),
                description: "System manual.".into(),
            }]
        );
    }

    #[test]
    fn personal_skills_override_registered_plugin_skills() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = temp.path().join("plugin-skill");
        let personal = temp.path().join("personal");
        let system = temp.path().join("system");
        let workspace = temp.path().join("workspace");
        skill(temp.path(), "plugin-skill", "review", "Plugin review.");
        skill(&personal, "review", "review", "Personal review.");

        assert_eq!(
            discover(&system, &personal, &workspace, &[plugin]).unwrap(),
            vec![SkillSpec {
                name: "review".into(),
                description: "Personal review.".into(),
            }]
        );
    }

    #[test]
    fn reads_folded_frontmatter_descriptions() {
        assert_eq!(
            parse_metadata(
                "---\nname: review\ndescription: >-\n  Review code for correctness\n  and maintainability.\n---\n"
            )
            .unwrap(),
            SkillSpec {
                name: "review".into(),
                description: "Review code for correctness and maintainability.".into(),
            }
        );
    }
}
