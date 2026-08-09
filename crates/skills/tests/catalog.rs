use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::Store;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn skill_instructions_are_hashed_and_resolved() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Arc::new(Store::open_in_memory(&managed).expect("store should open"));
    let project = store
        .create_project("Skills", directory.path().join("project"))
        .expect("project should be created");
    let catalog = ProjectSkillCatalog::new(Arc::clone(&store));
    let project_id = project.id;
    let skill = catalog
        .create(
            project_id,
            "source-audit",
            "Source audit",
            "Trace claims back to primary evidence.",
            "Open the primary source before accepting a claim.",
        )
        .expect("skill should be created");
    assert_eq!(skill.sha256.len(), 64);
    assert_eq!(
        catalog.list(project_id).expect("skills should list").len(),
        1
    );

    let resolved = catalog
        .resolve(project_id, &["source-audit".to_string()])
        .expect("skill should resolve");
    assert!(resolved.instructions.contains("Open the primary source"));
    assert_eq!(resolved.snapshots[0].slug, "source-audit");
    assert_eq!(resolved.snapshots[0].sha256, skill.sha256);

    store
        .write_managed_file(
            "skills/source-audit/SKILL.md",
            b"---\nname: Changed\ndescription: Changed later.\n---\n\nChanged instructions.\n",
        )
        .expect("source skill should change");
    let refreshed = catalog
        .resolve(project_id, &["source-audit".to_string()])
        .expect("updated skill should resolve for a future Turn");
    assert_ne!(refreshed.snapshots[0].sha256, resolved.snapshots[0].sha256);
    assert!(refreshed.instructions.contains("Changed instructions"));
    assert!(resolved.instructions.contains("Open the primary source"));
}

#[test]
fn invalid_slugs_and_incomplete_skill_documents_are_rejected() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = store
        .create_project("Invalid skills", directory.path().join("project"))
        .expect("project should be created");
    let catalog = ProjectSkillCatalog::new(Arc::clone(&store));
    let project_id = project.id;
    assert!(
        catalog
            .create(project_id, "../escape", "Bad", "Bad", "Bad")
            .is_err()
    );
    catalog
        .ensure_project(project_id)
        .expect("managed Project root should exist");
    store
        .write_managed_file("skills/incomplete/SKILL.md", b"No frontmatter")
        .expect("invalid skill should be written");
    assert!(catalog.load(project_id, "incomplete").is_err());
}
