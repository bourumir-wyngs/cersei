//! Session-scoped in-memory file storage for X* tools.

use crate::xfile_sync::{sync_disk_snapshot, SyncChange, SyncStats};
use crate::ToolContext;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_REVISIONS: usize = 16;

static XFILE_STORAGE_REGISTRY: Lazy<dashmap::DashMap<String, Arc<Mutex<XFileStorage>>>> =
    Lazy::new(dashmap::DashMap::new);
static XFILE_TAG_COUNTER: AtomicUsize = AtomicUsize::new(1);
static XFILE_TRACKING_COUNTER: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XLine {
    pub content: String,
    pub line_number: usize,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XFile {
    pub path: PathBuf,
    #[serde(default = "default_true")]
    pub exists: bool,
    pub content: Vec<XLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XFileRevision {
    pub number: usize,
    pub file: XFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<XFileRevisionMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XPathChangeMetadata {
    pub source_path: PathBuf,
    pub destination_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct XCheckpointEntry {
    path: PathBuf,
    revision: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct XFileCheckpoint {
    entries: HashMap<String, XCheckpointEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct XFileRevisionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<XPathChangeMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied: Option<XPathChangeMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct XDiskState {
    modified_ns: Option<u64>,
    len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XTrackedFileSummary {
    pub path: PathBuf,
    pub revision_count: usize,
    pub current_revision: usize,
    pub exists: bool,
    pub line_count: usize,
    pub current_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XTrackedFile {
    #[serde(default = "next_tracking_id")]
    tracking_id: String,
    revisions: VecDeque<XFileRevision>,
    next_revision_number: usize,
    last_disk_state: Option<XDiskState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct XFileStorage {
    files: HashMap<PathBuf, XTrackedFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<XFileCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedXFileStorage {
    version: u32,
    files: HashMap<PathBuf, XTrackedFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<XFileCheckpoint>,
}

#[derive(Debug, Clone)]
pub struct XFileHead {
    pub file: XFile,
    pub rendered_content: String,
    pub current_version: String,
    pub revision_count: usize,
}

#[derive(Debug, Clone)]
pub struct XFileSyncUpdate {
    pub file_path: PathBuf,
    pub current_version: String,
    pub revision_count: usize,
    pub stats: SyncStats,
    pub changes: Vec<SyncChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XCheckpointSummary {
    pub tracked_files: usize,
    pub current_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XRollbackSummary {
    pub used_explicit_checkpoint: bool,
    pub changed_files: usize,
    pub removed_files: usize,
    pub unchanged_files: usize,
    pub affected_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XCheckpointDiffEntry {
    pub baseline_file: XFile,
    pub baseline_revision: usize,
    pub current_file: XFile,
    pub current_revision: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XCheckpointDiffSummary {
    pub used_explicit_checkpoint: bool,
    pub changed_files: usize,
    pub entries: Vec<XCheckpointDiffEntry>,
}

#[derive(Debug, Clone)]
struct XFileTransition {
    current: XFile,
    target: Option<XFile>,
}

#[derive(Debug, Clone)]
struct XRollbackPlan {
    files: HashMap<PathBuf, XTrackedFile>,
    transitions: Vec<XFileTransition>,
    summary: XRollbackSummary,
}

#[derive(Debug, Clone)]
struct XBaselineTarget {
    revision: usize,
    file: XFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackDecision {
    KeepCurrent,
    Remove,
    ToRevision(usize),
}

#[derive(Debug, Clone)]
pub enum XLineMutation {
    ReplaceLine {
        tag: String,
        new_text: String,
    },
    InsertBefore {
        tag: String,
        new_lines: Vec<String>,
    },
    InsertAfter {
        tag: String,
        new_lines: Vec<String>,
    },
    DeleteLine {
        tag: String,
    },
    DeleteRange {
        from_tag: String,
        to_tag: String,
    },
    MoveRange {
        from_tag: String,
        to_tag: String,
        move_after_tag: String,
    },
    OverwriteRange {
        from_tag: String,
        to_tag: String,
        new_content: String,
    },
    RegexReplace {
        from_tag: String,
        to_tag: String,
        pattern: String,
        replacement: String,
    },
}

pub fn session_xfile_storage(session_id: &str) -> Arc<Mutex<XFileStorage>> {
    XFILE_STORAGE_REGISTRY
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(XFileStorage::default())))
        .clone()
}

pub fn clear_session_xfile_storage(session_id: &str) {
    XFILE_STORAGE_REGISTRY.remove(session_id);
}

pub fn save_session_xfile_storage_to_path(session_id: &str, path: &Path) -> Result<bool, String> {
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();

    if guard.files.is_empty() {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| format!("Failed to remove XFileStorage sidecar: {}", e))?;
        }
        return Ok(false);
    }

    let payload = PersistedXFileStorage {
        version: 1,
        files: guard.files.clone(),
        checkpoint: guard.checkpoint.clone(),
    };
    drop(guard);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create XFileStorage sidecar directory: {}", e))?;
    }

    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|e| format!("Failed to serialize XFileStorage: {}", e))?;
    std::fs::write(path, bytes)
        .map_err(|e| format!("Failed to write XFileStorage sidecar: {}", e))?;
    Ok(true)
}

pub fn load_session_xfile_storage_from_path(session_id: &str, path: &Path) -> Result<bool, String> {
    if !path.exists() {
        clear_session_xfile_storage(session_id);
        return Ok(false);
    }

    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read XFileStorage sidecar: {}", e))?;
    let payload: PersistedXFileStorage = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Failed to parse XFileStorage sidecar: {}", e))?;
    if payload.version != 1 {
        return Err(format!(
            "Unsupported XFileStorage sidecar version {}.",
            payload.version
        ));
    }

    observe_restored_tags(payload.files.values());

    XFILE_STORAGE_REGISTRY.insert(
        session_id.to_string(),
        Arc::new(Mutex::new(XFileStorage {
            files: payload.files,
            checkpoint: payload.checkpoint,
        })),
    );
    Ok(true)
}

pub fn xfile_session_id(ctx: &ToolContext) -> String {
    ctx.extensions
        .get::<crate::XFileStorageScope>()
        .map(|scope| scope.session_id.clone())
        .unwrap_or_else(|| ctx.session_id.clone())
}

pub fn resolve_xfile_path(ctx: &ToolContext, input: &str) -> PathBuf {
    let candidate = Path::new(input);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.working_dir.join(candidate)
    };

    if let Ok(canonical) = resolved.canonicalize() {
        return canonical;
    }

    if let Some(parent) = resolved.parent() {
        if let Ok(parent_canonical) = parent.canonicalize() {
            if let Some(file_name) = resolved.file_name() {
                return parent_canonical.join(file_name);
            }
        }
    }

    resolved
}

pub fn try_get_head(session_id: &str, path: &Path) -> Option<XFileHead> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();
    guard.current_head(&key)
}

pub async fn ensure_loaded(session_id: &str, path: &Path) -> Result<XFileHead, String> {
    let key = normalize_storage_path(path);
    if let Some(head) = try_get_head(session_id, &key) {
        return Ok(head);
    }

    let text = tokio::fs::read_to_string(&key).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            "Binary files cannot be handled with File tool, use Bash".to_string()
        } else {
            format!("Failed to read file: {}", e)
        }
    })?;
    let disk_state = read_disk_state(&key).ok().flatten();

    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    if let Some(head) = guard.current_head(&key) {
        return Ok(head);
    }

    Ok(guard.insert_loaded_file(key, &text, disk_state))
}

pub fn store_loaded_if_missing(session_id: &str, path: &Path, text: &str) -> XFileHead {
    let key = normalize_storage_path(path);
    let disk_state = read_disk_state(&key).ok().flatten();
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    if let Some(head) = guard.current_head(&key) {
        return head;
    }

    guard.insert_loaded_file(key, text, disk_state)
}

pub fn store_written_text(session_id: &str, path: &Path, text: &str) -> XFileHead {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.write_text(key, text)
}

pub fn store_deleted_file(session_id: &str, path: &Path) -> XFileHead {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.delete_file(key)
}

pub fn copy_tracked_file(
    session_id: &str,
    source: &Path,
    destination: &Path,
) -> Result<XFileHead, String> {
    let source_key = normalize_storage_path(source);
    let destination_key = normalize_storage_path(destination);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.copy_file(&source_key, destination_key)
}

pub fn move_tracked_file(
    session_id: &str,
    source: &Path,
    destination: &Path,
) -> Result<XFileHead, String> {
    let source_key = normalize_storage_path(source);
    let destination_key = normalize_storage_path(destination);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.move_file(&source_key, destination_key)
}

pub fn apply_mutations(
    session_id: &str,
    path: &Path,
    base_version: Option<&str>,
    operations: &[XLineMutation],
) -> Result<XFileHead, String> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.apply_mutations(&key, base_version, operations)
}

pub fn record_disk_state(session_id: &str, path: &Path) -> Result<(), String> {
    let key = normalize_storage_path(path);
    let disk_state = read_disk_state(&key)?;
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.update_disk_state(&key, disk_state)
}

pub async fn sync_if_disk_changed(
    session_id: &str,
    path: &Path,
) -> Result<Option<XFileSyncUpdate>, String> {
    let key = normalize_storage_path(path);
    let current_disk_state = read_disk_state(&key)?;

    {
        let storage = session_xfile_storage(session_id);
        let guard = storage.lock();
        let Some(tracked) = guard.files.get(&key) else {
            return Ok(None);
        };
        let latest_is_absent = tracked
            .revisions
            .back()
            .map(|revision| !revision.file.exists)
            .unwrap_or(false);
        if tracked.last_disk_state.as_ref() == current_disk_state.as_ref()
            && (current_disk_state.is_some() || latest_is_absent)
        {
            return Ok(None);
        }
    }

    let current_disk_state = match current_disk_state {
        Some(current_disk_state) => current_disk_state,
        None => {
            let storage = session_xfile_storage(session_id);
            let mut guard = storage.lock();
            let tracked = match guard.files.get_mut(&key) {
                Some(tracked) => tracked,
                None => return Ok(None),
            };
            let latest = tracked
                .revisions
                .back()
                .cloned()
                .ok_or_else(|| format!("No revision found for {}", key.display()))?;
            if !latest.file.exists {
                tracked.last_disk_state = None;
                return Ok(None);
            }

            let deleted = build_absent_file(key.clone());
            let changes = latest
                .file
                .content
                .iter()
                .map(|line| SyncChange::Deleted {
                    tag: line.tag.clone(),
                    content: line.content.clone(),
                })
                .collect::<Vec<_>>();
            let head = push_revision(tracked, deleted);
            tracked.last_disk_state = None;

            return Ok(Some(XFileSyncUpdate {
                file_path: key,
                current_version: head.current_version,
                revision_count: head.revision_count,
                stats: SyncStats {
                    kept: 0,
                    inserted: 0,
                    deleted: changes.len(),
                    replaced: 0,
                },
                changes,
            }));
        }
    };

    let disk_text = tokio::fs::read_to_string(&key).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            "Binary files cannot be handled with File tool, use Bash".to_string()
        } else {
            format!("Failed to read file: {}", e)
        }
    })?;

    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    let tracked = match guard.files.get_mut(&key) {
        Some(tracked) => tracked,
        None => return Ok(None),
    };
    if tracked.last_disk_state.as_ref() == Some(&current_disk_state) {
        return Ok(None);
    }
    let latest = tracked
        .revisions
        .back()
        .cloned()
        .ok_or_else(|| format!("No revision found for {}", key.display()))?;

    if tracked.last_disk_state.is_none()
        && latest.file.exists
        && render_file(&latest.file) == disk_text
    {
        tracked.last_disk_state = Some(current_disk_state);
        return Ok(None);
    }

    let synced = sync_disk_snapshot(&latest.file, &disk_text);
    let head = push_revision(tracked, synced.file);
    tracked.last_disk_state = Some(current_disk_state);

    Ok(Some(XFileSyncUpdate {
        file_path: key,
        current_version: head.current_version,
        revision_count: head.revision_count,
        stats: synced.stats,
        changes: synced.changes,
    }))
}

