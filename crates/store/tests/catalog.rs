use papermachine_store::ProjectCatalog;
use tempfile::tempdir;

#[test]
fn project_catalog_uses_atomic_managed_directories_and_preserves_workspace() {
    let directory = tempdir().expect("temporary root should be created");
    let data = directory.path().join("data");
    let workspace = directory.path().join("user/research");
    let catalog = ProjectCatalog::open(&data).expect("catalog should open");
    let created = catalog
        .create_project("Research world", &workspace)
        .expect("Project should be created");
    let project_id = created.project.id;
    let managed = data.join("projects").join(project_id.to_string());

    assert!(managed.join("state/project.db").is_file());
    assert!(
        std::fs::read_dir(data.join("staging"))
            .expect("staging should list")
            .next()
            .is_none()
    );
    std::fs::write(workspace.join("evidence.md"), "user-owned")
        .expect("Workspace fixture should be written");
    drop(created);

    let reopened = ProjectCatalog::open(&data).expect("catalog should reopen");
    let scanned = reopened.scan().expect("catalog should scan");
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].project.id, project_id);
    drop(scanned);

    let trash = reopened
        .retire_project(project_id)
        .expect("Project should move to trash");
    assert!(!managed.exists());
    assert!(trash.is_dir());
    assert_eq!(
        std::fs::read_to_string(workspace.join("evidence.md"))
            .expect("Workspace evidence should remain"),
        "user-owned"
    );
    reopened
        .purge_trash_entry(&trash)
        .expect("trash entry should purge");
    assert!(!trash.exists());
    assert!(workspace.is_dir());
}

#[test]
fn catalog_rejects_a_workspace_attached_to_another_project() {
    let directory = tempdir().expect("temporary root should be created");
    let catalog = ProjectCatalog::open(directory.path().join("data")).expect("catalog should open");
    let workspace = directory.path().join("workspace");
    catalog
        .create_project("First", &workspace)
        .expect("first Project should be created");
    let error = catalog
        .create_project("Second", &workspace)
        .err()
        .expect("duplicate Workspace must fail");
    assert!(error.to_string().contains("another Project"));
}

#[test]
fn startup_quarantines_unpublished_staging_state() {
    let directory = tempdir().expect("temporary root should be created");
    let catalog = ProjectCatalog::open(directory.path()).expect("catalog should open");
    let abandoned = directory.path().join("staging/abandoned");
    std::fs::create_dir_all(&abandoned).expect("abandoned staging should be created");
    std::fs::write(abandoned.join("partial"), "incomplete")
        .expect("partial state should be written");

    let quarantined = catalog
        .quarantine_staging()
        .expect("staging should quarantine");
    assert_eq!(quarantined.len(), 1);
    assert!(!abandoned.exists());
    assert_eq!(
        quarantined[0].parent(),
        Some(catalog.data_root().join("trash").as_path())
    );
    catalog
        .purge_trash_entry(&quarantined[0])
        .expect("quarantine should purge");
}
