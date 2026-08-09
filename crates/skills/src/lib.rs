//! Project-local instruction skills using the Codex `SKILL.md` shape.

use papermachine_protocol::ProjectId;
use papermachine_protocol::ProjectSkill;
use papermachine_protocol::SkillSnapshot;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Clone)]
pub struct ProjectSkillCatalog {
    store: StoreHandle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSkills {
    pub snapshots: Vec<SkillSnapshot>,
    pub instructions: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

impl ProjectSkillCatalog {
    pub fn new(store: StoreHandle) -> Self {
        Self { store }
    }

    pub async fn ensure_project(&self, project_id: ProjectId) -> Result<PathBuf, SkillError> {
        self.store
            .call(move |store| ensure_project(store, project_id))
            .await
    }

    pub async fn list(&self, project_id: ProjectId) -> Result<Vec<ProjectSkill>, SkillError> {
        self.store
            .call(move |store| {
                ensure_project(store, project_id)?;
                store
                    .list_managed_directories("skills")?
                    .into_iter()
                    .map(|slug| load_skill(store, project_id, &slug))
                    .collect()
            })
            .await
    }

    pub async fn load(
        &self,
        project_id: ProjectId,
        slug: &str,
    ) -> Result<ProjectSkill, SkillError> {
        let slug = slug.to_string();
        self.store
            .call(move |store| load_skill(store, project_id, &slug))
            .await
    }

    pub async fn create(
        &self,
        project_id: ProjectId,
        slug: &str,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<ProjectSkill, SkillError> {
        let slug = slug.to_string();
        let name = name.to_string();
        let description = description.to_string();
        let instructions = instructions.to_string();
        self.store
            .call(move |store| {
                create_skill(store, project_id, &slug, &name, &description, &instructions)
            })
            .await
    }

    pub async fn validate_enabled(
        &self,
        project_id: ProjectId,
        slugs: &[String],
    ) -> Result<(), SkillError> {
        let slugs = slugs.to_vec();
        self.store
            .call(move |store| {
                for slug in unique_slugs(&slugs)? {
                    load_skill(store, project_id, slug)?;
                }
                Ok(())
            })
            .await
    }

    pub async fn resolve(
        &self,
        project_id: ProjectId,
        slugs: &[String],
    ) -> Result<ResolvedSkills, SkillError> {
        let slugs = slugs.to_vec();
        self.store
            .call(move |store| resolve_skills(store, project_id, &slugs))
            .await
    }
}

fn ensure_project(store: &Store, project_id: ProjectId) -> Result<PathBuf, SkillError> {
    store.get_project(project_id)?;
    store.ensure_managed_directory("skills")?;
    store.ensure_managed_directory("sources")?;
    Ok(store.managed_root().to_path_buf())
}

fn load_skill(
    store: &Store,
    project_id: ProjectId,
    slug: &str,
) -> Result<ProjectSkill, SkillError> {
    validate_slug(slug)?;
    store.get_project(project_id)?;
    let relative_path = PathBuf::from("skills").join(slug).join("SKILL.md");
    if !store.managed_file_exists(&relative_path)? {
        return Err(SkillError::NotFound(slug.to_string()));
    }
    let source = store.read_managed_text(&relative_path, 1024 * 1024)?;
    let (frontmatter, instructions) = parse_skill_markdown(&source)?;
    Ok(ProjectSkill {
        slug: slug.to_string(),
        name: frontmatter.name,
        description: frontmatter.description,
        relative_path: format!("skills/{slug}/SKILL.md"),
        sha256: hex::encode(Sha256::digest(source.as_bytes())),
        instructions: instructions.to_string(),
    })
}

fn create_skill(
    store: &Store,
    project_id: ProjectId,
    slug: &str,
    name: &str,
    description: &str,
    instructions: &str,
) -> Result<ProjectSkill, SkillError> {
    validate_slug(slug)?;
    if name.trim().is_empty() || description.trim().is_empty() || instructions.trim().is_empty() {
        return Err(SkillError::Invalid(
            "skill name, description, and instructions must not be empty".to_string(),
        ));
    }
    ensure_project(store, project_id)?;
    if store
        .list_managed_directories("skills")?
        .iter()
        .any(|existing| existing == slug)
    {
        return Err(SkillError::AlreadyExists(slug.to_string()));
    }
    let document = format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
        yaml_scalar(name.trim())?,
        yaml_scalar(description.trim())?,
        instructions.trim()
    );
    if let Err(error) = store.write_managed_file(
        PathBuf::from("skills").join(slug).join("SKILL.md"),
        document.as_bytes(),
    ) {
        let _ = store.remove_managed_entry(PathBuf::from("skills").join(slug));
        return Err(error.into());
    }
    load_skill(store, project_id, slug)
}

fn resolve_skills(
    store: &Store,
    project_id: ProjectId,
    slugs: &[String],
) -> Result<ResolvedSkills, SkillError> {
    let mut snapshots = Vec::new();
    let mut sections = Vec::new();
    for slug in unique_slugs(slugs)? {
        let skill = load_skill(store, project_id, slug)?;
        snapshots.push(SkillSnapshot {
            slug: slug.to_string(),
            sha256: skill.sha256.clone(),
        });
        sections.push(format!(
            "## Skill: {} (`{}`)\n{}\n\n{}",
            skill.name, slug, skill.description, skill.instructions
        ));
    }
    Ok(ResolvedSkills {
        snapshots,
        instructions: if sections.is_empty() {
            String::new()
        } else {
            format!(
                "Project-local skills enabled for this turn:\n\n{}",
                sections.join("\n\n")
            )
        },
    })
}

fn parse_skill_markdown(source: &str) -> Result<(SkillFrontmatter, &str), SkillError> {
    let mut lines = source.lines();
    if lines.next() != Some("---") {
        return Err(SkillError::Invalid(
            "SKILL.md must start with YAML frontmatter".to_string(),
        ));
    }
    let mut yaml = Vec::new();
    let mut offset = 4_usize;
    let mut found_end = false;
    for line in lines {
        offset = offset.saturating_add(line.len() + 1);
        if line == "---" {
            found_end = true;
            break;
        }
        yaml.push(line);
    }
    if !found_end {
        return Err(SkillError::Invalid(
            "SKILL.md frontmatter is not closed".to_string(),
        ));
    }
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(&yaml.join("\n"))?;
    if frontmatter.name.trim().is_empty() || frontmatter.description.trim().is_empty() {
        return Err(SkillError::Invalid(
            "skill name and description must not be empty".to_string(),
        ));
    }
    let instructions = source.get(offset..).unwrap_or_default().trim();
    if instructions.is_empty() {
        return Err(SkillError::Invalid(
            "SKILL.md instructions must not be empty".to_string(),
        ));
    }
    Ok((frontmatter, instructions))
}

fn yaml_scalar(value: &str) -> Result<String, SkillError> {
    let serialized = serde_yaml::to_string(value)?;
    Ok(serialized.trim().to_string())
}

fn validate_slug(slug: &str) -> Result<(), SkillError> {
    let valid = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(SkillError::Invalid(format!("invalid skill slug: {slug}")))
    }
}

fn unique_slugs(slugs: &[String]) -> Result<Vec<&str>, SkillError> {
    let mut unique = BTreeSet::new();
    for slug in slugs {
        validate_slug(slug)?;
        unique.insert(slug.as_str());
    }
    Ok(unique.into_iter().collect())
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("skill already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid skill: {0}")]
    Invalid(String),
    #[error("skill frontmatter is invalid: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