pub fn list_tracked_files(session_id: &str) -> Vec<XTrackedFileSummary> {
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();
    let mut files: Vec<XTrackedFileSummary> = guard
        .files
        .iter()
        .filter_map(|(path, tracked)| {
            tracked
                .revisions
                .back()
                .map(|revision| XTrackedFileSummary {
                    path: path.clone(),
                    revision_count: tracked.revisions.len(),
                    current_revision: revision.number,
                    exists: revision.file.exists,
                    line_count: revision.file.content.len(),
                    current_version: compute_file_version(&revision.file),
                })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

pub fn list_revisions(session_id: &str, path: &Path) -> Option<Vec<XFileRevision>> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();
    guard
        .files
        .get(&key)
        .map(|tracked| tracked.revisions.iter().cloned().collect())
}

pub fn get_revision(session_id: &str, path: &Path, revision: usize) -> Option<XFileRevision> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();
    guard.files.get(&key).and_then(|tracked| {
        tracked
            .revisions
            .iter()
            .find(|rev| rev.number == revision)
            .cloned()
    })
}

pub fn restore_revision(
    session_id: &str,
    path: &Path,
    revision: usize,
) -> Result<XFileHead, String> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.restore_revision(&key, revision)
}

pub fn discard_head_revision(session_id: &str, path: &Path) -> Result<XFileHead, String> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.discard_head_revision(&key)
}

pub fn create_checkpoint(session_id: &str) -> XCheckpointSummary {
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.create_checkpoint()
}

pub fn diff_against_checkpoint(session_id: &str) -> Result<XCheckpointDiffSummary, String> {
    let storage = session_xfile_storage(session_id);
    let guard = storage.lock();
    guard.diff_against_checkpoint()
}

pub async fn rollback_to_checkpoint(session_id: &str) -> Result<XRollbackSummary, String> {
    let storage = session_xfile_storage(session_id);
    let plan = {
        let guard = storage.lock();
        guard.plan_rollback()?
    };

    apply_rollback_transitions_to_disk(&plan.transitions).await?;

    let mut guard = storage.lock();
    guard.files = plan.files;
    guard.refresh_disk_states()?;
    Ok(plan.summary)
}

pub async fn apply_file_to_disk(path: &Path, file: &XFile) -> Result<(), String> {
    if !file.exists {
        match tokio::fs::remove_file(path).await {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(format!("Failed to remove file {}: {}", path.display(), err)),
        }
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            format!(
                "Failed to create directories for {}: {}",
                path.display(),
                err
            )
        })?;
    }
    tokio::fs::write(path, render_file(file).as_bytes())
        .await
        .map_err(|err| format!("Failed to write file {}: {}", path.display(), err))
}

pub async fn apply_file_transition_to_disk(current: &XFile, target: &XFile) -> Result<(), String> {
    if current.path == target.path {
        return apply_file_to_disk(&target.path, target).await;
    }

    apply_file_to_disk(&target.path, target).await?;
    apply_file_to_disk(&current.path, &build_absent_file(current.path.clone())).await
}

async fn apply_rollback_transitions_to_disk(transitions: &[XFileTransition]) -> Result<(), String> {
    for transition in transitions {
        let remove_current = match &transition.target {
            Some(target) => !target.exists || target.path != transition.current.path,
            None => true,
        };
        if remove_current {
            apply_file_to_disk(
                &transition.current.path,
                &build_absent_file(transition.current.path.clone()),
            )
            .await?;
        }
    }

    for transition in transitions {
        if let Some(target) = &transition.target {
            if target.exists {
                apply_file_to_disk(&target.path, target).await?;
            }
        }
    }

    Ok(())
}

pub fn render_file(file: &XFile) -> String {
    if !file.exists || file.content.is_empty() {
        return String::new();
    }

    let mut rendered = file
        .content
        .iter()
        .map(|line| line.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    rendered.push('\n');
    rendered
}

pub fn compute_version(rendered: &str) -> String {
    format!("blake3:{}", blake3::hash(rendered.as_bytes()).to_hex())
}

pub fn compute_file_version(file: &XFile) -> String {
    if file.exists {
        compute_version(&render_file(file))
    } else {
        compute_version("<absent>")
    }
}

pub fn file_state(file: &XFile) -> &'static str {
    if file.exists {
        "present"
    } else {
        "absent"
    }
}

pub fn files_differ(old: &XFile, new: &XFile) -> bool {
    old.path != new.path || old.exists != new.exists || render_file(old) != render_file(new)
}

