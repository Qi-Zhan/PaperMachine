//! Agent rollout persistence, adapted from OpenAI Codex's one-live-writer
//! and durable-write-before-SQLite-projection design at commit
//! `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`.
//!
//! PaperMachine keeps its own Project/Session records and compact JSONL item
//! schema with PaperMachine-owned entity and durability semantics.

use crate::StoreError;
use papermachine_protocol::AGENT_ROLLOUT_VERSION;
use papermachine_protocol::ActiveTurnRolloutState;
use papermachine_protocol::AgentId;
use papermachine_protocol::AgentRolloutItem;
use papermachine_protocol::AgentRolloutRecord;
use papermachine_protocol::AgentRolloutState;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::TurnStatus;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn path(root: &Path, agent_id: AgentId) -> PathBuf {
    root.join(format!("{agent_id}.jsonl"))
}

pub(crate) fn read(root: &Path, agent_id: AgentId) -> Result<Vec<AgentRolloutRecord>, StoreError> {
    let mut records = Vec::new();
    scan(root, agent_id, |record| {
        records.push(record);
        Ok(())
    })?;
    Ok(records)
}

pub(crate) fn read_after(
    root: &Path,
    agent_id: AgentId,
    sequence: u64,
) -> Result<(u64, Vec<AgentRolloutRecord>), StoreError> {
    let mut records = Vec::new();
    let last_sequence = scan(root, agent_id, |record| {
        if record.sequence > sequence {
            records.push(record);
        }
        Ok(())
    })?;
    Ok((last_sequence, records))
}

pub(crate) fn last_sequence(root: &Path, agent_id: AgentId) -> Result<u64, StoreError> {
    scan(root, agent_id, |_| Ok(()))
}

pub(crate) fn reconstruct_file(
    root: &Path,
    agent_id: AgentId,
) -> Result<AgentRolloutState, StoreError> {
    let mut state = AgentRolloutState::default();
    scan(root, agent_id, |record| apply_record(&mut state, &record))?;
    Ok(state)
}

fn scan(
    root: &Path,
    agent_id: AgentId,
    mut visit: impl FnMut(AgentRolloutRecord) -> Result<(), StoreError>,
) -> Result<u64, StoreError> {
    let path = path(root, agent_id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Agent rollout is not a real file: {}",
            path.display()
        )));
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut complete_len = 0_u64;
    let mut expected_sequence = 1_u64;
    let mut accepted_trailing = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        if read == 0 {
            break;
        }
        let complete = line.last() == Some(&b'\n');
        let document = if complete {
            &line[..line.len() - 1]
        } else {
            line.as_slice()
        };
        if document.is_empty() {
            return Err(StoreError::Invariant(format!(
                "empty interior Agent rollout record {} in {}",
                expected_sequence,
                path.display()
            )));
        }
        let record = match serde_json::from_slice::<AgentRolloutRecord>(document) {
            Ok(record) => record,
            Err(_) if !complete => {
                let file = reader.into_inner();
                file.set_len(complete_len)
                    .map_err(|error| StoreError::Io(error.to_string()))?;
                file.sync_data()
                    .map_err(|error| StoreError::Io(error.to_string()))?;
                return Ok(expected_sequence - 1);
            }
            Err(error) => {
                return Err(StoreError::Invariant(format!(
                    "invalid Agent rollout record {expected_sequence} in {}: {error}",
                    path.display()
                )));
            }
        };
        validate_record(&record, agent_id, expected_sequence, &path)?;
        visit(record)?;
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| StoreError::Invariant("Agent rollout sequence overflow".to_string()))?;
        if complete {
            complete_len = complete_len.saturating_add(read as u64);
        } else {
            accepted_trailing = true;
            break;
        }
    }
    let mut file = reader.into_inner();
    if accepted_trailing {
        file.seek(SeekFrom::End(0))
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.write_all(b"\n")
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.flush()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.sync_data()
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    Ok(expected_sequence - 1)
}

pub(crate) fn append(root: &Path, record: &AgentRolloutRecord) -> Result<(), StoreError> {
    let path = path(root, record.agent_id);
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
        .unwrap_or(false)
    {
        return Err(StoreError::Invariant(format!(
            "Agent rollout is not a real file: {}",
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

fn apply_record(
    state: &mut AgentRolloutState,
    record: &AgentRolloutRecord,
) -> Result<(), StoreError> {
    match &record.item {
        AgentRolloutItem::TurnCreated { turn, .. } => {
            if state.active_turn.is_some() {
                return Err(StoreError::Invariant(format!(
                    "Agent rollout created Turn {} while another Turn was active",
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
        AgentRolloutItem::ContextCheckpoint {
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
                    "Agent rollout checkpoint for inactive Turn {turn_id}"
                ))
            })?;
            if active.turn_id != *turn_id {
                return Err(StoreError::Invariant(format!(
                    "Agent rollout checkpoint belongs to Turn {turn_id}, not active Turn {}",
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
        AgentRolloutItem::TurnUpdated { turn, .. } => {
            let active = state.active_turn.as_ref().ok_or_else(|| {
                StoreError::Invariant(format!("Agent rollout updated inactive Turn {}", turn.id))
            })?;
            if active.turn_id != turn.id {
                return Err(StoreError::Invariant(format!(
                    "Agent rollout updated Turn {}, not active Turn {}",
                    turn.id, active.turn_id
                )));
            }
            match turn.status {
                TurnStatus::Completed | TurnStatus::Interrupted => {
                    state.committed_context.clone_from(&active.context);
                    state.active_turn = None;
                }
                TurnStatus::Failed | TurnStatus::Cancelled => {
                    state.active_turn = None;
                }
                TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused => {}
            }
        }
    }
    Ok(())
}

fn validate_record(
    record: &AgentRolloutRecord,
    agent_id: AgentId,
    expected: u64,
    path: &Path,
) -> Result<(), StoreError> {
    if record.version != AGENT_ROLLOUT_VERSION {
        return Err(StoreError::Invariant(format!(
            "unsupported Agent rollout version {} in {}",
            record.version,
            path.display()
        )));
    }
    if record.agent_id != agent_id {
        return Err(StoreError::Invariant(format!(
            "Agent rollout {} contains record for {}",
            path.display(),
            record.agent_id
        )));
    }
    if record.sequence != expected {
        return Err(StoreError::Invariant(format!(
            "Agent rollout {} expected sequence {expected}, found {}",
            path.display(),
            record.sequence
        )));
    }
    Ok(())
}
