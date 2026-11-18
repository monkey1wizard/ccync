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
        let mut findings = Vec::new();
        let payload = match read_instruction_source(&self.input_path) {
            Ok(InstructionSource::Absent) => Some(None),
            Ok(InstructionSource::Content(content)) => Some(Some(content)),
            Err(error) => {
                findings.push(source_finding(&self.input_path, &error));
                None
            }
        };
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
            // A deselected target must carry no block, and an unreadable
            // source leaves no payload to compare, so both inspect for removal.
            let desired = if *any_selected {
                payload.as_ref().and_then(|payload| payload.as_deref())
            } else {
                None
            };
            let source_unknown = *any_selected && payload.is_none();
            let classification = classify(path, desired);
            let recovery = "run `ccync sync`";
            let block_present = match classification {
                BlockClassification::Create | BlockClassification::Append => {
                    if !source_unknown {
                        findings.push(DoctorFinding::error(
                            format!("managed block is missing: {}", resolved.display()),
                            recovery,
                        ));
                    }
                    false
                }
                BlockClassification::Rewrite => {
                    findings.push(DoctorFinding::error(
                        format!(
                            "managed block differs from the instruction source: {}",
                            resolved.display()
                        ),
                        recovery,
                    ));
                    true
                }
                BlockClassification::Prune if *any_selected => {
                    if !source_unknown {
                        findings.push(DoctorFinding::error(
                            format!(
                                "managed block remains after the instruction source was removed: {}",
                                resolved.display()
                            ),
                            recovery,
                        ));
                    }
                    true
                }
                BlockClassification::Prune => {
                    findings.push(DoctorFinding::warning(format!(
                        "managed block is stale: {}",
                        resolved.display()
                    )));
                    true
                }
                BlockClassification::Unchanged => desired.is_some(),
                BlockClassification::TargetRead => {
                    findings.push(DoctorFinding::error(
                        format!("managed block cannot be read: {}", resolved.display()),
                        "check the target file permissions",
                    ));
                    false
                }
                BlockClassification::MalformedMarker => {
                    findings.push(DoctorFinding::error(
                        format!("managed block is damaged: {}", resolved.display()),
                        "repair or remove the ccync markers in the target file, then run `ccync sync`",
                    ));
                    false
                }
                BlockClassification::DanglingLink => {
                    findings.push(DoctorFinding::error(
                        format!(
                            "managed block target is a dangling symbolic link: {}",
                            resolved.display()
                        ),
                        "remove or repair the symbolic link, then run `ccync sync`",
                    ));
                    false
                }
            };

            if *has_codex && block_present {
                let Some(override_path) = path
                    .parent()
                    .map(|parent| parent.join("AGENTS.override.md"))
                else {
                    continue;
                };
                match fs::read(&override_path) {
                    Ok(bytes) if !bytes.is_empty() => {
                        findings.push(DoctorFinding::warning(format!(
                            "managed Codex file is shadowed by non-empty override: {}",
                            override_path.display()
                        )));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        findings.push(DoctorFinding::error(
                            format!(
                                "Codex override cannot be read: {} ({error})",
                                override_path.display()
                            ),
                            "check the override file permissions or remove it",
                        ));
                    }
                }
            }
        }

        findings
    }
}

fn source_finding(path: &Path, error: &InstructionSourceError) -> DoctorFinding {
    let (message, recovery) = match error {
        InstructionSourceError::Read(detail) => (
            format!(
                "instruction source cannot be read: {} ({detail})",
                path.display()
            ),
            "check the instruction source file permissions",
        ),
        InstructionSourceError::Utf8(detail) => (
            format!(
                "instruction source is not valid UTF-8: {} ({detail})",
                path.display()
            ),
            "re-save the instruction source as UTF-8",
        ),
        InstructionSourceError::DanglingLink => (
            format!(
                "instruction source is a dangling symbolic link: {}",
                path.display()
            ),
            "remove or repair the symbolic link",
        ),
    };
    DoctorFinding::error(message, recovery)
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
    TargetRead,
    MalformedMarker,
    DanglingLink,
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

/// The state of an optional instruction source after reading it.
#[derive(Debug, PartialEq, Eq)]
pub enum InstructionSource {
    /// The source is intentionally absent because it does not exist or is empty.
    Absent,
    /// The source contains valid UTF-8, including whitespace-only content.
    Content(String),
}

/// Failure while reading an instruction source.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InstructionSourceError {
    #[error("failed to read instruction source: {0}")]
    Read(String),
    #[error("instruction source is not valid UTF-8: {0}")]
    Utf8(String),
    #[error("instruction source is a dangling symbolic link")]
    DanglingLink,
}

/// Read an optional instruction source without degrading failures to absence.
///
/// A missing path and a zero-byte file are absent. All other valid UTF-8 is
/// preserved exactly, including whitespace-only content.
pub fn read_instruction_source(path: &Path) -> Result<InstructionSource, InstructionSourceError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstructionSource::Absent);
        }
        Err(error) => return Err(InstructionSourceError::Read(error.to_string())),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if matches!(fs::metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound) {
                return Err(InstructionSourceError::DanglingLink);
            }
        }
        Ok(_) => {}
    }

    let bytes = fs::read(path).map_err(|error| InstructionSourceError::Read(error.to_string()))?;
    if bytes.is_empty() {
        return Ok(InstructionSource::Absent);
    }

    String::from_utf8(bytes)
        .map(InstructionSource::Content)
        .map_err(|error| InstructionSourceError::Utf8(error.to_string()))
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
    let span = locate_block(&bytes).map_err(BlockError::Damage)?;
    let had_block = span.is_some();
    let transformed = apply_block(&bytes, payload).map_err(BlockError::Damage)?;
    if transformed == bytes {
        return Ok(BlockOutcome::Unchanged);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| BlockError::Read(error.to_string()))?;
    }
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
        Err(_) => return BlockClassification::TargetRead,
    };

    if metadata.file_type().is_symlink()
        && matches!(
            fs::metadata(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    {
        return BlockClassification::DanglingLink;
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return BlockClassification::TargetRead,
    };
    let span = match locate_block(&bytes) {
        Ok(span) => span,
        Err(_) => return BlockClassification::MalformedMarker,
    };

    match (payload, span) {
        (None, Some(_)) => BlockClassification::Prune,
        (None, None) => BlockClassification::Unchanged,
        (Some(payload), None) => match apply_block(&bytes, Some(payload)) {
            Ok(transformed) if transformed == bytes => BlockClassification::Unchanged,
            Ok(_) => BlockClassification::Append,
            Err(_) => BlockClassification::MalformedMarker,
        },
        (Some(payload), Some(_)) => match apply_block(&bytes, Some(payload)) {
            Ok(transformed) if transformed == bytes => BlockClassification::Unchanged,
            Ok(_) => BlockClassification::Rewrite,
            Err(_) => BlockClassification::MalformedMarker,
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
        span.map_or_else(|| bytes.is_empty() || bytes.ends_with(b"\n"), |_| false),
    );

    Ok(match replacement_span {
        Some(span) => [
            bytes[..span.start].to_vec(),
            block,
            bytes[span.end..].to_vec(),
        ]
        .concat(),
        None => {
            let separator = if bytes.is_empty() || bytes.ends_with(b"\n") {
                Vec::new()
            } else {
                line_ending.to_vec()
            };
            [bytes.to_vec(), separator, block].concat()
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