pub fn diff_files(old: &XFile, new: &XFile, old_label: &str, new_label: &str) -> String {
    let old_label = format!("{} ({})", old_label, file_state(old));
    let new_label = format!("{} ({})", new_label, file_state(new));
    let mut diff = crate::file_history::unified_diff(
        &render_file(old),
        &render_file(new),
        &old_label,
        &new_label,
    );

    if old.exists != new.exists && !diff.contains("@@") {
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str("@@ file state @@\n");
        diff.push_str(&format!("-{}\n+{}", file_state(old), file_state(new)));
    }

    if old.path != new.path {
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str("@@ file path @@\n");
        diff.push_str(&format!("-{}\n+{}", old.path.display(), new.path.display()));
    }

    diff
}

pub(crate) fn next_tag() -> String {
    let next = XFILE_TAG_COUNTER.fetch_add(1, Ordering::Relaxed);
    STANDARD.encode(next.to_string())
}

fn next_tracking_id() -> String {
    format!(
        "xfile:{}",
        XFILE_TRACKING_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn normalize_storage_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    if let Some(parent) = path.parent() {
        if let Ok(parent_canonical) = parent.canonicalize() {
            if let Some(file_name) = path.file_name() {
                return parent_canonical.join(file_name);
            }
        }
    }

    path.to_path_buf()
}

fn read_disk_state(path: &Path) -> Result<Option<XDiskState>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "Failed to read metadata for {}: {}",
                path.display(),
                err
            ))
        }
    };

    Ok(Some(XDiskState {
        modified_ns: metadata.modified().ok().and_then(system_time_to_nanos),
        len: metadata.len(),
    }))
}

fn system_time_to_nanos(time: SystemTime) -> Option<u64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_nanos().try_into().ok()?)
}

fn observe_restored_tags<'a>(tracked_files: impl IntoIterator<Item = &'a XTrackedFile>) {
    let mut max_seen = 0usize;
    for tracked in tracked_files {
        for revision in &tracked.revisions {
            for line in &revision.file.content {
                if let Some(value) = parse_tag_counter(&line.tag) {
                    max_seen = max_seen.max(value);
                }
            }
        }
    }

    let mut current = XFILE_TAG_COUNTER.load(Ordering::Relaxed);
    while max_seen >= current {
        match XFILE_TAG_COUNTER.compare_exchange(
            current,
            max_seen + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn parse_tag_counter(tag: &str) -> Option<usize> {
    let decoded = STANDARD.decode(tag).ok()?;
    let as_str = std::str::from_utf8(&decoded).ok()?;
    as_str.parse::<usize>().ok()
}

fn split_text_into_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|line| line.to_string()).collect()
    }
}

fn default_true() -> bool {
    true
}

fn build_absent_file(path: PathBuf) -> XFile {
    XFile {
        path,
        exists: false,
        content: Vec::new(),
    }
}

fn build_file_with_new_tags(path: PathBuf, text: &str) -> XFile {
    let lines = split_text_into_lines(text);
    let content = lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| XLine {
            content: line,
            line_number: idx + 1,
            tag: next_tag(),
        })
        .collect();

    XFile {
        path,
        exists: true,
        content,
    }
}

fn clone_file_with_path(file: &XFile, path: PathBuf) -> XFile {
    XFile {
        path,
        exists: file.exists,
        content: file.content.clone(),
    }
}

fn clone_file_with_new_tags(file: &XFile, path: PathBuf) -> XFile {
    let content = file
        .content
        .iter()
        .enumerate()
        .map(|(idx, line)| XLine {
            content: line.content.clone(),
            line_number: idx + 1,
            tag: next_tag(),
        })
        .collect();

    XFile {
        path,
        exists: file.exists,
        content,
    }
}

fn make_head(revision: &XFileRevision, revision_count: usize) -> XFileHead {
    let rendered_content = render_file(&revision.file);
    let current_version = compute_file_version(&revision.file);
    XFileHead {
        file: revision.file.clone(),
        rendered_content,
        current_version,
        revision_count,
    }
}

fn renumber_lines(lines: &mut [XLine]) {
    for (idx, line) in lines.iter_mut().enumerate() {
        line.line_number = idx + 1;
    }
}

fn push_revision(tracked: &mut XTrackedFile, file: XFile) -> XFileHead {
    push_revision_with_metadata(tracked, file, None)
}

fn push_revision_with_metadata(
    tracked: &mut XTrackedFile,
    file: XFile,
    metadata: Option<XFileRevisionMetadata>,
) -> XFileHead {
    let number = tracked.next_revision_number;
    tracked.next_revision_number += 1;
    tracked.revisions.push_back(XFileRevision {
        number,
        file,
        metadata,
    });
    while tracked.revisions.len() > MAX_REVISIONS {
        tracked.revisions.pop_front();
    }

    let revision = tracked
        .revisions
        .back()
        .expect("revision just pushed must exist");
    make_head(revision, tracked.revisions.len())
}

fn insert_planned_file(
    files: &mut HashMap<PathBuf, XTrackedFile>,
    tracked: XTrackedFile,
) -> Result<(), String> {
    let head = tracked.current_head();
    let path = normalize_storage_path(&head.file.path);
    if files.contains_key(&path) {
        return Err(format!(
            "Rollback would leave multiple tracked files at {}.",
            path.display()
        ));
    }
    files.insert(path, tracked);
    Ok(())
}

impl XTrackedFile {
    fn current_head(&self) -> XFileHead {
        let revision = self
            .revisions
            .back()
            .expect("tracked file must always have a head revision");
        make_head(revision, self.revisions.len())
    }
}

impl XFileStorage {
    fn current_head(&self, path: &Path) -> Option<XFileHead> {
        self.files.get(path).map(|tracked| tracked.current_head())
    }

    fn insert_loaded_file(
        &mut self,
        path: PathBuf,
        text: &str,
        disk_state: Option<XDiskState>,
    ) -> XFileHead {
        if let Some(tracked) = self.files.get(&path) {
            return tracked.current_head();
        }

        let file = build_file_with_new_tags(path.clone(), text);
        let mut tracked = XTrackedFile {
            tracking_id: next_tracking_id(),
            revisions: VecDeque::new(),
            next_revision_number: 1,
            last_disk_state: disk_state,
        };
        let head = push_revision(&mut tracked, file);
        self.files.insert(path, tracked);
        head
    }

    fn write_text(&mut self, path: PathBuf, text: &str) -> XFileHead {
        let file = build_file_with_new_tags(path.clone(), text);
        let tracked = self.files.entry(path.clone()).or_insert_with(|| {
            let mut tracked = XTrackedFile {
                tracking_id: next_tracking_id(),
                revisions: VecDeque::new(),
                next_revision_number: 1,
                last_disk_state: None,
            };
            if !path.exists() {
                push_revision(&mut tracked, build_absent_file(path.clone()));
            }
            tracked
        });
        push_revision(tracked, file)
    }

    fn delete_file(&mut self, path: PathBuf) -> XFileHead {
        let tracked = self
            .files
            .entry(path.clone())
            .or_insert_with(|| XTrackedFile {
                tracking_id: next_tracking_id(),
                revisions: VecDeque::new(),
                next_revision_number: 1,
                last_disk_state: None,
            });
        let head = push_revision_with_metadata(
            tracked,
            build_absent_file(path),
            Some(XFileRevisionMetadata {
                operation: Some("delete".to_string()),
                ..XFileRevisionMetadata::default()
            }),
        );
        tracked.last_disk_state = None;
        head
    }

    fn copy_file(&mut self, source: &Path, destination: PathBuf) -> Result<XFileHead, String> {
        if source == destination {
            return Err("Source and destination must be different for copy.".to_string());
        }
        if self.files.contains_key(&destination) {
            return Err(format!(
                "Destination is already tracked in XFileStorage: {}",
                destination.display()
            ));
        }

        let source_tracked = self
            .files
            .get(source)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", source.display()))?;
        let source_head = source_tracked.current_head();
        if !source_head.file.exists {
            return Err(format!(
                "Cannot copy {} because the current revision is absent.",
                source.display()
            ));
        }

        let mut tracked = XTrackedFile {
            tracking_id: next_tracking_id(),
            revisions: VecDeque::new(),
            next_revision_number: 1,
            last_disk_state: None,
        };
        push_revision(&mut tracked, build_absent_file(destination.clone()));
        let head = push_revision_with_metadata(
            &mut tracked,
            clone_file_with_new_tags(&source_head.file, destination.clone()),
            Some(XFileRevisionMetadata {
                operation: Some("copy".to_string()),
                copied: Some(XPathChangeMetadata {
                    source_path: source_head.file.path.clone(),
                    destination_path: destination.clone(),
                }),
                ..XFileRevisionMetadata::default()
            }),
        );
        self.files.insert(destination, tracked);
        Ok(head)
    }

