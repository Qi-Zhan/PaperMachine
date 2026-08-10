use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use crate::path::create_resolved_file;
use crate::path::read_resolved_file;
use crate::path::remove_resolved_file;
use crate::path::resolve_tool_path;
use crate::path::write_resolved_file;
use async_trait::async_trait;
use papermachine_protocol::PathOperation;
use papermachine_protocol::ToolDefinition;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub struct ApplyPatchTool;

#[async_trait]
impl ToolExecutor for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a Codex-style *** Begin Patch / *** End Patch edit to text files allowed by the current access boundary.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The complete Codex apply_patch body."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context
            .authorization
            .preset
            .allows_local_tool("apply_patch")
        {
            return Err(ToolError::PermissionDenied {
                tool: "apply_patch".to_string(),
                access: context.authorization.preset,
            });
        }
        let patch = match arguments {
            Value::String(patch) => patch,
            Value::Object(mut object) => object
                .remove("patch")
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|_| object.is_empty())
                .ok_or_else(|| invalid("expected exactly one string field named patch"))?,
            _ => return Err(invalid("expected a patch string")),
        };
        if patch.len() > MAX_PATCH_BYTES {
            return Err(invalid("patch exceeds 4 MiB"));
        }
        let hunks = parse_patch(&patch)?;
        let changes = prepare_changes(&context, hunks).await?;
        let summaries = tokio::task::spawn_blocking(move || apply_changes(changes))
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))??;
        Ok(ToolOutput {
            summary: format!("applied patch to {} file(s)", summaries.len()),
            value: json!({"files": summaries}),
        })
    }
}

#[derive(Debug)]
enum Hunk {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Debug, Default)]
struct Chunk {
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    end_of_file: bool,
}

fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ToolError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.trim().split('\n').collect::<Vec<_>>();
    if lines.first().copied() != Some("*** Begin Patch") {
        return Err(invalid("first line must be *** Begin Patch"));
    }
    if lines.last().copied() != Some("*** End Patch") {
        return Err(invalid("last line must be *** End Patch"));
    }
    let mut hunks = Vec::new();
    let mut index = 1;
    while index + 1 < lines.len() {
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = checked_path(path)?;
            index += 1;
            let mut content = Vec::new();
            while index + 1 < lines.len() && !is_hunk_header(lines[index]) {
                let value = lines[index]
                    .strip_prefix('+')
                    .ok_or_else(|| invalid_at(index + 1, "added file lines must start with +"))?;
                content.push(value);
                index += 1;
            }
            if content.is_empty() {
                return Err(invalid_at(
                    index + 1,
                    "added file must contain at least one line",
                ));
            }
            hunks.push(Hunk::Add {
                path,
                content: format!("{}\n", content.join("\n")),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            hunks.push(Hunk::Delete {
                path: checked_path(path)?,
            });
            index += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = checked_path(path)?;
            index += 1;
            let mut move_to = None;
            let mut chunks = Vec::new();
            let mut current: Option<Chunk> = None;
            while index + 1 < lines.len() && !is_hunk_header(lines[index]) {
                let line = lines[index];
                if let Some(path) = line.strip_prefix("*** Move to: ") {
                    if move_to.is_some() || current.is_some() || !chunks.is_empty() {
                        return Err(invalid_at(index + 1, "move must precede update chunks"));
                    }
                    move_to = Some(checked_path(path)?);
                } else if line == "@@" || line.starts_with("@@ ") {
                    if let Some(chunk) = current.take() {
                        push_chunk(&mut chunks, chunk, index + 1)?;
                    }
                    current = Some(Chunk {
                        context: line.strip_prefix("@@ ").map(str::to_string),
                        ..Chunk::default()
                    });
                } else if line == "*** End of File" {
                    current.get_or_insert_with(Chunk::default).end_of_file = true;
                } else if let Some((kind, text)) = split_change_line(line) {
                    let chunk = current.get_or_insert_with(Chunk::default);
                    match kind {
                        ' ' => {
                            chunk.old.push(text.to_string());
                            chunk.new.push(text.to_string());
                        }
                        '-' => chunk.old.push(text.to_string()),
                        '+' => chunk.new.push(text.to_string()),
                        _ => unreachable!("split_change_line only returns patch prefixes"),
                    }
                } else {
                    return Err(invalid_at(index + 1, "invalid update line"));
                }
                index += 1;
            }
            if let Some(chunk) = current.take() {
                push_chunk(&mut chunks, chunk, index + 1)?;
            }
            if chunks.is_empty() && move_to.is_none() {
                return Err(invalid("update hunk is empty"));
            }
            hunks.push(Hunk::Update {
                path,
                move_to,
                chunks,
            });
        } else {
            return Err(invalid_at(
                index + 1,
                "expected an Add, Delete, or Update hunk",
            ));
        }
    }
    if hunks.is_empty() {
        return Err(invalid("patch must contain at least one hunk"));
    }
    Ok(hunks)
}

