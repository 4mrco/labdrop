use crate::error::{LabDropError, Result};
use crate::fs_space::query_disk_space;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

/// Kind of item selected for distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    File,
    Directory,
}

/// A user-selected source file or folder to be staged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedItem {
    pub source_path: PathBuf,
    pub canonical_path: PathBuf,
    pub original_name: String,
    pub staged_name: String,
    pub kind: ItemKind,
    pub file_count: usize,
    pub total_bytes: u64,
    pub renamed_due_to_collision: bool,
}

/// A single staged file entry inside the staging directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFileEntry {
    /// Relative path inside the staging directory (using `/` separators for cross-platform URLs).
    pub relative_path: String,
    /// Original source file path on the transmitting computer.
    pub source_path: PathBuf,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256 digest computed during staging.
    pub sha256: String,
}

/// Complete manifest of a staged distribution directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagingManifest {
    pub session_id: String,
    pub staged_dir: PathBuf,
    pub total_files: usize,
    pub total_bytes: u64,
    pub selected_items: Vec<SelectedItem>,
    pub files: Vec<StagedFileEntry>,
}

/// Real-time progress snapshot emitted during staging.
#[derive(Debug, Clone)]
pub struct StagingProgress {
    pub current_file: String,
    pub files_completed: usize,
    pub total_files: usize,
    pub bytes_copied: u64,
    pub total_bytes: u64,
}

/// Manages the list of files and directories selected by the user before staging.
#[derive(Debug, Clone, Default)]
pub struct SelectionSet {
    items: Vec<SelectedItem>,
}

impl SelectionSet {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn items(&self) -> &[SelectedItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.total_bytes).sum()
    }

    pub fn total_files(&self) -> usize {
        self.items.iter().map(|i| i.file_count).sum()
    }

    /// Checks whether a given path is already in the selection set.
    pub fn contains_path(&self, path: &Path) -> bool {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.items.iter().any(|i| i.canonical_path == canonical)
    }

    /// Toggles selection of `path`: removes if already selected, adds if not.
    /// Returns `Ok(true)` if added, `Ok(false)` if removed.
    pub fn toggle_path(&mut self, path: &Path) -> Result<bool> {
        let canonical = fs::canonicalize(path).map_err(|_| {
            LabDropError::PathNotFound(path.to_path_buf())
        })?;
        if let Some(idx) = self.items.iter().position(|i| i.canonical_path == canonical) {
            self.items.remove(idx);
            self.recompute_collision_names();
            Ok(false)
        } else {
            self.add_path(path)?;
            Ok(true)
        }
    }

    /// Adds a file or directory to the selection set.
    /// If the exact canonical path is already selected, returns `Ok(false)`.
    pub fn add_path(&mut self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Err(LabDropError::PathNotFound(path.to_path_buf()));
        }
        let canonical = fs::canonicalize(path).map_err(|e| LabDropError::IoPath {
            path: path.to_path_buf(),
            source: e,
        })?;

        if self.items.iter().any(|i| i.canonical_path == canonical) {
            return Ok(false);
        }

        let metadata = fs::metadata(&canonical).map_err(|e| LabDropError::IoPath {
            path: canonical.clone(),
            source: e,
        })?;

        let original_name = path
            .file_name()
            .or_else(|| canonical.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".to_string());

        let (kind, file_count, total_bytes) = if metadata.is_dir() {
            let mut count = 0usize;
            let mut bytes = 0u64;
            for entry in WalkDir::new(&canonical).follow_links(false) {
                let entry = entry.map_err(|e| {
                    LabDropError::Io(
                        e.into_io_error()
                            .unwrap_or_else(|| std::io::Error::other("walkdir error")),
                    )
                })?;
                if entry.file_type().is_file() {
                    count += 1;
                    bytes = bytes.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
                }
            }
            (ItemKind::Directory, count, bytes)
        } else {
            (ItemKind::File, 1, metadata.len())
        };

        self.items.push(SelectedItem {
            source_path: path.to_path_buf(),
            canonical_path: canonical,
            original_name: original_name.clone(),
            staged_name: original_name,
            kind,
            file_count,
            total_bytes,
            renamed_due_to_collision: false,
        });

        self.recompute_collision_names();
        Ok(true)
    }

    /// Removes the item at `index`.
    pub fn remove_index(&mut self, index: usize) -> Option<SelectedItem> {
        if index < self.items.len() {
            let removed = self.items.remove(index);
            self.recompute_collision_names();
            Some(removed)
        } else {
            None
        }
    }

    /// Clears all selected items.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Recomputes collision-free `staged_name` values across all selected items.
    fn recompute_collision_names(&mut self) {
        let mut used_lower: HashSet<String> = HashSet::new();
        for item in &mut self.items {
            let base_candidate = sanitize_top_level_name(&item.original_name);
            if used_lower.insert(base_candidate.to_lowercase()) {
                item.staged_name = base_candidate;
                item.renamed_due_to_collision = false;
            } else {
                let mut counter = 2usize;
                loop {
                    let candidate = disambiguate_name(&base_candidate, counter, item.kind);
                    if used_lower.insert(candidate.to_lowercase()) {
                        item.staged_name = candidate;
                        item.renamed_due_to_collision = true;
                        break;
                    }
                    counter += 1;
                }
            }
        }
    }
}

