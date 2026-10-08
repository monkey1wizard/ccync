//! ccync-owned managed marker-block module: parser, emitter, I/O wrapper,
//! dedupe, dry-run classifier, and the doctor health check.

use ccync_foundation::health::{DoctorFinding, HealthCheck};
use ccync_foundation::platform::atomic_write_bytes;
use ccync_foundation::target::ProjectionTargetId;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

const BEGIN_MARKER: &[u8] = b"<!-- ccync:managed:begin -->";
const END_MARKER: &[u8] = b"<!-- ccync:managed:end -->";

/// One target's resolution, independent of current selection state.
pub struct TargetResolution {
    pub target: ProjectionTargetId,
    pub path: PathBuf,
    /// Whether this target is presently selected (from config), not whether
    /// a file/block currently exists at `path`.
    pub selected: bool,
}

/// Read-only doctor check for managed marker blocks projected to target files.
pub struct ManagedBlockHealthCheck {
    input_path: PathBuf,
    targets: Vec<TargetResolution>,
}

impl ManagedBlockHealthCheck {
    pub fn new(input_path: PathBuf, targets: Vec<TargetResolution>) -> Self {
        Self {
            input_path,
            targets,
        }
    }
}

impl HealthCheck for ManagedBlockHealthCheck {
    fn name(&self) -> &str {
        "managed-blocks"
    }

    fn check(&self) -> Vec<DoctorFinding> {
        let input_exists = self.input_path.exists();
        let mut findings = Vec::new();
        let mut groups: Vec<(PathBuf, bool, bool)> = Vec::new(); // (path, any_selected, has_codex)
        let mut index_by_resolved = std::collections::HashMap::new();

        for resolution in &self.targets {
            let resolved = resolved_path(&resolution.path);
            let has_codex = resolution.target == ProjectionTargetId::Codex;
            match index_by_resolved.get(&resolved) {
                Some(&index) => {
                    let entry: &mut (PathBuf, bool, bool) = &mut groups[index];
                    entry.1 |= resolution.selected;
                    entry.2 |= has_codex;
                }
                None => {
                    index_by_resolved.insert(resolved, groups.len());
                    groups.push((resolution.path.clone(), resolution.selected, has_codex));
                }
            }
        }

        for (path, any_selected, has_codex) in &groups {
            let resolved = resolved_path(path);
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if *any_selected && input_exists {
                        findings.push(DoctorFinding::error(
                            format!("managed block is absent: {}", resolved.display()),
                            "run `ccync sync`",
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    findings.push(DoctorFinding::error(
                        format!(
                            "managed block cannot be read: {} ({error})",
                            resolved.display()
                        ),
                        "check the target file permissions",
                    ));
                    continue;
                }
            };

            match locate_block(&bytes) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if *any_selected && input_exists {
                        findings.push(DoctorFinding::error(
                            format!("managed block is absent: {}", resolved.display()),
                            "run `ccync sync`",
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    findings.push(DoctorFinding::error(
                        format!("managed block is damaged: {}", resolved.display()),
                        "run `ccync sync`",
                    ));
                    continue;
                }
            };

            if !*any_selected {
                findings.push(DoctorFinding::warning(format!(
                    "managed block is stale: {}",
                    resolved.display()
                )));
            }

            if *has_codex {
                let override_path = path
                    .parent()
                    .map(|parent| parent.join("AGENTS.override.md"));
                if override_path
                    .as_deref()
                    .and_then(|path| fs::read(path).ok())
                    .is_some_and(|bytes| !bytes.is_empty())
                {
                    findings.push(DoctorFinding::warning(format!(
                        "managed Codex file is shadowed by non-empty override: {}",
                        override_path.expect("override path exists").display()
                    )));
                }
            }
        }

        findings
    }
}

fn resolved_path(path: &Path) -> PathBuf {
    path.parent()
        .and_then(|parent| fs::canonicalize(parent).ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Remove duplicate target paths while preserving the first-seen spelling.
pub fn dedupe_resolved(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(paths.len());

    for path in paths {
        if seen.insert(resolved_path(&path)) {
            deduped.push(path);
        }
    }

    deduped
}

/// Result of applying a managed block to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOutcome {
    Create,
    Append,
    Rewrite,
    Prune,
    Unchanged,
}

/// Dry-run classification for a managed block operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockClassification {
    Create,
    Append,
    Rewrite,
    Prune,
    Unchanged,
    Damaged,
    Dangling,
}

/// Failure while reading, transforming, or publishing a managed block.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("failed to read managed block file: {0}")]
    Read(String),
    #[error("managed block is malformed: {0}")]
    Damage(BlockDamage),
    #[error("failed to write managed block file: {0}")]
    Write(String),
}

/// Read, transform, and atomically publish a managed block.
pub fn write_block(path: &Path, payload: Option<&str>) -> Result<BlockOutcome, BlockError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(BlockError::Read(error.to_string())),
    };

    let bytes = match metadata {
        Some(_) => fs::read(path).map_err(|error| BlockError::Read(error.to_string()))?,
        None => Vec::new(),
    };
    let had_file = metadata.is_some();
    if payload.is_none() && !had_file {
        return Ok(BlockOutcome::Unchanged);
    }
    let span = locate_block(&bytes).map_err(BlockError::Damage)?;
    let had_block = span.is_some();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| BlockError::Read(error.to_string()))?;
    }

    let transformed = if payload.is_none() {
        if let Some(span) = span {
            apply_block(&bytes, payload).map_err(BlockError::Damage)?;
            [bytes[..span.start].to_vec(), bytes[span.end..].to_vec()].concat()
        } else {
            apply_block(&bytes, payload).map_err(BlockError::Damage)?
        }
    } else {
        let mut transformed = apply_block(&bytes, payload).map_err(BlockError::Damage)?;
        if bytes.is_empty() {
            transformed.push(b'\n');
        }
        transformed
    };
    atomic_write_bytes(path, &transformed).map_err(|error| BlockError::Write(error.to_string()))?;

    Ok(match (payload, had_file, had_block) {
        (Some(_), false, _) => BlockOutcome::Create,
        (Some(_), true, false) => BlockOutcome::Append,
        (Some(_), true, true) => BlockOutcome::Rewrite,
        (None, _, true) => BlockOutcome::Prune,
        (None, _, false) => BlockOutcome::Unchanged,
    })
}

