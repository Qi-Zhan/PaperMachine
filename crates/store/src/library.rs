use crate::StoreError;
use chrono::Utc;
use papermachine_protocol::Project;
use papermachine_protocol::ProjectId;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
pub struct ProjectLibrary {
    connection: Arc<Mutex<Connection>>,
}

impl ProjectLibrary {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let connection = Connection::open(path)?;
        initialize(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        initialize(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn register(&self, project: &Project) -> Result<(), StoreError> {
        let connection = self.connection()?;
        let duplicate_path: Option<String> = connection
            .query_row(
                "SELECT id FROM projects WHERE workspace_path = ?1 AND id != ?2",
                params![project.workspace_path, project.id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate_path.is_some() {
            return Err(StoreError::Invariant(format!(
                "Project Workspace is already registered: {}",
                project.workspace_path
            )));
        }
        connection.execute(
            "INSERT INTO projects (id, workspace_path, document_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
               workspace_path = excluded.workspace_path,
               document_json = excluded.document_json,
               updated_at = excluded.updated_at",
            params![
                project.id.to_string(),
                project.workspace_path,
                serde_json::to_string(project)?,
                project.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Project>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT document_json FROM projects ORDER BY updated_at DESC, id ASC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(serde_json::from_str(&row?)?);
        }
        Ok(projects)
    }

    pub fn get(&self, id: ProjectId) -> Result<Project, StoreError> {
        let connection = self.connection()?;
        let document = connection
            .query_row(
                "SELECT document_json FROM projects WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "project",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&document)?)
    }

    pub fn remove(&self, id: ProjectId) -> Result<Project, StoreError> {
        let project = self.get(id)?;
        self.connection()?
            .execute("DELETE FROM projects WHERE id = ?1", [id.to_string()])?;
        Ok(project)
    }

    pub fn update(&self, project: &Project) -> Result<(), StoreError> {
        let mut project = project.clone();
        project.updated_at = Utc::now();
        self.register(&project)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::LockPoisoned)
    }
}

fn initialize(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS projects (
           id TEXT PRIMARY KEY,
           workspace_path TEXT NOT NULL UNIQUE,
           document_json TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &str) -> Project {
        let now = Utc::now();
        Project {
            id: ProjectId::new(),
            name: "Research".to_string(),
            description: String::new(),
            workspace_path: root.to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn registers_lists_and_removes_project_references() {
        let library = ProjectLibrary::open_in_memory().expect("library should open");
        let project = project("/research/paper");
        library.register(&project).expect("project should register");

        assert_eq!(
            library.list().expect("projects should list"),
            vec![project.clone()]
        );
        assert_eq!(
            library.remove(project.id).expect("project should remove"),
            project
        );
        assert!(library.list().expect("projects should list").is_empty());
    }

    #[test]
    fn one_directory_can_only_identify_one_project() {
        let library = ProjectLibrary::open_in_memory().expect("library should open");
        library
            .register(&project("/research/paper"))
            .expect("first project should register");
        let error = library
            .register(&project("/research/paper"))
            .expect_err("duplicate directory should fail");
        assert!(error.to_string().contains("already registered"));
    }
}
