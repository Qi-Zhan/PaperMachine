//! Session rollout persistence, adapted from OpenAI Codex's one-live-writer
//! and durable-write-before-SQLite-projection design at commit
//! `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`.
//!
//! PaperMachine keeps its own Project/Session records and compact JSONL item
//! schema with PaperMachine-owned entity and durability semantics.

use crate::StoreError;
use papermachine_protocol::ActiveTurnRolloutState;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::SESSION_ROLLOUT_VERSION;
use papermachine_protocol::SessionId;
use papermachine_protocol::SessionRolloutItem;
use papermachine_protocol::SessionRolloutRecord;
use papermachine_protocol::SessionRolloutState;
use papermachine_protocol::TurnStatus;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join(format!("{session_id}.jsonl"))
}

pub(crate) fn read(
    root: &Path,
    session_id: SessionId,
) -> Result<Vec<SessionRolloutRecord>, StoreError> {
    let path = path(root, session_id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Session rollout is not a real file: {}",
            path.display()
        )));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;

    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let trailing = &bytes[complete_len..];
    let mut accepted_trailing = None;
    if !trailing.is_empty() {
        match serde_json::from_slice::<SessionRolloutRecord>(trailing) {
            Ok(record) => accepted_trailing = Some(record),
            Err(_) => {
                // A process may stop between bytes of its final append. Only
                // the incomplete tail is recoverable; an invalid interior
                // record remains a hard corruption error below.
                file.set_len(complete_len as u64)
                    .map_err(|error| StoreError::Io(error.to_string()))?;
                file.sync_data()
                    .map_err(|error| StoreError::Io(error.to_string()))?;
            }
        }
    }

    let mut records = Vec::new();
    let lines = bytes[..complete_len].split(|byte| *byte == b'\n');
    let line_count = lines.clone().count();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            if index + 1 == line_count {
                continue;
            }
            return Err(StoreError::Invariant(format!(
                "empty interior Session rollout record {} in {}",
                index + 1,
                path.display()
            )));
        }
        let record = serde_json::from_slice(line).map_err(|error| {
            StoreError::Invariant(format!(
                "invalid Session rollout record {} in {}: {error}",
                index + 1,
                path.display()
            ))
        })?;
        records.push(record);
    }
    if let Some(record) = accepted_trailing {
        records.push(record);
        file.seek(SeekFrom::End(0))
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.write_all(b"\n")
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.flush()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.sync_data()
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    validate(&records, session_id, &path)?;
    Ok(records)
}

pub(crate) fn append(root: &Path, record: &SessionRolloutRecord) -> Result<(), StoreError> {
    let path = path(root, record.session_id);
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
        .unwrap_or(false)
    {
        return Err(StoreError::Invariant(format!(
            "Session rollout is not a real file: {}",
            path.display()
        )));
    }
    let existed = path.exists();
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    file.flush()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    file.sync_data()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    #[cfg(unix)]
    if !existed {
        File::open(root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    Ok(())
}

pub(crate) fn reconstruct(
    records: &[SessionRolloutRecord],
) -> Result<SessionRolloutState, StoreError> {
    let mut state = SessionRolloutState::default();
    for record in records {
        match &record.item {
            SessionRolloutItem::TurnCreated { turn, .. } => {
                if state.active_turn.is_some() {
                    return Err(StoreError::Invariant(format!(
                        "Session rollout created Turn {} while another Turn was active",
                        turn.id
                    )));
                }
                state.active_turn = Some(ActiveTurnRolloutState {
                    turn_id: turn.id,
                    context: state.committed_context.clone(),
                    has_checkpoint: false,
                    usage: Default::default(),
                    completed_model_steps: 0,
                    hosted_search_calls_used: 0,
                    checkpoint_message: None,
                });
            }
            SessionRolloutItem::ContextCheckpoint {
                turn_id,
                mutation,
                usage,
                completed_model_steps,
                hosted_search_calls_used,
                checkpoint_message,
                ..
            } => {
                let active = state.active_turn.as_mut().ok_or_else(|| {
                    StoreError::Invariant(format!(
                        "Session rollout checkpoint for inactive Turn {turn_id}"
                    ))
                })?;
                if active.turn_id != *turn_id {
                    return Err(StoreError::Invariant(format!(
                        "Session rollout checkpoint belongs to Turn {turn_id}, not active Turn {}",
                        active.turn_id
                    )));
                }
                match mutation {
                    ModelContextMutation::Unchanged => {}
                    ModelContextMutation::Append { items } => {
                        active.context.extend(items.iter().cloned());
                    }
                    ModelContextMutation::Replace { items, .. } => {
                        active.context.clone_from(items);
                    }
                }
                active.has_checkpoint = true;
                active.usage = *usage;
                active.completed_model_steps = *completed_model_steps;
                active.hosted_search_calls_used = *hosted_search_calls_used;
                active.checkpoint_message.clone_from(checkpoint_message);
            }
            SessionRolloutItem::TurnUpdated { turn, .. } => {
                let active = state.active_turn.as_ref().ok_or_else(|| {
                    StoreError::Invariant(format!(
                        "Session rollout updated inactive Turn {}",
                        turn.id
                    ))
                })?;
                if active.turn_id != turn.id {
                    return Err(StoreError::Invariant(format!(
                        "Session rollout updated Turn {}, not active Turn {}",
                        turn.id, active.turn_id
                    )));
                }
                match turn.status {
                    TurnStatus::Completed => {
                        state.committed_context.clone_from(&active.context);
                        state.active_turn = None;
                    }
                    TurnStatus::Interrupted => {
                        state.committed_context.clone_from(&active.context);
                        state.active_turn = None;
                    }
                    TurnStatus::Failed | TurnStatus::Cancelled => {
                        state.active_turn = None;
                    }
                    TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused => {}
                }
            }
            SessionRolloutItem::StepsCreated { .. }
            | SessionRolloutItem::StepsUpdated { .. }
            | SessionRolloutItem::SessionEventAppended { .. } => {}
        }
    }
    Ok(state)
}

fn validate(
    records: &[SessionRolloutRecord],
    session_id: SessionId,
    path: &Path,
) -> Result<(), StoreError> {
    for (index, record) in records.iter().enumerate() {
        let expected = index as u64 + 1;
        if record.version != SESSION_ROLLOUT_VERSION {
            return Err(StoreError::Invariant(format!(
                "unsupported Session rollout version {} in {}",
                record.version,
                path.display()
            )));
        }
        if record.session_id != session_id {
            return Err(StoreError::Invariant(format!(
                "Session rollout {} contains record for {}",
                path.display(),
                record.session_id
            )));
        }
        if record.sequence != expected {
            return Err(StoreError::Invariant(format!(
                "Session rollout {} expected sequence {expected}, found {}",
                path.display(),
                record.sequence
            )));
        }
    }
    Ok(())
}