fn sanitize_top_level_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "item".to_string()
    } else {
        trimmed.replace(['/', '\\', '\0'], "_")
    }
}

/// Inserts ` (N)` before the file extension (for files) or at the end (for directories).
pub fn disambiguate_name(name: &str, index: usize, kind: ItemKind) -> String {
    if kind == ItemKind::Directory {
        return format!("{name} ({index})");
    }
    if let Some((stem, ext)) = split_stem_and_ext(name) {
        format!("{stem} ({index}).{ext}")
    } else {
        format!("{name} ({index})")
    }
}

fn split_stem_and_ext(name: &str) -> Option<(&str, &str)> {
    // Handle compound archive extensions like .tar.gz, .tar.xz, .tar.bz2
    for compound in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if let Some(stem) = name.strip_suffix(compound) {
            if !stem.is_empty() {
                return Some((stem, &compound[1..]));
            }
        }
    }
    let dot_pos = name.rfind('.')?;
    if dot_pos == 0 || dot_pos + 1 >= name.len() {
        return None;
    }
    Some((&name[..dot_pos], &name[dot_pos + 1..]))
}

/// Returns the default base directory on internal storage for temporary staging sessions.
pub fn default_staging_base_dir() -> PathBuf {
    #[cfg(unix)]
    {
        let var_tmp = Path::new("/var/tmp");
        if var_tmp.is_dir() {
            return var_tmp.join("labdrop-sessions");
        }
    }
    std::env::temp_dir().join("labdrop-sessions")
}