    fn move_file(&mut self, source: &Path, destination: PathBuf) -> Result<XFileHead, String> {
        if source == destination {
            return Err("Source and destination must be different for move.".to_string());
        }
        if self.files.contains_key(&destination) {
            return Err(format!(
                "Destination is already tracked in XFileStorage: {}",
                destination.display()
            ));
        }

        let mut tracked = self
            .files
            .remove(source)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", source.display()))?;
        let current = tracked.current_head();
        if !current.file.exists {
            self.files.insert(source.to_path_buf(), tracked);
            return Err(format!(
                "Cannot move {} because the current revision is absent.",
                source.display()
            ));
        }

        let head = push_revision_with_metadata(
            &mut tracked,
            clone_file_with_path(&current.file, destination.clone()),
            Some(XFileRevisionMetadata {
                operation: Some("move".to_string()),
                moved: Some(XPathChangeMetadata {
                    source_path: current.file.path.clone(),
                    destination_path: destination.clone(),
                }),
                ..XFileRevisionMetadata::default()
            }),
        );
        tracked.last_disk_state = None;
        self.files.insert(destination, tracked);
        Ok(head)
    }

    fn update_disk_state(
        &mut self,
        path: &Path,
        disk_state: Option<XDiskState>,
    ) -> Result<(), String> {
        let tracked = self
            .files
            .get_mut(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;
        tracked.last_disk_state = disk_state;
        Ok(())
    }

    fn create_checkpoint(&mut self) -> XCheckpointSummary {
        let mut current_paths = self.files.keys().cloned().collect::<Vec<_>>();
        current_paths.sort();

        let entries = self
            .files
            .values()
            .filter_map(|tracked| {
                tracked.revisions.back().map(|revision| {
                    (
                        tracked.tracking_id.clone(),
                        XCheckpointEntry {
                            path: revision.file.path.clone(),
                            revision: revision.number,
                        },
                    )
                })
            })
            .collect();
        self.checkpoint = Some(XFileCheckpoint { entries });

        XCheckpointSummary {
            tracked_files: current_paths.len(),
            current_paths,
        }
    }

    fn diff_against_checkpoint(&self) -> Result<XCheckpointDiffSummary, String> {
        let use_explicit_checkpoint = self.checkpoint.is_some();
        let mut tracked_files = self.files.values().cloned().collect::<Vec<_>>();
        tracked_files.sort_by(|left, right| {
            left.current_head()
                .file
                .path
                .cmp(&right.current_head().file.path)
        });

        let mut entries = Vec::new();
        for tracked in tracked_files {
            let current = tracked.current_head();
            let Some(baseline) = self.rollback_baseline_for_tracked_file(&tracked)? else {
                continue;
            };
            if files_differ(&baseline.file, &current.file) {
                entries.push(XCheckpointDiffEntry {
                    baseline_file: baseline.file,
                    baseline_revision: baseline.revision,
                    current_file: current.file,
                    current_revision: tracked
                        .revisions
                        .back()
                        .map(|revision| revision.number)
                        .expect("tracked file must have a head revision"),
                });
            }
        }

        Ok(XCheckpointDiffSummary {
            used_explicit_checkpoint: use_explicit_checkpoint,
            changed_files: entries.len(),
            entries,
        })
    }

    fn rollback_baseline_for_tracked_file(
        &self,
        tracked: &XTrackedFile,
    ) -> Result<Option<XBaselineTarget>, String> {
        let earliest_revision = tracked
            .revisions
            .front()
            .cloned()
            .expect("tracked file must have at least one revision");
        let use_explicit_checkpoint = self.checkpoint.is_some();
        let checkpoint_target = self
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.entries.get(&tracked.tracking_id));

        if let Some(checkpoint_target) = checkpoint_target {
            let target = tracked
                .revisions
                .iter()
                .find(|revision| revision.number == checkpoint_target.revision)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "Checkpoint revision {} is not retained for {}.",
                        checkpoint_target.revision,
                        checkpoint_target.path.display()
                    )
                })?;
            return Ok(Some(XBaselineTarget {
                revision: target.number,
                file: target.file,
            }));
        }

        if use_explicit_checkpoint && earliest_revision.file.exists {
            return Ok(None);
        }

        Ok(Some(XBaselineTarget {
            revision: earliest_revision.number,
            file: earliest_revision.file,
        }))
    }

    fn plan_rollback(&self) -> Result<XRollbackPlan, String> {
        let use_explicit_checkpoint = self.checkpoint.is_some();
        let mut tracked_files = self.files.values().cloned().collect::<Vec<_>>();
        tracked_files.sort_by(|left, right| {
            left.current_head()
                .file
                .path
                .cmp(&right.current_head().file.path)
        });

        let mut new_files = HashMap::new();
        let mut transitions = Vec::new();
        let mut changed_files = 0usize;
        let mut removed_files = 0usize;
        let mut unchanged_files = 0usize;
        let mut affected_paths = Vec::new();

        for tracked in tracked_files {
            let current_revision = tracked
                .revisions
                .back()
                .cloned()
                .expect("tracked file must have a head revision");
            let decision = match self.rollback_baseline_for_tracked_file(&tracked)? {
                Some(baseline) if !baseline.file.exists => RollbackDecision::Remove,
                Some(baseline) => RollbackDecision::ToRevision(baseline.revision),
                None => RollbackDecision::KeepCurrent,
            };

            match decision {
                RollbackDecision::KeepCurrent => {
                    unchanged_files += 1;
                    insert_planned_file(&mut new_files, tracked)?;
                }
                RollbackDecision::Remove => {
                    changed_files += 1;
                    removed_files += 1;
                    affected_paths.push(current_revision.file.path.clone());
                    transitions.push(XFileTransition {
                        current: current_revision.file,
                        target: None,
                    });
                }
                RollbackDecision::ToRevision(target_revision) => {
                    let mut rolled_back = tracked.clone();
                    while rolled_back
                        .revisions
                        .back()
                        .map(|revision| revision.number > target_revision)
                        .unwrap_or(false)
                    {
                        rolled_back.revisions.pop_back();
                    }
                    let Some(target_head) = rolled_back.revisions.back().cloned() else {
                        return Err(format!(
                            "Rollback target revision {} is not retained for {}.",
                            target_revision,
                            current_revision.file.path.display()
                        ));
                    };
                    if target_head.number != target_revision {
                        return Err(format!(
                            "Rollback target revision {} is not retained for {}.",
                            target_revision,
                            current_revision.file.path.display()
                        ));
                    }

                    let target_file = target_head.file;
                    if files_differ(&current_revision.file, &target_file) {
                        changed_files += 1;
                        affected_paths.push(target_file.path.clone());
                        transitions.push(XFileTransition {
                            current: current_revision.file,
                            target: Some(target_file.clone()),
                        });
                    } else {
                        unchanged_files += 1;
                    }

                    rolled_back.last_disk_state = None;
                    insert_planned_file(&mut new_files, rolled_back)?;
                }
            }
        }

        affected_paths.sort();
        affected_paths.dedup();

        Ok(XRollbackPlan {
            files: new_files,
            transitions,
            summary: XRollbackSummary {
                used_explicit_checkpoint: use_explicit_checkpoint,
                changed_files,
                removed_files,
                unchanged_files,
                affected_paths,
            },
        })
    }

    fn refresh_disk_states(&mut self) -> Result<(), String> {
        for (path, tracked) in self.files.iter_mut() {
            let current = tracked.current_head();
            tracked.last_disk_state = if current.file.exists {
                read_disk_state(path)?
            } else {
                None
            };
        }
        Ok(())
    }

    fn restore_revision(&mut self, path: &Path, revision: usize) -> Result<XFileHead, String> {
        let mut tracked = self
            .files
            .remove(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;

        let current = tracked.current_head();
        let target = tracked
            .revisions
            .iter()
            .find(|candidate| candidate.number == revision)
            .cloned()
            .ok_or_else(|| {
                self.files.insert(path.to_path_buf(), tracked.clone());
                format!(
                    "Revision {} not found in XFileStorage for {}.",
                    revision,
                    path.display()
                )
            })?;
        let target_key = normalize_storage_path(&target.file.path);
        if target_key != path && self.files.contains_key(&target_key) {
            self.files.insert(path.to_path_buf(), tracked);
            return Err(format!(
                "Cannot restore {} to {} because that path is already tracked in XFileStorage.",
                path.display(),
                target.file.path.display()
            ));
        }

        let moved = if current.file.path != target.file.path {
            Some(XPathChangeMetadata {
                source_path: current.file.path.clone(),
                destination_path: target.file.path.clone(),
            })
        } else {
            None
        };
        let head = push_revision_with_metadata(
            &mut tracked,
            target.file,
            Some(XFileRevisionMetadata {
                operation: Some("restore".to_string()),
                moved,
                ..XFileRevisionMetadata::default()
            }),
        );
        tracked.last_disk_state = None;
        let insert_key = normalize_storage_path(&head.file.path);
        self.files.insert(insert_key, tracked);
        Ok(head)
    }

    fn discard_head_revision(&mut self, path: &Path) -> Result<XFileHead, String> {
        let mut tracked = self
            .files
            .remove(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;

        if tracked.revisions.len() < 2 {
            self.files.insert(path.to_path_buf(), tracked);
            return Err(format!(
                "No previous XFileStorage revision is available to revert for {}.",
                path.display()
            ));
        }

        let discarded = tracked
            .revisions
            .pop_back()
            .expect("checked revision list length");
        let head = tracked.current_head();
        let insert_key = normalize_storage_path(&head.file.path);
        if insert_key != path && self.files.contains_key(&insert_key) {
            tracked.revisions.push_back(discarded);
            self.files.insert(path.to_path_buf(), tracked);
            return Err(format!(
                "Cannot revert {} because {} is already tracked in XFileStorage.",
                path.display(),
                head.file.path.display()
            ));
        }
        tracked.last_disk_state = None;
        self.files.insert(insert_key, tracked);
        Ok(head)
    }

    fn apply_mutations(
        &mut self,
        path: &Path,
        base_version: Option<&str>,
        operations: &[XLineMutation],
    ) -> Result<XFileHead, String> {
        let tracked = self
            .files
            .get_mut(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;

        let latest = tracked
            .revisions
            .back()
            .cloned()
            .ok_or_else(|| format!("No revision found for {}", path.display()))?;
        if !latest.file.exists {
            return Err(format!(
                "Cannot edit {} because the current revision is absent. Use Write to recreate the file.",
                path.display()
            ));
        }
        let latest_version = compute_file_version(&latest.file);
        if let Some(base_version) = base_version {
            if !base_version.trim().is_empty() && base_version != latest_version {
                return Err(format!(
                    "Edit base_version mismatch: expected {}, current {}",
                    base_version, latest_version
                ));
            }
        }

        let mut file = latest.file;
        for operation in operations {
            apply_mutation_to_file(&mut file, operation)?;
        }

        Ok(push_revision(tracked, file))
    }
}

fn apply_mutation_to_file(file: &mut XFile, operation: &XLineMutation) -> Result<(), String> {
    match operation {
        XLineMutation::ReplaceLine { tag, new_text } => {
            let idx = find_line_index(file, tag)?;
            file.content[idx].content = new_text.clone();
        }
        XLineMutation::InsertBefore { tag, new_lines } => {
            let idx = find_line_index(file, tag)?;
            let inserted = build_inserted_lines(new_lines);
            file.content.splice(idx..idx, inserted);
        }
        XLineMutation::InsertAfter { tag, new_lines } => {
            let idx = find_line_index(file, tag)?;
            let inserted = build_inserted_lines(new_lines);
            file.content.splice((idx + 1)..(idx + 1), inserted);
        }
        XLineMutation::DeleteLine { tag } => {
            let idx = find_line_index(file, tag)?;
            file.content.remove(idx);
        }
        XLineMutation::DeleteRange { from_tag, to_tag } => {
            let (start_idx, end_idx) = find_range_bounds(file, from_tag, to_tag)?;
            file.content.drain(start_idx..=end_idx);
        }
        XLineMutation::MoveRange {
            from_tag,
            to_tag,
            move_after_tag,
        } => {
            let (start_idx, end_idx) = find_range_bounds(file, from_tag, to_tag)?;
            let move_after_idx = find_line_index(file, move_after_tag)?;
            if (start_idx..=end_idx).contains(&move_after_idx) {
                return Err(format!(
                    "move_after_tag '{}' must be outside the moved range in {}",
                    move_after_tag,
                    file.path.display()
                ));
            }

            let moved_len = end_idx - start_idx + 1;
            let moved: Vec<XLine> = file.content.drain(start_idx..=end_idx).collect();
            let insertion_idx = if move_after_idx < start_idx {
                move_after_idx + 1
            } else {
                move_after_idx - moved_len + 1
            };
            file.content.splice(insertion_idx..insertion_idx, moved);
        }
        XLineMutation::OverwriteRange {
            from_tag,
            to_tag,
            new_content,
        } => {
            let (start_idx, end_idx) = find_range_bounds(file, from_tag, to_tag)?;
            overwrite_range(file, start_idx, end_idx, new_content);
        }
        XLineMutation::RegexReplace {
            from_tag,
            to_tag,
            pattern,
            replacement,
        } => {
            let (start_idx, end_idx) = find_range_bounds(file, from_tag, to_tag)?;
            regex_replace_range(file, start_idx, end_idx, pattern, replacement)?;
        }
    }

    renumber_lines(&mut file.content);
    Ok(())
}

fn overwrite_range(file: &mut XFile, start_idx: usize, end_idx: usize, new_content: &str) {
    let replacement_lines = split_text_into_lines(new_content);
    let range_len = end_idx - start_idx + 1;
    let retained_len = replacement_lines.len().min(range_len);

    for (offset, line) in replacement_lines.iter().take(retained_len).enumerate() {
        file.content[start_idx + offset].content = line.clone();
    }

    if replacement_lines.len() < range_len {
        file.content
            .drain((start_idx + retained_len)..(start_idx + range_len));
    } else if replacement_lines.len() > range_len {
        let inserted = build_inserted_lines(&replacement_lines[range_len..]);
        file.content
            .splice((start_idx + range_len)..(start_idx + range_len), inserted);
    }
}

fn regex_replace_range(
    file: &mut XFile,
    start_idx: usize,
    end_idx: usize,
    pattern: &str,
    replacement: &str,
) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("regex_replace requires non-empty `pattern`.".to_string());
    }

    let regex = Regex::new(pattern)
        .map_err(|err| format!("Invalid regex_replace pattern '{}': {}", pattern, err))?;

    for idx in start_idx..=end_idx {
        let replaced = regex
            .replace_all(&file.content[idx].content, replacement)
            .into_owned();
        if replaced.contains('\n') || replaced.contains('\r') {
            return Err(format!(
                "regex_replace must not create multi-line content for tag '{}' in {}",
                file.content[idx].tag,
                file.path.display()
            ));
        }
        file.content[idx].content = replaced;
    }

    Ok(())
}