fn push_chunk(chunks: &mut Vec<Chunk>, chunk: Chunk, line: usize) -> Result<(), ToolError> {
    if chunk.context.is_none() && chunk.old.is_empty() && chunk.new.is_empty() {
        return Err(invalid_at(line, "empty update chunk"));
    }
    chunks.push(chunk);
    Ok(())
}

fn split_change_line(line: &str) -> Option<(char, &str)> {
    let kind = line.chars().next()?;
    matches!(kind, ' ' | '+' | '-').then(|| (kind, &line[kind.len_utf8()..]))
}

fn is_hunk_header(line: &str) -> bool {
    line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
        || line == "*** End Patch"
}

fn checked_path(path: &str) -> Result<String, ToolError> {
    let path = path.trim();
    if path.is_empty() {
        Err(invalid("patch path must not be empty"))
    } else {
        Ok(path.to_string())
    }
}

enum PreparedChange {
    Add {
        path: PathBuf,
        display: String,
        content: String,
    },
    Delete {
        path: PathBuf,
        display: String,
        original: String,
    },
    Update {
        path: PathBuf,
        destination: Option<PathBuf>,
        display: String,
        destination_display: Option<String>,
        original: String,
        content: String,
    },
}

async fn prepare_changes(
    context: &ToolContext,
    hunks: Vec<Hunk>,
) -> Result<Vec<PreparedChange>, ToolError> {
    let mut touched = BTreeSet::new();
    let mut prepared = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        match hunk {
            Hunk::Add { path, content } => {
                ensure_unique(&mut touched, &path)?;
                let resolved =
                    resolve_tool_path(&context.authorization, &path, PathOperation::Write).await?;
                if tokio::fs::symlink_metadata(&resolved).await.is_ok() {
                    return Err(invalid(&format!("file already exists: {path}")));
                }
                prepared.push(PreparedChange::Add {
                    path: resolved,
                    display: path,
                    content,
                });
            }
            Hunk::Delete { path } => {
                ensure_unique(&mut touched, &path)?;
                let (resolved, original) = authorized_existing_file(context, &path).await?;
                prepared.push(PreparedChange::Delete {
                    path: resolved,
                    display: path,
                    original,
                });
            }
            Hunk::Update {
                path,
                move_to,
                chunks,
            } => {
                ensure_unique(&mut touched, &path)?;
                let (resolved, original) = authorized_existing_file(context, &path).await?;
                let display = path.clone();
                let content = apply_chunks(&path, &original, &chunks)?;
                let (destination, destination_display) = if let Some(move_to) = move_to {
                    ensure_unique(&mut touched, &move_to)?;
                    let destination =
                        resolve_tool_path(&context.authorization, &move_to, PathOperation::Write)
                            .await?;
                    if tokio::fs::symlink_metadata(&destination).await.is_ok() {
                        return Err(invalid(&format!("move destination exists: {move_to}")));
                    }
                    (Some(destination), Some(move_to))
                } else {
                    (None, None)
                };
                prepared.push(PreparedChange::Update {
                    path: resolved,
                    destination,
                    display,
                    destination_display,
                    original,
                    content,
                });
            }
        }
    }
    Ok(prepared)
}

async fn authorized_existing_file(
    context: &ToolContext,
    path: &str,
) -> Result<(PathBuf, String), ToolError> {
    let read = resolve_tool_path(&context.authorization, path, PathOperation::Read).await?;
    let write = resolve_tool_path(&context.authorization, path, PathOperation::Write).await?;
    if read != write {
        return Err(ToolError::Execution(format!(
            "path changed while authorizing patch: {path}"
        )));
    }
    let source = read.clone();
    let content = tokio::task::spawn_blocking(move || read_text(&source))
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))??;
    Ok((read, content))
}