/// Classify the change a managed block operation would make without writing.
pub fn classify(path: &Path, payload: Option<&str>) -> BlockClassification {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return if payload.is_some() {
                BlockClassification::Create
            } else {
                BlockClassification::Unchanged
            };
        }
        Err(_) => return BlockClassification::Damaged,
    };

    if metadata.file_type().is_symlink()
        && matches!(
            fs::metadata(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    {
        return BlockClassification::Dangling;
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return BlockClassification::Damaged,
    };
    let span = match locate_block(&bytes) {
        Ok(span) => span,
        Err(_) => return BlockClassification::Damaged,
    };

    match (payload, span) {
        (None, Some(_)) => BlockClassification::Prune,
        (None, None) => BlockClassification::Unchanged,
        (Some(_), None) => BlockClassification::Append,
        (Some(payload), Some(_)) => match apply_block(&bytes, Some(payload)) {
            Ok(transformed) if transformed == bytes => BlockClassification::Unchanged,
            Ok(_) => BlockClassification::Rewrite,
            Err(_) => BlockClassification::Damaged,
        },
    }
}

/// Byte range covering a complete managed marker pair, including both marker
/// lines and their line endings when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Why a marker pair could not be located cleanly.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("malformed managed block")]
pub enum BlockDamage {
    Malformed,
}

/// Locate the ccync-owned marker pair within `bytes`.
///
/// `Ok(None)` means no marker pair is present. `Ok(Some(_))` returns the
/// located span. `Err(_)` means the markers are damaged (more than one
/// begin/end marker, or an unpaired marker).
pub(crate) fn locate_block(bytes: &[u8]) -> Result<Option<BlockSpan>, BlockDamage> {
    let mut begin = None;
    let mut end = None;
    let mut offset = 0;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let content_end = line
            .len()
            .saturating_sub(if line.ends_with(b"\n") { 1 } else { 0 });
        let content = line[..content_end]
            .strip_suffix(b"\r")
            .unwrap_or(&line[..content_end]);

        if content == BEGIN_MARKER {
            if begin.replace(offset).is_some() {
                return Err(BlockDamage::Malformed);
            }
        } else if content == END_MARKER && end.replace(offset + line.len()).is_some() {
            return Err(BlockDamage::Malformed);
        }

        offset += line.len();
    }

    match (begin, end) {
        (None, None) => Ok(None),
        (Some(start), Some(finish)) if start < finish => Ok(Some(BlockSpan { start, end: finish })),
        _ => Err(BlockDamage::Malformed),
    }
}

