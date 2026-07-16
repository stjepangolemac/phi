use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillSpec {
    pub name: String,
    pub description: String,
    pub path: String,
}

#[derive(Clone, Debug)]
struct Skill {
    spec: SkillSpec,
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SkillCatalog {
    pub skills: Vec<SkillSpec>,
    resource_roots: BTreeMap<String, PathBuf>,
}

impl SkillCatalog {
    pub fn resource_roots(&self) -> BTreeMap<String, PathBuf> {
        self.resource_roots.clone()
    }
}

pub fn discover(
    system_root: &Path,
    personal_root: &Path,
    workspace: &Path,
    plugin_roots: &[PathBuf],
) -> Result<SkillCatalog> {
    let skills = index(system_root, personal_root, workspace, plugin_roots)?;
    let resource_roots = skills
        .iter()
        .map(|(name, skill)| (resource_prefix(name), skill.root.clone()))
        .collect();
    let skills = skills.into_values().map(|skill| skill.spec).collect();
    Ok(SkillCatalog {
        skills,
        resource_roots,
    })
}

fn resource_prefix(name: &str) -> String {
    format!("skill://{name}/")
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
    let path = format!("{}SKILL.md", resource_prefix(&name));
    Ok(SkillSpec {
        name,
        description,
        path,
    })
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
    use crate::capability::{ReadFile, Tool};
    use serde_json::json;

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
            discover(&system, &personal, &workspace, &[])
                .unwrap()
                .skills,
            vec![SkillSpec {
                name: "review".into(),
                description: "Workspace review.".into(),
                path: "skill://review/SKILL.md".into(),
            }]
        );

        assert_eq!(
            discover(&system, &personal, &workspace, &[])
                .unwrap()
                .resource_roots()["skill://review/"],
            fs::canonicalize(workspace.join(".phi/skills/review")).unwrap()
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
            discover(&system, &personal, &workspace, &[])
                .unwrap()
                .skills,
            vec![SkillSpec {
                name: "phi-harness".into(),
                description: "System manual.".into(),
                path: "skill://phi-harness/SKILL.md".into(),
            }]
        );
        assert_eq!(
            discover(&system, &personal, &workspace, &[])
                .unwrap()
                .resource_roots()["skill://phi-harness/"],
            fs::canonicalize(system.join("phi-harness")).unwrap()
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
            discover(
                &system,
                &personal,
                &workspace,
                std::slice::from_ref(&plugin),
            )
            .unwrap()
            .skills,
            vec![SkillSpec {
                name: "review".into(),
                description: "Personal review.".into(),
                path: "skill://review/SKILL.md".into(),
            }]
        );
        assert_eq!(
            discover(
                &system,
                &personal,
                &workspace,
                std::slice::from_ref(&plugin),
            )
            .unwrap()
            .resource_roots()["skill://review/"],
            fs::canonicalize(personal.join("review")).unwrap()
        );
    }

    #[test]
    fn resolved_resources_read_system_workspace_personal_and_plugin_skills() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("system");
        let personal = temp.path().join("personal");
        let workspace = temp.path().join("workspace");
        let plugin = temp.path().join("plugin-skill");
        skill(&system, "system", "system", "System skill.");
        skill(
            &workspace.join(".phi/skills"),
            "workspace",
            "workspace",
            "Workspace skill.",
        );
        skill(&personal, "personal", "personal", "Personal skill.");
        skill(temp.path(), "plugin-skill", "plugin", "Plugin skill.");

        let catalog = discover(&system, &personal, &workspace, &[plugin]).unwrap();
        assert_eq!(
            catalog
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["personal", "plugin", "system", "workspace"]
        );
        let reader = ReadFile {
            full_access: false,
            additional_root: None,
            resource_roots: catalog.resource_roots(),
            resource_help: None,
        };
        for name in ["system", "workspace", "personal", "plugin"] {
            let instructions = reader
                .execute(
                    &workspace,
                    json!({ "path": format!("skill://{name}/SKILL.md") }),
                )
                .unwrap();
            assert!(
                instructions["content"]
                    .as_str()
                    .unwrap()
                    .contains("Instructions.")
            );
            let reference = reader
                .execute(
                    &workspace,
                    json!({ "path": format!("skill://{name}/references/details.md") }),
                )
                .unwrap();
            assert_eq!(reference["content"], "Details.");
        }
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
                path: "skill://review/SKILL.md".into(),
            }
        );
    }
}
