//! Project-local skill packages using the Codex `SKILL.md` shape.

use papermachine_protocol::ProjectId;
use papermachine_protocol::ProjectSkill;
use papermachine_protocol::SkillSnapshot;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone)]
pub struct ProjectSkillCatalog {
    store: Arc<Store>,
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
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn ensure_project(&self, project_id: ProjectId) -> Result<PathBuf, SkillError> {
        let root = self.project_root(project_id)?;
        std::fs::create_dir_all(root.join("skills"))?;
        std::fs::create_dir_all(root.join("sources"))?;
        Ok(root)
    }

    pub fn list(&self, project_id: ProjectId) -> Result<Vec<ProjectSkill>, SkillError> {
        let skills_root = self.ensure_project(project_id)?.join("skills");
        let mut directories = std::fs::read_dir(&skills_root)?.collect::<Result<Vec<_>, _>>()?;
        directories.sort_by_key(std::fs::DirEntry::file_name);
        let mut skills = Vec::new();
        for entry in directories {
            if entry.file_type()?.is_dir() {
                skills.push(self.load(project_id, &entry.file_name().to_string_lossy())?);
            }
        }
        Ok(skills)
    }

    pub fn load(&self, project_id: ProjectId, slug: &str) -> Result<ProjectSkill, SkillError> {
        validate_slug(slug)?;
        let package = self.project_root(project_id)?.join("skills").join(slug);
        let skill_path = package.join("SKILL.md");
        if !skill_path.is_file() {
            return Err(SkillError::NotFound(slug.to_string()));
        }
        ensure_package_is_regular(&package, &package)?;
        let source = std::fs::read_to_string(&skill_path)?;
        let (frontmatter, instructions) = parse_skill_markdown(&source)?;
        Ok(ProjectSkill {
            slug: slug.to_string(),
            name: frontmatter.name,
            description: frontmatter.description,
            relative_path: format!(".papermachine/skills/{slug}/SKILL.md"),
            sha256: hash_package(&package)?,
            instructions: instructions.to_string(),
        })
    }

    pub fn create(
        &self,
        project_id: ProjectId,
        slug: &str,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<ProjectSkill, SkillError> {
        validate_slug(slug)?;
        if name.trim().is_empty() || description.trim().is_empty() || instructions.trim().is_empty()
        {
            return Err(SkillError::Invalid(
                "skill name, description, and instructions must not be empty".to_string(),
            ));
        }
        let package = self.ensure_project(project_id)?.join("skills").join(slug);
        if package.exists() {
            return Err(SkillError::AlreadyExists(slug.to_string()));
        }
        std::fs::create_dir_all(&package)?;
        let document = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            yaml_scalar(name.trim())?,
            yaml_scalar(description.trim())?,
            instructions.trim()
        );
        let temporary = package.join("SKILL.md.tmp");
        std::fs::write(&temporary, document)?;
        std::fs::rename(&temporary, package.join("SKILL.md"))?;
        self.load(project_id, slug)
    }

    pub fn validate_enabled(
        &self,
        project_id: ProjectId,
        slugs: &[String],
    ) -> Result<(), SkillError> {
        for slug in unique_slugs(slugs)? {
            self.load(project_id, slug)?;
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        project_id: ProjectId,
        slugs: &[String],
        session_workspace: &Path,
    ) -> Result<ResolvedSkills, SkillError> {
        let mut snapshots = Vec::new();
        let mut sections = Vec::new();
        for slug in unique_slugs(slugs)? {
            let skill = self.load(project_id, slug)?;
            let hash_prefix = skill.sha256.get(..16).unwrap_or(&skill.sha256);
            let relative_package = PathBuf::from(".papermachine")
                .join("state")
                .join("skill-snapshots")
                .join(slug)
                .join(hash_prefix);
            let destination = session_workspace.join(&relative_package);
            if !destination.exists() {
                copy_package(
                    &self.project_root(project_id)?.join("skills").join(slug),
                    &destination,
                )?;
            }
            let relative_path = relative_package
                .join("SKILL.md")
                .to_string_lossy()
                .into_owned();
            snapshots.push(SkillSnapshot {
                slug: slug.to_string(),
                sha256: skill.sha256.clone(),
                relative_path: relative_path.clone(),
            });
            sections.push(format!(
                "## Skill: {} (`{}`)\nPackage: `{}`\n{}\n\n{}",
                skill.name, slug, relative_path, skill.description, skill.instructions
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

    pub fn resolve_snapshots(
        &self,
        session_workspace: &Path,
        snapshots: &[SkillSnapshot],
    ) -> Result<String, SkillError> {
        let mut sections = Vec::new();
        for snapshot in snapshots {
            let path = safe_relative_path(&snapshot.relative_path)?;
            let package = session_workspace
                .join(path)
                .parent()
                .ok_or_else(|| {
                    SkillError::Invalid("skill snapshot path has no package directory".to_string())
                })?
                .to_path_buf();
            if hash_package(&package)? != snapshot.sha256 {
                return Err(SkillError::SnapshotChanged(snapshot.slug.clone()));
            }
            let source = std::fs::read_to_string(package.join("SKILL.md"))?;
            let (frontmatter, instructions) = parse_skill_markdown(&source)?;
            sections.push(format!(
                "## Skill: {} (`{}`)\nPackage: `{}`\n{}\n\n{}",
                frontmatter.name,
                snapshot.slug,
                snapshot.relative_path,
                frontmatter.description,
                instructions
            ));
        }
        Ok(if sections.is_empty() {
            String::new()
        } else {
            format!(
                "Project-local skills enabled for this turn:\n\n{}",
                sections.join("\n\n")
            )
        })
    }

    fn project_root(&self, project_id: ProjectId) -> Result<PathBuf, SkillError> {
        Ok(PathBuf::from(self.store.get_project(project_id)?.root_path).join(".papermachine"))
    }
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

fn safe_relative_path(value: &str) -> Result<&Path, SkillError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(SkillError::Invalid(format!(
            "skill snapshot path escapes workspace: {value}"
        )));
    }
    Ok(path)
}

fn hash_package(package: &Path) -> Result<String, SkillError> {
    let mut files = Vec::new();
    collect_files(package, package, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(package.join(&relative))?);
        hasher.update([0]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_files(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<(), SkillError> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(SkillError::Invalid(format!(
                "skill packages may not contain symlinks: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|error| SkillError::Invalid(error.to_string()))?
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

fn ensure_package_is_regular(root: &Path, current: &Path) -> Result<(), SkillError> {
    let mut files = Vec::new();
    collect_files(root, current, &mut files)
}

fn copy_package(source: &Path, destination: &Path) -> Result<(), SkillError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(SkillError::Invalid(format!(
                "skill packages may not contain symlinks: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            copy_package(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("skill already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid skill package: {0}")]
    Invalid(String),
    #[error("skill snapshot changed after turn creation: {0}")]
    SnapshotChanged(String),
    #[error("skill I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("skill frontmatter is invalid: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
