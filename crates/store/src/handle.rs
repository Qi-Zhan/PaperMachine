use crate::Store;
use crate::StoreError;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub const STORE_QUEUE_CAPACITY: usize = 256;

type StoreJob = Box<dyn FnOnce(&Store) + Send + 'static>;

enum StoreCommand {
    Run(StoreJob),
    Shutdown(oneshot::Sender<()>),
}

struct StoreHandleInner {
    sender: mpsc::Sender<StoreCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    managed_root: PathBuf,
}

/// Bounded asynchronous access to one Project's synchronous Store core.
///
/// The core and its SQLite connection live on exactly one dedicated blocking
/// thread. Callers submit owned jobs through a bounded queue, so database,
/// hashing, and managed-filesystem work never runs on a Tokio worker.
#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<StoreHandleInner>,
}

impl StoreHandle {
    pub fn spawn(store: Store) -> Result<Self, StoreError> {
        let managed_root = store.managed_root().to_path_buf();
        let (sender, mut receiver) = mpsc::channel(STORE_QUEUE_CAPACITY);
        let thread = std::thread::Builder::new()
            .name(format!(
                "papermachine-store-{}",
                managed_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("project")
            ))
            .spawn(move || {
                while let Some(command) = receiver.blocking_recv() {
                    match command {
                        StoreCommand::Run(job) => job(&store),
                        StoreCommand::Shutdown(acknowledged) => {
                            let _ = acknowledged.send(());
                            break;
                        }
                    }
                }
            })
            .map_err(|error| StoreError::Io(error.to_string()))?;
        Ok(Self {
            inner: Arc::new(StoreHandleInner {
                sender,
                thread: Mutex::new(Some(thread)),
                stopped: AtomicBool::new(false),
                managed_root,
            }),
        })
    }

    pub fn managed_root(&self) -> &Path {
        &self.inner.managed_root
    }

    pub async fn call<T, E, F>(&self, operation: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
        F: FnOnce(&Store) -> Result<T, E> + Send + 'static,
    {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(E::from(StoreError::Invariant(
                "Project Store has been stopped".to_string(),
            )));
        }
        let (result_tx, result_rx) = oneshot::channel();
        self.inner
            .sender
            .send(StoreCommand::Run(Box::new(move |store| {
                let result = operation(store);
                let _ = result_tx.send(result);
            })))
            .await
            .map_err(|_| {
                E::from(StoreError::Invariant(
                    "Project Store thread stopped".to_string(),
                ))
            })?;
        result_rx.await.map_err(|_| {
            E::from(StoreError::Invariant(
                "Project Store job was abandoned".to_string(),
            ))
        })?
    }

    pub async fn shutdown(&self) -> Result<(), StoreError> {
        if self.inner.stopped.swap(true, Ordering::AcqRel) {
            return self.join_thread().await;
        }
        let (acknowledged_tx, acknowledged_rx) = oneshot::channel();
        if self
            .inner
            .sender
            .send(StoreCommand::Shutdown(acknowledged_tx))
            .await
            .is_err()
        {
            return match self.join_thread().await {
                Ok(()) => Err(StoreError::Invariant(
                    "Project Store thread stopped before shutdown".to_string(),
                )),
                Err(error) => Err(error),
            };
        }
        acknowledged_rx.await.map_err(|_| {
            StoreError::Invariant("Project Store shutdown was abandoned".to_string())
        })?;
        self.join_thread().await
    }

    async fn join_thread(&self) -> Result<(), StoreError> {
        let thread = self
            .inner
            .thread
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .take();
        let Some(thread) = thread else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|error| StoreError::Invariant(format!("Store join task failed: {error}")))?
            .map_err(|_| StoreError::Invariant("Project Store thread panicked".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn jobs_are_isolated_on_the_store_thread() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
        let project = store
            .create_project("Handle", directory.path().join("workspace"))
            .expect("Project should be created");
        let caller_thread = std::thread::current().id();
        let handle = StoreHandle::spawn(store).expect("Store thread should start");

        let store_thread = handle
            .call(|_| Ok::<_, StoreError>(std::thread::current().id()))
            .await
            .expect("Store job should run");
        assert_ne!(store_thread, caller_thread);

        let project_id = project.id;
        handle
            .call(move |store| store.create_session(project_id, "Session", "", "model", Vec::new()))
            .await
            .expect("Session should be created");

        handle.shutdown().await.expect("Store should stop cleanly");
        let error = handle
            .call(|_| Ok::<_, StoreError>(()))
            .await
            .expect_err("stopped Store must fail closed");
        assert!(error.to_string().contains("stopped"));
    }
}
