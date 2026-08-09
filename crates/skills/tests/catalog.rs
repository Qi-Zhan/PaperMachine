use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::Store;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn skill_packages_are_hashed_materialized_and_recovered_from_snapshots() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Arc::new(Store::open_in_memory(&managed).expect("store should open"));
    let project = store
        .create_project("Skills", directory.path().join("project"))
        .expect("project should be created");
    let catalog = ProjectSkillCatalog::new(store);
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
    assert!(managed.join(&resolved.snapshots[0].relative_path).is_file());

    let package = managed.join("skills/source-audit/SKILL.md");
    std::fs::write(
        package,
        "---\nname: Changed\ndescription: Changed later.\n---\n\nChanged instructions.\n",
    )
    .expect("source skill should change");
    let recovered = catalog
        .resolve_snapshots(project_id, &resolved.snapshots)
        .expect("snapshot should remain reproducible");
    assert!(recovered.contains("Open the primary source"));
    assert!(!recovered.contains("Changed instructions"));
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
    let catalog = ProjectSkillCatalog::new(store);
    let project_id = project.id;
    assert!(
        catalog
            .create(project_id, "../escape", "Bad", "Bad", "Bad")
            .is_err()
    );
    let package = catalog
        .ensure_project(project_id)
        .expect("managed Project root should exist")
        .join("skills/incomplete");
    std::fs::create_dir_all(&package).expect("package should exist");
    std::fs::write(package.join("SKILL.md"), "No frontmatter")
        .expect("invalid skill should be written");
    assert!(catalog.load(project_id, "incomplete").is_err());
}