fn build_inserted_lines(lines: &[String]) -> Vec<XLine> {
    lines
        .iter()
        .map(|line| XLine {
            content: line.clone(),
            line_number: 0,
            tag: next_tag(),
        })
        .collect()
}

fn find_line_index(file: &XFile, tag: &str) -> Result<usize, String> {
    file.content
        .iter()
        .position(|line| line.tag == tag)
        .ok_or_else(|| format!("Unknown tag '{}' in {}", tag, file.path.display()))
}

fn find_range_bounds(file: &XFile, from_tag: &str, to_tag: &str) -> Result<(usize, usize), String> {
    let start_idx = find_line_index(file, from_tag)?;
    let end_idx = find_line_index(file, to_tag)?;
    if end_idx < start_idx {
        return Err(format!(
            "to_tag '{}' must not come before from_tag '{}' in {}",
            to_tag,
            from_tag,
            file.path.display()
        ));
    }
    Ok((start_idx, end_idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn tempdir_in_system_tmp(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(std::env::temp_dir())
            .unwrap()
    }

    async fn write_head_to_disk(session_id: &str, head: &XFileHead) {
        apply_file_to_disk(&head.file.path, &head.file)
            .await
            .unwrap();
        record_disk_state(session_id, &head.file.path).unwrap();
    }

    async fn discard_head_and_apply(session_id: &str, path: &Path) -> XFileHead {
        let revisions = list_revisions(session_id, path).unwrap();
        let current = revisions.last().cloned().unwrap();
        let previous = revisions[revisions.len() - 2].clone();

        apply_file_transition_to_disk(&current.file, &previous.file)
            .await
            .unwrap();
        let head = discard_head_revision(session_id, path).unwrap();
        record_disk_state(session_id, &head.file.path).unwrap();
        head
    }

    async fn restore_revision_and_apply(
        session_id: &str,
        path: &Path,
        revision: usize,
    ) -> XFileHead {
        let revisions = list_revisions(session_id, path).unwrap();
        let current = revisions.last().cloned().unwrap();
        let target = revisions
            .iter()
            .find(|candidate| candidate.number == revision)
            .cloned()
            .unwrap();

        apply_file_transition_to_disk(&current.file, &target.file)
            .await
            .unwrap();
        let head = restore_revision(session_id, path, revision).unwrap();
        record_disk_state(session_id, &head.file.path).unwrap();
        head
    }

    #[test]
    fn write_creates_revisioned_file_with_fresh_tags() {
        clear_session_xfile_storage("xstorage-write");
        let path = PathBuf::from("/tmp/example.txt");

        let first = store_written_text("xstorage-write", &path, "a\nb\n");
        let second = store_written_text("xstorage-write", &path, "c\nd\n");
        let revisions = list_revisions("xstorage-write", &path).unwrap();

        assert_eq!(first.revision_count, 2);
        assert_eq!(first.file.content.len(), 2);
        assert_eq!(revisions[0].number, 1);
        assert!(!revisions[0].file.exists);
        assert_eq!(second.revision_count, 3);
        assert_ne!(first.file.content[0].tag, second.file.content[0].tag);
        assert_eq!(second.rendered_content, "c\nd\n");
    }

    #[test]
    fn replace_keeps_tag_but_insert_gets_new_tag() {
        clear_session_xfile_storage("xstorage-edit");
        let path = PathBuf::from("/tmp/edit.txt");
        let first = store_written_text("xstorage-edit", &path, "one\ntwo\n");
        let first_tag = first.file.content[0].tag.clone();
        let second_tag = first.file.content[1].tag.clone();

        let edited = apply_mutations(
            "xstorage-edit",
            &path,
            Some(&first.current_version),
            &[
                XLineMutation::ReplaceLine {
                    tag: first_tag.clone(),
                    new_text: "ONE".to_string(),
                },
                XLineMutation::InsertAfter {
                    tag: first_tag.clone(),
                    new_lines: vec!["middle".to_string()],
                },
            ],
        )
        .unwrap();

        assert_eq!(edited.revision_count, 3);
        assert_eq!(edited.file.content[0].tag, first_tag);
        assert_eq!(edited.file.content[0].content, "ONE");
        assert_eq!(edited.file.content[2].tag, second_tag);
        assert_eq!(edited.file.content[1].content, "middle");
        assert_ne!(edited.file.content[1].tag, edited.file.content[0].tag);
    }

    #[test]
    fn delete_range_removes_inclusive_lines() {
        clear_session_xfile_storage("xstorage-delete-range");
        let path = PathBuf::from("/tmp/delete-range.txt");
        let first = store_written_text("xstorage-delete-range", &path, "one\ntwo\nthree\nfour\n");

        let edited = apply_mutations(
            "xstorage-delete-range",
            &path,
            Some(&first.current_version),
            &[XLineMutation::DeleteRange {
                from_tag: first.file.content[1].tag.clone(),
                to_tag: first.file.content[2].tag.clone(),
            }],
        )
        .unwrap();

        assert_eq!(edited.rendered_content, "one\nfour\n");
        assert_eq!(edited.file.content.len(), 2);
        assert_eq!(edited.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(edited.file.content[1].tag, first.file.content[3].tag);
    }

    #[test]
    fn move_range_preserves_tags_and_reorders_lines() {
        clear_session_xfile_storage("xstorage-move-range");
        let path = PathBuf::from("/tmp/move-range.txt");
        let first = store_written_text("xstorage-move-range", &path, "one\ntwo\nthree\nfour\n");

        let edited = apply_mutations(
            "xstorage-move-range",
            &path,
            Some(&first.current_version),
            &[XLineMutation::MoveRange {
                from_tag: first.file.content[1].tag.clone(),
                to_tag: first.file.content[2].tag.clone(),
                move_after_tag: first.file.content[3].tag.clone(),
            }],
        )
        .unwrap();

        assert_eq!(edited.rendered_content, "one\nfour\ntwo\nthree\n");
        assert_eq!(edited.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(edited.file.content[1].tag, first.file.content[3].tag);
        assert_eq!(edited.file.content[2].tag, first.file.content[1].tag);
        assert_eq!(edited.file.content[3].tag, first.file.content[2].tag);
    }

    #[test]
    fn move_range_rejects_move_after_tag_inside_range() {
        clear_session_xfile_storage("xstorage-move-range-error");
        let path = PathBuf::from("/tmp/move-range-error.txt");
        let first = store_written_text("xstorage-move-range-error", &path, "one\ntwo\nthree\n");

        let error = apply_mutations(
            "xstorage-move-range-error",
            &path,
            Some(&first.current_version),
            &[XLineMutation::MoveRange {
                from_tag: first.file.content[0].tag.clone(),
                to_tag: first.file.content[1].tag.clone(),
                move_after_tag: first.file.content[1].tag.clone(),
            }],
        )
        .unwrap_err();

        assert!(error.contains("must be outside the moved range"));
    }

    #[test]
    fn overwrite_range_keeps_overlapping_tags_and_adds_new_ones() {
        clear_session_xfile_storage("xstorage-overwrite-range");
        let path = PathBuf::from("/tmp/overwrite-range.txt");
        let first = store_written_text("xstorage-overwrite-range", &path, "one\ntwo\nthree\n");

        let edited = apply_mutations(
            "xstorage-overwrite-range",
            &path,
            Some(&first.current_version),
            &[XLineMutation::OverwriteRange {
                from_tag: first.file.content[0].tag.clone(),
                to_tag: first.file.content[1].tag.clone(),
                new_content: "ONE\nTWO\nMIDDLE".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(edited.rendered_content, "ONE\nTWO\nMIDDLE\nthree\n");
        assert_eq!(edited.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(edited.file.content[1].tag, first.file.content[1].tag);
        assert_eq!(edited.file.content[3].tag, first.file.content[2].tag);
        assert_ne!(edited.file.content[2].tag, first.file.content[0].tag);
        assert_ne!(edited.file.content[2].tag, first.file.content[1].tag);
    }

    #[test]
    fn regex_replace_preserves_tags_and_applies_per_line() {
        clear_session_xfile_storage("xstorage-regex-replace");
        let path = PathBuf::from("/tmp/regex-replace.txt");
        let first = store_written_text("xstorage-regex-replace", &path, "a1\nb2\nc3\n");

        let edited = apply_mutations(
            "xstorage-regex-replace",
            &path,
            Some(&first.current_version),
            &[XLineMutation::RegexReplace {
                from_tag: first.file.content[0].tag.clone(),
                to_tag: first.file.content[1].tag.clone(),
                pattern: r"\d".to_string(),
                replacement: "X".to_string(),
            }],
        )
        .unwrap();

        assert_eq!(edited.rendered_content, "aX\nbX\nc3\n");
        assert_eq!(edited.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(edited.file.content[1].tag, first.file.content[1].tag);
        assert_eq!(edited.file.content[2].tag, first.file.content[2].tag);
    }

    #[test]
    fn regex_replace_rejects_multiline_replacement_output() {
        clear_session_xfile_storage("xstorage-regex-replace-multiline");
        let path = PathBuf::from("/tmp/regex-replace-multiline.txt");
        let first = store_written_text("xstorage-regex-replace-multiline", &path, "a1\n");

        let error = apply_mutations(
            "xstorage-regex-replace-multiline",
            &path,
            Some(&first.current_version),
            &[XLineMutation::RegexReplace {
                from_tag: first.file.content[0].tag.clone(),
                to_tag: first.file.content[0].tag.clone(),
                pattern: "1".to_string(),
                replacement: "1\n2".to_string(),
            }],
        )
        .unwrap_err();

        assert!(error.contains("must not create multi-line content"));
    }

    #[test]
    fn restore_revision_appends_new_head() {
        clear_session_xfile_storage("xstorage-restore");
        let path = PathBuf::from("/tmp/restore.txt");
        store_written_text("xstorage-restore", &path, "first\n");
        let second = store_written_text("xstorage-restore", &path, "second\n");

        let restored = restore_revision("xstorage-restore", &path, 2).unwrap();

        assert_eq!(second.revision_count, 3);
        assert_eq!(restored.revision_count, 4);
        assert_eq!(restored.rendered_content, "first\n");

        let revisions = list_revisions("xstorage-restore", &path).unwrap();
        assert_eq!(revisions.len(), 4);
        assert_eq!(revisions.last().unwrap().number, 4);
    }

    #[test]
    fn discard_head_revision_restores_previous_head() {
        clear_session_xfile_storage("xstorage-discard");
        let path = PathBuf::from("/tmp/discard.txt");
        store_written_text("xstorage-discard", &path, "first\n");
        let second = store_written_text("xstorage-discard", &path, "second\n");

        discard_head_revision("xstorage-discard", &path).unwrap();
        let head = try_get_head("xstorage-discard", &path).unwrap();

        assert_eq!(second.revision_count, 3);
        assert_eq!(head.revision_count, 2);
        assert_eq!(head.rendered_content, "first\n");
    }

    #[tokio::test]
    async fn storage_handles_copy_move_delete_and_reverts_on_real_tmp_files() {
        let tmp = tempdir_in_system_tmp("xstorage-flow-");
        assert!(tmp.path().starts_with(std::env::temp_dir()));

        let session_id = format!("xstorage-flow-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let source = tmp.path().join("source.txt");
        let copy = tmp.path().join("copies/source-copy.txt");
        let moved = tmp.path().join("moved/source-moved.txt");

        tokio::fs::write(&source, "alpha\nbeta\ngamma\n")
            .await
            .unwrap();

        let source_head = ensure_loaded(&session_id, &source).await.unwrap();
        assert_eq!(source_head.rendered_content, "alpha\nbeta\ngamma\n");
        assert_eq!(list_revisions(&session_id, &source).unwrap().len(), 1);

        let copy_head = copy_tracked_file(&session_id, &source, &copy).unwrap();
        write_head_to_disk(&session_id, &copy_head).await;
        assert_eq!(
            tokio::fs::read_to_string(&copy).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        for (source_line, copy_line) in source_head
            .file
            .content
            .iter()
            .zip(copy_head.file.content.iter())
        {
            assert_eq!(source_line.content, copy_line.content);
            assert_ne!(source_line.tag, copy_line.tag);
        }

        let copy_revisions = list_revisions(&session_id, &copy).unwrap();
        assert_eq!(copy_revisions.len(), 2);
        assert!(!copy_revisions[0].file.exists);
        assert_eq!(
            copy_revisions[1]
                .metadata
                .as_ref()
                .unwrap()
                .operation
                .as_deref(),
            Some("copy")
        );
        let copied = copy_revisions[1]
            .metadata
            .as_ref()
            .unwrap()
            .copied
            .as_ref()
            .unwrap();
        assert_eq!(copied.source_path, source);
        assert_eq!(copied.destination_path, copy);

        let copy_absent = restore_revision_and_apply(&session_id, &copy, 1).await;
        assert!(!copy.exists());
        assert!(!copy_absent.file.exists);
        assert_eq!(copy_absent.file.path, copy);

        let copy_restored = restore_revision_and_apply(&session_id, &copy, 2).await;
        assert_eq!(
            tokio::fs::read_to_string(&copy).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        assert_eq!(copy_restored.file.path, copy);
        for (restored_line, original_copy_line) in copy_restored
            .file
            .content
            .iter()
            .zip(copy_head.file.content.iter())
        {
            assert_eq!(restored_line.tag, original_copy_line.tag);
        }

        let current_source = try_get_head(&session_id, &source).unwrap();
        let moved_head = move_tracked_file(&session_id, &source, &moved).unwrap();
        apply_file_transition_to_disk(&current_source.file, &moved_head.file)
            .await
            .unwrap();
        record_disk_state(&session_id, &moved_head.file.path).unwrap();

        assert!(!source.exists());
        assert_eq!(
            tokio::fs::read_to_string(&moved).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        assert!(try_get_head(&session_id, &source).is_none());

        for (source_line, moved_line) in source_head
            .file
            .content
            .iter()
            .zip(moved_head.file.content.iter())
        {
            assert_eq!(source_line.tag, moved_line.tag);
        }

        let moved_revisions = list_revisions(&session_id, &moved).unwrap();
        assert_eq!(moved_revisions.len(), 2);
        assert_eq!(moved_revisions[0].file.path, source);
        assert_eq!(moved_revisions[1].file.path, moved);
        assert_eq!(
            moved_revisions[1]
                .metadata
                .as_ref()
                .unwrap()
                .operation
                .as_deref(),
            Some("move")
        );
        let moved_metadata = moved_revisions[1]
            .metadata
            .as_ref()
            .unwrap()
            .moved
            .as_ref()
            .unwrap();
        assert_eq!(moved_metadata.source_path, source);
        assert_eq!(moved_metadata.destination_path, moved);

        let current_moved = try_get_head(&session_id, &moved).unwrap();
        let deleted_head = store_deleted_file(&session_id, &moved);
        apply_file_transition_to_disk(&current_moved.file, &deleted_head.file)
            .await
            .unwrap();
        record_disk_state(&session_id, &deleted_head.file.path).unwrap();

        assert!(!moved.exists());
        assert!(!try_get_head(&session_id, &moved).unwrap().file.exists);
        assert_eq!(
            list_revisions(&session_id, &moved)
                .unwrap()
                .last()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .operation
                .as_deref(),
            Some("delete")
        );

        let moved_after_delete_revert = discard_head_and_apply(&session_id, &moved).await;
        assert_eq!(moved_after_delete_revert.file.path, moved);
        assert_eq!(
            tokio::fs::read_to_string(&moved).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        let restored_to_source = restore_revision_and_apply(&session_id, &moved, 1).await;
        assert_eq!(restored_to_source.file.path, source);
        assert!(source.exists());
        assert!(!moved.exists());
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        let restored_revisions = list_revisions(&session_id, &source).unwrap();
        assert_eq!(
            restored_revisions
                .last()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .operation
                .as_deref(),
            Some("restore")
        );
        let restored_move = restored_revisions
            .last()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .moved
            .as_ref()
            .unwrap();
        assert_eq!(restored_move.source_path, moved);
        assert_eq!(restored_move.destination_path, source);

        let moved_again = discard_head_and_apply(&session_id, &source).await;
        assert_eq!(moved_again.file.path, moved);
        assert!(try_get_head(&session_id, &source).is_none());
        assert_eq!(
            tokio::fs::read_to_string(&moved).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        let source_again = discard_head_and_apply(&session_id, &moved).await;
        assert_eq!(source_again.file.path, source);
        assert!(source.exists());
        assert!(!moved.exists());
        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        assert!(try_get_head(&session_id, &moved).is_none());

        for (restored_line, original_line) in source_again
            .file
            .content
            .iter()
            .zip(source_head.file.content.iter())
        {
            assert_eq!(restored_line.tag, original_line.tag);
        }
    }

    #[tokio::test]
    async fn checkpoint_and_rollback_restore_moved_deleted_and_copied_files() {
        let tmp = tempdir_in_system_tmp("xstorage-checkpoint-");
        let session_id = format!("xstorage-checkpoint-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let source = tmp.path().join("source.txt");
        let deleted = tmp.path().join("deleted.txt");
        let copy = tmp.path().join("copy.txt");
        let moved = tmp.path().join("moved/source.txt");

        tokio::fs::write(&source, "alpha\nbeta\n").await.unwrap();
        tokio::fs::write(&deleted, "delete-me\n").await.unwrap();

        let source_head = ensure_loaded(&session_id, &source).await.unwrap();
        let deleted_head = ensure_loaded(&session_id, &deleted).await.unwrap();
        let checkpoint = create_checkpoint(&session_id);
        assert_eq!(checkpoint.tracked_files, 2);

        let copy_head = copy_tracked_file(&session_id, &source, &copy).unwrap();
        write_head_to_disk(&session_id, &copy_head).await;

        let current_source = try_get_head(&session_id, &source).unwrap();
        let moved_head = move_tracked_file(&session_id, &source, &moved).unwrap();
        apply_file_transition_to_disk(&current_source.file, &moved_head.file)
            .await
            .unwrap();
        record_disk_state(&session_id, &moved_head.file.path).unwrap();

        let deleted_now = store_deleted_file(&session_id, &deleted);
        apply_file_transition_to_disk(&deleted_head.file, &deleted_now.file)
            .await
            .unwrap();
        record_disk_state(&session_id, &deleted_now.file.path).unwrap();

        let summary = rollback_to_checkpoint(&session_id).await.unwrap();
        assert!(summary.used_explicit_checkpoint);
        assert_eq!(summary.changed_files, 3);
        assert_eq!(summary.removed_files, 1);

        assert_eq!(
            tokio::fs::read_to_string(&source).await.unwrap(),
            "alpha\nbeta\n"
        );
        assert!(!copy.exists());
        assert!(!moved.exists());
        assert_eq!(
            tokio::fs::read_to_string(&deleted).await.unwrap(),
            "delete-me\n"
        );

        assert!(try_get_head(&session_id, &copy).is_none());
        assert!(try_get_head(&session_id, &moved).is_none());

        let restored_source = try_get_head(&session_id, &source).unwrap();
        let restored_deleted = try_get_head(&session_id, &deleted).unwrap();
        assert_eq!(list_revisions(&session_id, &source).unwrap().len(), 1);
        assert_eq!(list_revisions(&session_id, &deleted).unwrap().len(), 1);
        for (restored_line, original_line) in restored_source
            .file
            .content
            .iter()
            .zip(source_head.file.content.iter())
        {
            assert_eq!(restored_line.tag, original_line.tag);
        }
        assert_eq!(restored_deleted.rendered_content, "delete-me\n");
    }

    #[tokio::test]
    async fn rollback_without_explicit_checkpoint_uses_earliest_retained_state() {
        let tmp = tempdir_in_system_tmp("xstorage-implicit-");
        let session_id = format!("xstorage-implicit-{}", Uuid::new_v4());
        clear_session_xfile_storage(&session_id);

        let existing = tmp.path().join("existing.txt");
        let created = tmp.path().join("created.txt");

        tokio::fs::write(&existing, "before\n").await.unwrap();
        ensure_loaded(&session_id, &existing).await.unwrap();

        let updated = store_written_text(&session_id, &existing, "after\n");
        write_head_to_disk(&session_id, &updated).await;

        let created_head = store_written_text(&session_id, &created, "created\n");
        write_head_to_disk(&session_id, &created_head).await;

        let summary = rollback_to_checkpoint(&session_id).await.unwrap();
        assert!(!summary.used_explicit_checkpoint);
        assert_eq!(summary.changed_files, 2);
        assert_eq!(summary.removed_files, 1);

        assert_eq!(
            tokio::fs::read_to_string(&existing).await.unwrap(),
            "before\n"
        );
        assert!(!created.exists());
        assert!(try_get_head(&session_id, &created).is_none());

        let restored_existing = try_get_head(&session_id, &existing).unwrap();
        assert_eq!(restored_existing.rendered_content, "before\n");
        assert_eq!(list_revisions(&session_id, &existing).unwrap().len(), 1);
    }

    #[test]
    fn history_is_capped_to_sixteen_revisions() {
        clear_session_xfile_storage("xstorage-cap");
        let path = PathBuf::from("/tmp/cap.txt");
        let mut head = store_written_text("xstorage-cap", &path, "0\n");
        for n in 1..20 {
            head = store_written_text("xstorage-cap", &path, &format!("{}\n", n));
        }

        assert_eq!(head.revision_count, 16);
        let revisions = list_revisions("xstorage-cap", &path).unwrap();
        assert_eq!(revisions.len(), 16);
        assert_eq!(revisions.first().unwrap().number, 6);
        assert_eq!(revisions.last().unwrap().number, 21);
    }

    #[test]
    fn save_and_restore_session_storage_preserves_tags_and_revisions() {
        let session_id = "xstorage-persist";
        clear_session_xfile_storage(session_id);
        let path = PathBuf::from("/tmp/persist.txt");
        let first = store_written_text(session_id, &path, "one\ntwo\n");
        let second = apply_mutations(
            session_id,
            &path,
            Some(&first.current_version),
            &[XLineMutation::InsertAfter {
                tag: first.file.content[0].tag.clone(),
                new_lines: vec!["middle".to_string()],
            }],
        )
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("session.xfiles.json");
        save_session_xfile_storage_to_path(session_id, &snapshot).unwrap();

        clear_session_xfile_storage(session_id);
        assert!(try_get_head(session_id, &path).is_none());

        assert!(load_session_xfile_storage_from_path(session_id, &snapshot).unwrap());
        let restored = try_get_head(session_id, &path).unwrap();

        assert_eq!(restored.revision_count, 3);
        assert_eq!(restored.rendered_content, second.rendered_content);
        assert_eq!(restored.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(restored.file.content[2].tag, first.file.content[1].tag);

        let revisions = list_revisions(session_id, &path).unwrap();
        assert_eq!(revisions.len(), 3);
        assert_eq!(revisions[0].number, 1);
        assert_eq!(revisions[0].file.exists, false);
        assert_eq!(revisions[1].number, 2);
        assert_eq!(revisions[2].number, 3);
    }

    #[test]
    fn delete_revision_marks_file_absent() {
        clear_session_xfile_storage("xstorage-delete");
        let path = PathBuf::from("/tmp/delete.txt");

        store_written_text("xstorage-delete", &path, "first\n");
        let deleted = store_deleted_file("xstorage-delete", &path);

        assert_eq!(deleted.revision_count, 3);
        assert!(!deleted.file.exists);
        assert_eq!(deleted.rendered_content, "");
    }

    #[tokio::test]
    async fn sync_if_disk_changed_records_absent_revision_for_deleted_file() {
        let session_id = "xstorage-sync-delete";
        clear_session_xfile_storage(session_id);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sync-delete.txt");
        tokio::fs::write(&path, "first\n").await.unwrap();

        ensure_loaded(session_id, &path).await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();

        let update = sync_if_disk_changed(session_id, &path)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(update.stats.deleted, 1);
        assert_eq!(update.revision_count, 2);

        let head = try_get_head(session_id, &path).unwrap();
        assert!(!head.file.exists);
        assert_eq!(head.rendered_content, "");
    }

    #[test]
    fn restoring_storage_advances_tag_counter() {
        let source_session = "xstorage-source-tags";
        clear_session_xfile_storage(source_session);
        let path = PathBuf::from("/tmp/tag-counter.txt");
        let initial = store_written_text(source_session, &path, "alpha\nbeta\n");
        let highest_existing_tag = initial
            .file
            .content
            .iter()
            .filter_map(|line| parse_tag_counter(&line.tag))
            .max()
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("session.xfiles.json");
        save_session_xfile_storage_to_path(source_session, &snapshot).unwrap();

        let restored_session = "xstorage-restored-tags";
        clear_session_xfile_storage(restored_session);
        load_session_xfile_storage_from_path(restored_session, &snapshot).unwrap();

        let restored = apply_mutations(
            restored_session,
            &path,
            Some(&initial.current_version),
            &[XLineMutation::InsertAfter {
                tag: initial.file.content[1].tag.clone(),
                new_lines: vec!["gamma".to_string()],
            }],
        )
        .unwrap();

        let new_tag_value = parse_tag_counter(&restored.file.content[2].tag).unwrap();
        assert!(new_tag_value > highest_existing_tag);
    }

    #[test]
    fn restoring_from_missing_sidecar_clears_in_memory_session_state() {
        let session_id = "xstorage-missing-sidecar";
        clear_session_xfile_storage(session_id);
        let path = PathBuf::from("/tmp/missing-sidecar.txt");
        store_written_text(session_id, &path, "alpha\n");
        assert!(try_get_head(session_id, &path).is_some());

        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.xfiles.json");
        assert!(!missing.exists());

        assert!(!load_session_xfile_storage_from_path(session_id, &missing).unwrap());
        assert!(try_get_head(session_id, &path).is_none());
    }
}
