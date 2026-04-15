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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_REVISIONS: usize = 16;

static XFILE_STORAGE_REGISTRY: Lazy<dashmap::DashMap<String, Arc<Mutex<XFileStorage>>>> =
    Lazy::new(dashmap::DashMap::new);
static XFILE_TAG_COUNTER: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XLine {
    pub content: String,
    pub line_number: usize,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XFile {
    pub path: PathBuf,
    pub content: Vec<XLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XFileRevision {
    pub number: usize,
    pub file: XFile,
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
    pub line_count: usize,
    pub current_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XTrackedFile {
    revisions: VecDeque<XFileRevision>,
    next_revision_number: usize,
    last_disk_state: Option<XDiskState>,
}

#[derive(Debug, Default)]
pub struct XFileStorage {
    files: HashMap<PathBuf, XTrackedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedXFileStorage {
    version: u32,
    files: HashMap<PathBuf, XTrackedFile>,
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
        })),
    );
    Ok(true)
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

    let text = tokio::fs::read_to_string(&key)
        .await
        .map_err(|e| format!("Failed to read file: {}", e))?;
    let disk_state = read_disk_state(&key).ok();

    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    if let Some(head) = guard.current_head(&key) {
        return Ok(head);
    }

    Ok(guard.insert_loaded_file(key, &text, disk_state))
}

pub fn store_loaded_if_missing(session_id: &str, path: &Path, text: &str) -> XFileHead {
    let key = normalize_storage_path(path);
    let disk_state = read_disk_state(&key).ok();
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
    guard.update_disk_state(&key, Some(disk_state))
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
        if tracked.last_disk_state.as_ref() == Some(&current_disk_state) {
            return Ok(None);
        }
    }

    let disk_text = tokio::fs::read_to_string(&key)
        .await
        .map_err(|e| format!("Failed to read file: {}", e))?;

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
    if tracked.last_disk_state.is_none() && render_file(&latest.file) == disk_text {
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
            tracked.revisions.back().map(|revision| {
                let rendered_content = render_file(&revision.file);
                XTrackedFileSummary {
                    path: path.clone(),
                    revision_count: tracked.revisions.len(),
                    current_revision: revision.number,
                    line_count: revision.file.content.len(),
                    current_version: compute_version(&rendered_content),
                }
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

pub fn discard_head_revision(session_id: &str, path: &Path) -> Result<(), String> {
    let key = normalize_storage_path(path);
    let storage = session_xfile_storage(session_id);
    let mut guard = storage.lock();
    guard.discard_head_revision(&key)
}

pub fn render_file(file: &XFile) -> String {
    if file.content.is_empty() {
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

pub(crate) fn next_tag() -> String {
    let next = XFILE_TAG_COUNTER.fetch_add(1, Ordering::Relaxed);
    STANDARD.encode(next.to_string())
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

fn read_disk_state(path: &Path) -> Result<XDiskState, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("Failed to read metadata for {}: {}", path.display(), e))?;
    Ok(XDiskState {
        modified_ns: metadata.modified().ok().and_then(system_time_to_nanos),
        len: metadata.len(),
    })
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

    XFile { path, content }
}

fn make_head(revision: &XFileRevision, revision_count: usize) -> XFileHead {
    let rendered_content = render_file(&revision.file);
    let current_version = compute_version(&rendered_content);
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
    let number = tracked.next_revision_number;
    tracked.next_revision_number += 1;
    tracked.revisions.push_back(XFileRevision { number, file });
    while tracked.revisions.len() > MAX_REVISIONS {
        tracked.revisions.pop_front();
    }

    let revision = tracked
        .revisions
        .back()
        .expect("revision just pushed must exist");
    make_head(revision, tracked.revisions.len())
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
        let tracked = self.files.entry(path).or_insert_with(|| XTrackedFile {
            revisions: VecDeque::new(),
            next_revision_number: 1,
            last_disk_state: None,
        });
        push_revision(tracked, file)
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

    fn restore_revision(&mut self, path: &Path, revision: usize) -> Result<XFileHead, String> {
        let tracked = self
            .files
            .get_mut(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;

        let target = tracked
            .revisions
            .iter()
            .find(|candidate| candidate.number == revision)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Revision {} not found in XFileStorage for {}.",
                    revision,
                    path.display()
                )
            })?;

        Ok(push_revision(tracked, target.file))
    }

    fn discard_head_revision(&mut self, path: &Path) -> Result<(), String> {
        let tracked = self
            .files
            .get_mut(path)
            .ok_or_else(|| format!("File is not loaded in XFileStorage: {}", path.display()))?;

        if tracked.revisions.len() < 2 {
            return Err(format!(
                "No previous XFileStorage revision is available to revert for {}.",
                path.display()
            ));
        }

        tracked.revisions.pop_back();
        Ok(())
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
        let latest_rendered = render_file(&latest.file);
        let latest_version = compute_version(&latest_rendered);
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

    #[test]
    fn write_creates_revisioned_file_with_fresh_tags() {
        clear_session_xfile_storage("xstorage-write");
        let path = PathBuf::from("/tmp/example.txt");

        let first = store_written_text("xstorage-write", &path, "a\nb\n");
        let second = store_written_text("xstorage-write", &path, "c\nd\n");

        assert_eq!(first.file.content.len(), 2);
        assert_eq!(second.revision_count, 2);
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

        assert_eq!(edited.revision_count, 2);
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

        let restored = restore_revision("xstorage-restore", &path, 1).unwrap();

        assert_eq!(second.revision_count, 2);
        assert_eq!(restored.revision_count, 3);
        assert_eq!(restored.rendered_content, "first\n");

        let revisions = list_revisions("xstorage-restore", &path).unwrap();
        assert_eq!(revisions.len(), 3);
        assert_eq!(revisions.last().unwrap().number, 3);
    }

    #[test]
    fn discard_head_revision_restores_previous_head() {
        clear_session_xfile_storage("xstorage-discard");
        let path = PathBuf::from("/tmp/discard.txt");
        store_written_text("xstorage-discard", &path, "first\n");
        let second = store_written_text("xstorage-discard", &path, "second\n");

        discard_head_revision("xstorage-discard", &path).unwrap();
        let head = try_get_head("xstorage-discard", &path).unwrap();

        assert_eq!(second.revision_count, 2);
        assert_eq!(head.revision_count, 1);
        assert_eq!(head.rendered_content, "first\n");
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
        assert_eq!(revisions.first().unwrap().number, 5);
        assert_eq!(revisions.last().unwrap().number, 20);
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

        assert_eq!(restored.revision_count, 2);
        assert_eq!(restored.rendered_content, second.rendered_content);
        assert_eq!(restored.file.content[0].tag, first.file.content[0].tag);
        assert_eq!(restored.file.content[2].tag, first.file.content[1].tag);

        let revisions = list_revisions(session_id, &path).unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].number, 1);
        assert_eq!(revisions[1].number, 2);
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