/// Stages all items in `selection` into `session_staged_dir` with disk space verification,
/// progress reporting, SHA-256 calculation, and atomic rollback on failure or cancellation.
pub fn stage_selection<F>(
    session_id: &str,
    selection: &SelectionSet,
    session_staged_dir: &Path,
    available_space_override: Option<u64>,
    cancel_flag: Option<&AtomicBool>,
    mut on_progress: F,
) -> Result<StagingManifest>
where
    F: FnMut(&StagingProgress),
{
    if selection.is_empty() {
        return Err(LabDropError::EmptySelection);
    }

    let required_bytes = selection.total_bytes();
    // Require at least 8 MiB safety margin for metadata/manifest/logs when non-empty
    let required_with_margin = required_bytes.saturating_add(8 * 1024 * 1024);

    let available_bytes = match available_space_override {
        Some(avail) => avail,
        None => query_disk_space(session_staged_dir)?.available_bytes,
    };

    if available_bytes < required_with_margin {
        return Err(LabDropError::InsufficientDiskSpace {
            staging_dir: session_staged_dir.to_path_buf(),
            required_bytes,
            available_bytes,
        });
    }

    // Collect the flat list of (source_file, relative_target_path, file_size)
    let mut copy_tasks: Vec<(PathBuf, PathBuf, String, u64)> = Vec::new();
    for item in selection.items() {
        if !item.canonical_path.exists() {
            return Err(LabDropError::PathNotFound(item.source_path.clone()));
        }
        match item.kind {
            ItemKind::File => {
                let rel_path = PathBuf::from(&item.staged_name);
                let rel_str = item.staged_name.clone();
                let len = fs::metadata(&item.canonical_path)
                    .map_err(|e| LabDropError::IoPath {
                        path: item.canonical_path.clone(),
                        source: e,
                    })?
                    .len();
                copy_tasks.push((item.canonical_path.clone(), rel_path, rel_str, len));
            }
            ItemKind::Directory => {
                for entry in WalkDir::new(&item.canonical_path).follow_links(false) {
                    let entry = entry.map_err(|e| {
                        let path = e
                            .path()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| item.canonical_path.clone());
                        let source = e
                            .into_io_error()
                            .unwrap_or_else(|| std::io::Error::other("directory traversal failed"));
                        LabDropError::IoPath { path, source }
                    })?;
                    let sub_rel = entry
                        .path()
                        .strip_prefix(&item.canonical_path)
                        .unwrap_or_else(|_| Path::new(""));
                    if entry.file_type().is_dir() {
                        // Ensure empty directories inside selected folders are also preserved!
                        let target_dir = if sub_rel.as_os_str().is_empty() {
                            session_staged_dir.join(&item.staged_name)
                        } else {
                            session_staged_dir.join(&item.staged_name).join(sub_rel)
                        };
                        if let Err(e) = create_dir_world_readable(&target_dir) {
                            let _ = fs::remove_dir_all(session_staged_dir);
                            return Err(LabDropError::IoPath {
                                path: target_dir,
                                source: e,
                            });
                        }
                    } else if entry.file_type().is_file() {
                        let rel_path = Path::new(&item.staged_name).join(sub_rel);
                        let rel_str = path_to_forward_slashes(&rel_path);
                        let len = entry
                            .metadata()
                            .map_err(|e| LabDropError::IoPath {
                                path: entry.path().to_path_buf(),
                                source: e
                                    .into_io_error()
                                    .unwrap_or_else(|| std::io::Error::other("metadata error")),
                            })?
                            .len();
                        copy_tasks.push((entry.path().to_path_buf(), rel_path, rel_str, len));
                    }
                }
            }
        }
    }

    if let Err(e) = create_dir_world_readable(session_staged_dir) {
        return Err(LabDropError::IoPath {
            path: session_staged_dir.to_path_buf(),
            source: e,
        });
    }

    let total_files = copy_tasks.len();
    let total_bytes: u64 = copy_tasks.iter().map(|(_, _, _, len)| *len).sum();
    let mut files_completed = 0usize;
    let mut bytes_copied = 0u64;
    let mut staged_entries = Vec::with_capacity(total_files);

    let copy_result: Result<()> = (|| {
        let mut buf = vec![0u8; 64 * 1024];

        for (src_path, rel_path, rel_str, expected_size) in copy_tasks {
            if let Some(flag) = cancel_flag {
                if flag.load(Ordering::Relaxed) {
                    return Err(LabDropError::StagingCancelled);
                }
            }

            on_progress(&StagingProgress {
                current_file: rel_str.clone(),
                files_completed,
                total_files,
                bytes_copied,
                total_bytes,
            });

            let dest_path = session_staged_dir.join(&rel_path);
            if let Some(parent) = dest_path.parent() {
                create_dir_world_readable(parent).map_err(|e| LabDropError::IoPath {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }

            let mut src_file = File::open(&src_path).map_err(|e| LabDropError::IoPath {
                path: src_path.clone(),
                source: e,
            })?;
            let mut dest_file = File::create(&dest_path).map_err(|e| LabDropError::IoPath {
                path: dest_path.clone(),
                source: e,
            })?;

            let mut hasher = Sha256::new();
            let mut file_bytes = 0u64;

            loop {
                if let Some(flag) = cancel_flag {
                    if flag.load(Ordering::Relaxed) {
                        return Err(LabDropError::StagingCancelled);
                    }
                }

                let n = src_file.read(&mut buf).map_err(|e| LabDropError::IoPath {
                    path: src_path.clone(),
                    source: e,
                })?;
                if n == 0 {
                    break;
                }
                dest_file
                    .write_all(&buf[..n])
                    .map_err(|e| LabDropError::IoPath {
                        path: dest_path.clone(),
                        source: e,
                    })?;
                hasher.update(&buf[..n]);
                file_bytes += n as u64;
                bytes_copied += n as u64;

                on_progress(&StagingProgress {
                    current_file: rel_str.clone(),
                    files_completed,
                    total_files,
                    bytes_copied,
                    total_bytes,
                });
            }

            dest_file.flush().map_err(|e| LabDropError::IoPath {
                path: dest_path.clone(),
                source: e,
            })?;
            set_file_world_readable(&dest_path);

            let sha256 = hex_encode(&hasher.finalize());
            staged_entries.push(StagedFileEntry {
                relative_path: rel_str.clone(),
                source_path: src_path,
                size_bytes: file_bytes.max(expected_size),
                sha256,
            });

            files_completed += 1;
            on_progress(&StagingProgress {
                current_file: rel_str,
                files_completed,
                total_files,
                bytes_copied,
                total_bytes,
            });
        }
        Ok(())
    })();

    if let Err(err) = copy_result {
        // Atomic rollback: never leave incomplete staging data after a failed or cancelled operation
        let _ = fs::remove_dir_all(session_staged_dir);
        return Err(err);
    }

    let manifest = StagingManifest {
        session_id: session_id.to_string(),
        staged_dir: session_staged_dir.to_path_buf(),
        total_files,
        total_bytes: bytes_copied,
        selected_items: selection.items().to_vec(),
        files: staged_entries,
    };

    Ok(manifest)
}

pub fn create_dir_world_readable(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Ensure parent chain under labdrop-sessions is also 0755 so guest smbd (nobody) can traverse
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        if let Some(parent) = dir.parent() {
            if parent.to_string_lossy().contains("labdrop") {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
                if let Some(grandparent) = parent.parent() {
                    if grandparent.to_string_lossy().contains("labdrop") {
                        let _ =
                            fs::set_permissions(grandparent, fs::Permissions::from_mode(0o755));
                    }
                }
            }
        }
    }
    Ok(())
}

