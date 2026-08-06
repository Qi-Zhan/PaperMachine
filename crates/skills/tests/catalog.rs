use papermachine_protocol::ResearchId;
use papermachine_skills::ResearchSkillCatalog;
use tempfile::tempdir;

#[test]
fn skill_packages_are_hashed_materialized_and_recovered_from_snapshots() {
    let directory = tempdir().expect("temporary directory should be created");
    let catalog = ResearchSkillCatalog::new(directory.path().join("researches"));
    let research_id = ResearchId::new();
    let skill = catalog
        .create(
            research_id,
            "source-audit",
            "Source audit",
            "Trace claims back to primary evidence.",
            "Open the primary source before accepting a claim.",
        )
        .expect("skill should be created");
    assert_eq!(skill.sha256.len(), 64);
    assert_eq!(
        catalog.list(research_id).expect("skills should list").len(),
        1
    );

    let workspace = directory.path().join("session-workspace");
    let resolved = catalog
        .resolve(research_id, &["source-audit".to_string()], &workspace)
        .expect("skill should resolve");
    assert!(resolved.instructions.contains("Open the primary source"));
    assert!(
        workspace
            .join(&resolved.snapshots[0].relative_path)
            .is_file()
    );

    let package = directory
        .path()
        .join("researches")
        .join(research_id.to_string())
        .join("skills/source-audit/SKILL.md");
    std::fs::write(
        package,
        "---\nname: Changed\ndescription: Changed later.\n---\n\nChanged instructions.\n",
    )
    .expect("source skill should change");
    let recovered = catalog
        .resolve_snapshots(&workspace, &resolved.snapshots)
        .expect("snapshot should remain reproducible");
    assert!(recovered.contains("Open the primary source"));
    assert!(!recovered.contains("Changed instructions"));
}

#[test]
fn invalid_slugs_and_incomplete_skill_documents_are_rejected() {
    let directory = tempdir().expect("temporary directory should be created");
    let catalog = ResearchSkillCatalog::new(directory.path());
    let research_id = ResearchId::new();
    assert!(
        catalog
            .create(research_id, "../escape", "Bad", "Bad", "Bad")
            .is_err()
    );
    let package = catalog
        .ensure_research(research_id)
        .expect("research root should exist")
        .join("skills/incomplete");
    std::fs::create_dir_all(&package).expect("package should exist");
    std::fs::write(package.join("SKILL.md"), "No frontmatter")
        .expect("invalid skill should be written");
    assert!(catalog.load(research_id, "incomplete").is_err());
}