fn read_text(path: &Path) -> Result<String, ToolError> {
    let (bytes, _, truncated) = read_resolved_file(path, MAX_FILE_BYTES)?;
    if truncated {
        return Err(ToolError::Execution(format!(
            "file exceeds {MAX_FILE_BYTES} bytes: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| ToolError::Execution(error.to_string()))
}

fn apply_chunks(path: &str, source: &str, chunks: &[Chunk]) -> Result<String, ToolError> {
    let normalized = source.replace("\r\n", "\n");
    let mut lines = normalized
        .split('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut cursor = 0;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let offset = find_sequence(&lines, std::slice::from_ref(context), cursor)
                .ok_or_else(|| invalid(&format!("failed to find context {context:?} in {path}")))?;
            cursor = offset + 1;
        }
        let start = if chunk.old.is_empty() {
            if chunk.end_of_file {
                lines.len()
            } else {
                cursor
            }
        } else {
            find_sequence(&lines, &chunk.old, cursor).ok_or_else(|| {
                invalid(&format!(
                    "failed to find expected lines in {path}:\n{}",
                    chunk.old.join("\n")
                ))
            })?
        };
        if chunk.end_of_file && start + chunk.old.len() != lines.len() {
            return Err(invalid(&format!("expected lines are not at end of {path}")));
        }
        lines.splice(start..start + chunk.old.len(), chunk.new.clone());
        cursor = start + chunk.new.len();
    }
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    })
}

fn find_sequence(lines: &[String], pattern: &[String], start: usize) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if start > lines.len() || pattern.len() > lines.len().saturating_sub(start) {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    for mode in 0..3 {
        let found = (start..=last_start).find(|offset| {
            lines[*offset..*offset + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(left, right)| match mode {
                    0 => left == right,
                    1 => left.trim_end() == right.trim_end(),
                    _ => left.trim() == right.trim(),
                })
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

fn apply_changes(changes: Vec<PreparedChange>) -> Result<Vec<String>, ToolError> {
    for change in &changes {
        match change {
            PreparedChange::Add { path, .. } => {
                if path.exists() {
                    return Err(ToolError::Execution(format!(
                        "file appeared while applying patch: {}",
                        path.display()
                    )));
                }
            }
            PreparedChange::Delete { path, original, .. }
            | PreparedChange::Update { path, original, .. } => {
                ensure_unchanged(path, original)?;
            }
        }
    }
    let mut summaries = Vec::with_capacity(changes.len());
    for change in changes {
        match change {
            PreparedChange::Add {
                path,
                display,
                content,
            } => {
                create_resolved_file(&path, content.as_bytes())?;
                summaries.push(format!("added {display}"));
            }
            PreparedChange::Delete {
                path,
                display,
                original: _,
            } => {
                remove_resolved_file(&path)?;
                summaries.push(format!("deleted {display}"));
            }
            PreparedChange::Update {
                path,
                destination,
                display,
                destination_display,
                original: _,
                content,
            } => {
                if let Some(destination) = destination {
                    create_resolved_file(&destination, content.as_bytes())?;
                    remove_resolved_file(&path)?;
                    summaries.push(format!(
                        "moved {display} to {}",
                        destination_display.unwrap_or_default()
                    ));
                } else {
                    write_resolved_file(&path, content.as_bytes(), false)?;
                    summaries.push(format!("updated {display}"));
                }
            }
        }
    }
    Ok(summaries)
}

fn ensure_unchanged(path: &Path, expected: &str) -> Result<(), ToolError> {
    if read_text(path)? == expected {
        Ok(())
    } else {
        Err(ToolError::Execution(format!(
            "file changed while applying patch: {}",
            path.display()
        )))
    }
}

fn ensure_unique(touched: &mut BTreeSet<String>, path: &str) -> Result<(), ToolError> {
    if touched.insert(path.to_string()) {
        Ok(())
    } else {
        Err(invalid(&format!("patch touches {path} more than once")))
    }
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: "apply_patch".to_string(),
        message: message.to_string(),
    }
}

fn invalid_at(line: usize, message: &str) -> ToolError {
    invalid(&format!("line {line}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_applies_add_update_delete_grammar() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: old.txt\n@@\n-old\n+updated\n*** Delete File: gone.txt\n*** End Patch",
        )
        .expect("patch should parse");
        assert_eq!(hunks.len(), 3);
        let Hunk::Update { chunks, .. } = &hunks[1] else {
            panic!("second hunk should update")
        };
        assert_eq!(
            apply_chunks("old.txt", "old\n", chunks).expect("chunk should apply"),
            "updated\n"
        );
    }
}