fn set_file_world_readable(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(file, fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

pub fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Parses one or more file/folder paths pasted or dragged-and-dropped into a terminal.
/// Supports quoted paths (`'...'`, `"..."`), `file://` URIs, shell backslash-escaped spaces,
/// multi-line drops, and raw unquoted paths containing spaces.
pub fn parse_dropped_paths(raw: &str) -> Vec<PathBuf> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut results = Vec::new();

    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // If the entire trimmed line exists directly on disk as a single path (e.g. unquoted Windows/Linux path with spaces)
        if !trimmed.starts_with("file://")
            && !trimmed.starts_with('\'')
            && !trimmed.starts_with('"')
            && Path::new(trimmed).exists()
        {
            results.push(PathBuf::from(trimmed));
            continue;
        }

        // Tokenize line respecting single quotes, double quotes, and Unix shell backslash escapes
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_single = false;
        let mut in_double = false;
        let mut chars = trimmed.chars().peekable();

        let looks_like_windows_drive = trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && (trimmed.as_bytes()[2] == b'\\' || trimmed.as_bytes()[2] == b'/');

        while let Some(c) = chars.next() {
            if in_single {
                if c == '\'' {
                    in_single = false;
                } else {
                    current.push(c);
                }
            } else if in_double {
                if c == '"' {
                    in_double = false;
                } else {
                    current.push(c);
                }
            } else {
                match c {
                    '\'' => in_single = true,
                    '"' => in_double = true,
                    '\\' if !looks_like_windows_drive && !current.starts_with("\\\\") => {
                        if let Some(next_c) = chars.next() {
                            current.push(next_c);
                        }
                    }
                    c if c.is_whitespace() => {
                        if !current.is_empty() {
                            tokens.push(std::mem::take(&mut current));
                        }
                    }
                    _ => current.push(c),
                }
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }

        for tok in tokens {
            if let Some(uri_path) = decode_file_uri(&tok) {
                results.push(uri_path);
            } else {
                results.push(PathBuf::from(tok));
            }
        }
    }

    results
}

fn decode_file_uri(s: &str) -> Option<PathBuf> {
    let rest = s.strip_prefix("file://")?;
    // Strip optional `localhost` host prefix
    let path_part = if let Some(after) = rest.strip_prefix("localhost/") {
        format!("/{after}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        return None;
    };

    let decoded = crate::http_webdav::percent_decode(&path_part)?;
    // On Windows, `file:///C:/Users/...` -> `C:/Users/...`
    #[cfg(windows)]
    {
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
            return Some(PathBuf::from(&decoded[1..]));
        }
    }
    Some(PathBuf::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn duplicate_filenames_from_different_folders_should_be_disambiguated() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("dir_a");
        let dir_b = tmp.path().join("dir_b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let file_a = dir_a.join("installer.exe");
        let file_b = dir_b.join("installer.exe");
        fs::write(&file_a, b"version A").unwrap();
        fs::write(&file_b, b"version B Longer").unwrap();

        let mut sel = SelectionSet::new();
        assert!(sel.add_path(&file_a).unwrap());
        assert!(sel.add_path(&file_b).unwrap());
        // Adding exact same path again is deduplicated
        assert!(!sel.add_path(&file_a).unwrap());

        assert_eq!(sel.items().len(), 2);
        assert_eq!(sel.items()[0].staged_name, "installer.exe");
        assert!(!sel.items()[0].renamed_due_to_collision);
        assert_eq!(sel.items()[1].staged_name, "installer (2).exe");
        assert!(sel.items()[1].renamed_due_to_collision);

        let staged_dir = tmp.path().join("staged");
        let manifest =
            stage_selection("test-session", &sel, &staged_dir, None, None, |_| {}).unwrap();
        assert_eq!(manifest.total_files, 2);
        assert_eq!(
            fs::read(staged_dir.join("installer.exe")).unwrap(),
            b"version A"
        );
        assert_eq!(
            fs::read(staged_dir.join("installer (2).exe")).unwrap(),
            b"version B Longer"
        );
        // Verify originals are untouched
        assert_eq!(fs::read(&file_a).unwrap(), b"version A");
        assert_eq!(fs::read(&file_b).unwrap(), b"version B Longer");
    }

    #[test]
    fn unicode_spaces_and_nested_directory_structure_should_be_preserved() {
        let tmp = TempDir::new().unwrap();
        let src_folder = tmp.path().join("Material do Laboratório — 2026 🧪");
        let nested = src_folder.join("Subpasta Com Espaços").join("日本語フォルダ");
        fs::create_dir_all(&nested).unwrap();

        let long_name = format!("{}.txt", "a_very_long_filename_segment_".repeat(6));
        let file_path = nested.join(&long_name);
        fs::write(&file_path, b"unicode and long name payload").unwrap();

        let mut sel = SelectionSet::new();
        sel.add_path(&src_folder).unwrap();

        let staged_dir = tmp.path().join("staged");
        let manifest =
            stage_selection("unicode-session", &sel, &staged_dir, None, None, |_| {}).unwrap();
        assert_eq!(manifest.total_files, 1);

        let expected_staged = staged_dir
            .join("Material do Laboratório — 2026 🧪")
            .join("Subpasta Com Espaços")
            .join("日本語フォルダ")
            .join(&long_name);
        assert!(expected_staged.exists());
        assert_eq!(
            fs::read(&expected_staged).unwrap(),
            b"unicode and long name payload"
        );
    }

    #[test]
    fn insufficient_disk_space_should_abort_before_staging() {
        let tmp = TempDir::new().unwrap();
        let file_p = tmp.path().join("payload.bin");
        fs::write(&file_p, vec![0xABu8; 4096]).unwrap();

        let mut sel = SelectionSet::new();
        sel.add_path(&file_p).unwrap();

        let staged_dir = tmp.path().join("staged_no_space");
        let res = stage_selection("low-space", &sel, &staged_dir, Some(1024), None, |_| {});
        assert!(matches!(res, Err(LabDropError::InsufficientDiskSpace { .. })));
        assert!(!staged_dir.exists());
    }

    #[test]
    fn cancelled_staging_should_rollback_and_leave_no_incomplete_directory() {
        let tmp = TempDir::new().unwrap();
        let file_p = tmp.path().join("large.bin");
        fs::write(&file_p, vec![0x55u8; 256 * 1024]).unwrap();

        let mut sel = SelectionSet::new();
        sel.add_path(&file_p).unwrap();

        let staged_dir = tmp.path().join("staged_cancelled");
        let cancel = AtomicBool::new(false);
        let res = stage_selection(
            "cancel-session",
            &sel,
            &staged_dir,
            None,
            Some(&cancel),
            |prog| {
                if prog.bytes_copied > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(matches!(res, Err(LabDropError::StagingCancelled)));
        assert!(!staged_dir.exists());
    }

    #[test]
    fn parse_dropped_paths_should_handle_quotes_escapes_uris_and_multiline() {
        let tmp = TempDir::new().unwrap();
        let spaced = tmp.path().join("My Installer Folder");
        fs::create_dir_all(&spaced).unwrap();

        let input = format!(
            "'/tmp/foo bar.txt' \"/tmp/baz qux.exe\"\n/tmp/escaped\\ name.bin\nfile:///tmp/hello%20world.pdf\n{}",
            spaced.display()
        );
        let parsed = parse_dropped_paths(&input);
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("/tmp/foo bar.txt"),
                PathBuf::from("/tmp/baz qux.exe"),
                PathBuf::from("/tmp/escaped name.bin"),
                PathBuf::from("/tmp/hello world.pdf"),
                spaced,
            ]
        );
    }
}