/// Apply or remove the ccync-owned marker block without performing I/O.
pub(crate) fn apply_block(bytes: &[u8], payload: Option<&str>) -> Result<Vec<u8>, BlockDamage> {
    let span = locate_block(bytes)?;

    let Some(payload) = payload else {
        return Ok(match span {
            Some(span) => [
                bytes[..span.start].to_vec(),
                bytes[marker_end(bytes, span)..].to_vec(),
            ]
            .concat(),
            None => bytes.to_vec(),
        });
    };

    let line_ending = dominant_line_ending(bytes);
    let replacement_span = span.map(|span| BlockSpan {
        start: span.start,
        end: marker_end(bytes, span),
    });
    let block = emit_block(
        payload,
        line_ending,
        span.map_or_else(|| bytes.ends_with(b"\n"), |_| false),
    );

    Ok(match replacement_span {
        Some(span) => [
            bytes[..span.start].to_vec(),
            block,
            bytes[span.end..].to_vec(),
        ]
        .concat(),
        None => {
            let source = if has_tied_line_endings(bytes) {
                normalize_line_endings(bytes)
            } else {
                bytes.to_vec()
            };
            let separator = if bytes.is_empty() || bytes.ends_with(b"\n") {
                Vec::new()
            } else {
                line_ending.to_vec()
            };
            [source, separator, block].concat()
        }
    })
}

fn marker_end(bytes: &[u8], span: BlockSpan) -> usize {
    let end_marker_end = span.end;
    if bytes[end_marker_end.saturating_sub(2)..end_marker_end] == *b"\r\n" {
        end_marker_end - 2
    } else if bytes[end_marker_end.saturating_sub(1)..end_marker_end] == *b"\n" {
        end_marker_end - 1
    } else {
        end_marker_end
    }
}

fn dominant_line_ending(bytes: &[u8]) -> &'static [u8] {
    let mut lf = 0;
    let mut crlf = 0;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.ends_with(b"\r\n") {
            crlf += 1;
        } else if line.ends_with(b"\n") {
            lf += 1;
        }
    }

    if crlf > lf {
        b"\r\n"
    } else {
        b"\n"
    }
}

fn has_tied_line_endings(bytes: &[u8]) -> bool {
    let mut lf = 0;
    let mut crlf = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.ends_with(b"\r\n") {
            crlf += 1;
        } else if line.ends_with(b"\n") {
            lf += 1;
        }
    }
    lf == crlf && lf != 0
}

fn normalize_line_endings(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            continue;
        }
        normalized.push(*byte);
    }
    normalized
}

fn emit_block(payload: &str, line_ending: &[u8], trailing_newline: bool) -> Vec<u8> {
    let normalized_payload = payload.replace("\r\n", "\n").replace('\r', "\n");
    let mut block = Vec::new();
    block.extend_from_slice(BEGIN_MARKER);
    block.extend_from_slice(line_ending);

    for (index, line) in normalized_payload.split('\n').enumerate() {
        if index > 0 {
            block.extend_from_slice(line_ending);
        }
        block.extend_from_slice(line.as_bytes());
    }

    block.extend_from_slice(line_ending);
    block.extend_from_slice(END_MARKER);
    if trailing_newline {
        block.extend_from_slice(line_ending);
    }
    block
}
