use crate::copy_bench::format_bytes;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

pub const TINY_FILE_MAX_BYTES: u64 = 256 * KIB;
pub const LARGE_FILE_MIN_BYTES: u64 = 64 * MIB;
pub const MAX_WORKERS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileClass {
    Tiny,
    Medium,
    Large,
}

#[derive(Debug)]
pub struct ManifestEntry {
    pub relative_path: PathBuf,
    pub file_size: u64,
    pub last_write_time: u64,
    pub file_attributes: u32,
    pub class: FileClass,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub relative_path: PathBuf,
}

#[derive(Debug)]
pub struct ScanResult {
    pub manifest: Vec<ManifestEntry>,
    pub directories: Vec<DirectoryEntry>,
    pub report: ScanReport,
}

#[derive(Debug)]
pub struct ScanReport {
    pub worker_count: usize,
    pub files_scanned: u64,
    pub directories_scanned: u64,
    pub reparse_points_skipped: u64,
    pub total_bytes: u64,
    pub tiny_files: u64,
    pub tiny_bytes: u64,
    pub medium_files: u64,
    pub medium_bytes: u64,
    pub large_files: u64,
    pub large_bytes: u64,
    pub sparse_files: u64,
    pub compressed_files: u64,
    pub relative_path_utf16_units: u64,
    pub elapsed: Duration,
}

impl ScanReport {
    pub fn print(&self) {
        let entries = self.files_scanned.saturating_add(self.directories_scanned);

        let seconds = self.elapsed.as_secs_f64();

        let entries_per_second = if seconds == 0.0 {
            0.0
        } else {
            entries as f64 / seconds
        };

        println!("Parallel manifest scan complete");
        println!("  Workers:             {}", self.worker_count);
        println!(
            "  Files:               {}",
            format_bytes(self.files_scanned)
        );
        println!(
            "  Directories:         {}",
            format_bytes(self.directories_scanned)
        );
        println!(
            "  Reparse skipped:     {}",
            format_bytes(self.reparse_points_skipped)
        );
        println!(
            "  Total file data:     {} bytes",
            format_bytes(self.total_bytes)
        );
        println!();
        println!(
            "  Tiny files:          {} / {} bytes",
            format_bytes(self.tiny_files),
            format_bytes(self.tiny_bytes)
        );
        println!(
            "  Medium files:        {} / {} bytes",
            format_bytes(self.medium_files),
            format_bytes(self.medium_bytes)
        );
        println!(
            "  Large files:         {} / {} bytes",
            format_bytes(self.large_files),
            format_bytes(self.large_bytes)
        );
        println!();
        println!("  Sparse files:        {}", format_bytes(self.sparse_files));
        println!(
            "  Compressed files:    {}",
            format_bytes(self.compressed_files)
        );
        println!(
            "  Path UTF-16 units:   {}",
            format_bytes(self.relative_path_utf16_units)
        );
        println!("  Scan time:           {:.6} s", seconds);
        println!("  Enumeration rate:    {:.0} entries/s", entries_per_second);
    }
}

#[derive(Debug)]
struct WorkerResult {
    manifest: Vec<ManifestEntry>,
    directories: Vec<DirectoryEntry>,
    directories_scanned: u64,
    reparse_points_skipped: u64,
}

#[derive(Debug)]
struct DirectoryScan {
    manifest: Vec<ManifestEntry>,
    directories: Vec<DirectoryEntry>,
    discovered_directories: Vec<PathBuf>,
    reparse_points_skipped: u64,
}

#[derive(Debug)]
struct DirectoryQueue {
    state: Mutex<DirectoryQueueState>,
    ready: Condvar,
}

#[derive(Debug)]
struct DirectoryQueueState {
    pending: VecDeque<PathBuf>,
    active_workers: usize,
    cancelled: bool,
}

impl DirectoryQueue {
    fn new(root: PathBuf) -> Self {
        let mut pending = VecDeque::new();
        pending.push_back(root);

        Self {
            state: Mutex::new(DirectoryQueueState {
                pending,
                active_workers: 0,
                cancelled: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn take_directory(&self) -> io::Result<Option<PathBuf>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("directory queue lock poisoned"))?;

        loop {
            if state.cancelled {
                return Ok(None);
            }

            if let Some(directory) = state.pending.pop_front() {
                state.active_workers = state
                    .active_workers
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("active directory worker count overflowed"))?;

                return Ok(Some(directory));
            }

            if state.active_workers == 0 {
                return Ok(None);
            }

            state = self
                .ready
                .wait(state)
                .map_err(|_| io::Error::other("directory queue lock poisoned while waiting"))?;
        }
    }

    fn complete_directory(&self, discovered_directories: Vec<PathBuf>) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("directory queue lock poisoned"))?;

        state.active_workers = state
            .active_workers
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("directory worker completed without active work"))?;

        if !state.cancelled {
            state.pending.extend(discovered_directories);
        }

        self.ready.notify_all();
        Ok(())
    }

    fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.cancelled = true;
        }

        self.ready.notify_all();
    }
}

pub fn default_worker_count() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get().min(16))
        .unwrap_or(4)
}

pub fn validate_worker_count(worker_count: usize) -> io::Result<()> {
    if !(1..=MAX_WORKERS).contains(&worker_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("worker count must be between 1 and {MAX_WORKERS}"),
        ));
    }

    Ok(())
}

pub fn run(root: &Path, worker_count: usize) -> io::Result<ScanResult> {
    validate_worker_count(worker_count)?;

    let started = Instant::now();
    let root = root.canonicalize()?;
    let root_metadata = fs::metadata(&root)?;

    if !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("scan root is not a directory: {}", root.display()),
        ));
    }

    if root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("scan root must not be a reparse point: {}", root.display()),
        ));
    }

    let root = Arc::new(root);
    let queue = Arc::new(DirectoryQueue::new(root.as_ref().clone()));

    let worker_results = thread::scope(|scope| -> io::Result<Vec<WorkerResult>> {
        let mut handles = Vec::with_capacity(worker_count);

        for worker_index in 0..worker_count {
            let worker_root = Arc::clone(&root);
            let worker_queue = Arc::clone(&queue);

            let handle = thread::Builder::new()
                .name(format!("networkcopy-scanner-{worker_index}"))
                .spawn_scoped(scope, move || {
                    scan_worker(worker_root.as_path(), worker_queue)
                })?;

            handles.push(handle);
        }

        let mut results = Vec::with_capacity(worker_count);
        let mut first_error: Option<io::Error> = None;

        for handle in handles {
            match handle.join() {
                Ok(Ok(result)) => results.push(result),

                Ok(Err(error)) => {
                    queue.cancel();

                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }

                Err(_) => {
                    queue.cancel();

                    if first_error.is_none() {
                        first_error = Some(io::Error::other("manifest scanner worker panicked"));
                    }
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }

        Ok(results)
    })?;

    let mut manifest = Vec::new();
    let mut directories = Vec::new();
    let mut directories_scanned = 0_u64;
    let mut reparse_points_skipped = 0_u64;

    for worker_result in worker_results {
        directories_scanned = directories_scanned
            .checked_add(worker_result.directories_scanned)
            .ok_or_else(|| io::Error::other("directory count overflowed while merging workers"))?;

        reparse_points_skipped = reparse_points_skipped
            .checked_add(worker_result.reparse_points_skipped)
            .ok_or_else(|| {
                io::Error::other("reparse-point count overflowed while merging workers")
            })?;

        manifest.extend(worker_result.manifest);
        directories.extend(worker_result.directories);
    }

    manifest.sort_unstable_by(|left, right| {
        left.relative_path
            .as_os_str()
            .encode_wide()
            .cmp(right.relative_path.as_os_str().encode_wide())
    });

    directories.sort_unstable_by(|left, right| {
        left.relative_path
            .as_os_str()
            .encode_wide()
            .cmp(right.relative_path.as_os_str().encode_wide())
    });

    let report = build_report(
        &manifest,
        worker_count,
        directories_scanned,
        reparse_points_skipped,
        started.elapsed(),
    )?;

    let result = ScanResult {
        manifest,
        directories,
        report,
    };

    validate_tree_entries(&result.manifest, &result.directories)?;

    Ok(result)
}

fn scan_worker(root: &Path, queue: Arc<DirectoryQueue>) -> io::Result<WorkerResult> {
    let mut manifest = Vec::new();
    let mut directories = Vec::new();
    let mut directories_scanned = 0_u64;
    let mut reparse_points_skipped = 0_u64;

    while let Some(directory) = queue.take_directory()? {
        let directory_scan = match scan_directory(root, &directory) {
            Ok(result) => result,

            Err(error) => {
                queue.cancel();

                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to scan {}: {error}", directory.display()),
                ));
            }
        };

        directories_scanned = directories_scanned
            .checked_add(1)
            .ok_or_else(|| io::Error::other("directory count overflowed in scanner worker"))?;

        reparse_points_skipped = reparse_points_skipped
            .checked_add(directory_scan.reparse_points_skipped)
            .ok_or_else(|| io::Error::other("reparse-point count overflowed in scanner worker"))?;

        manifest.extend(directory_scan.manifest);
        directories.extend(directory_scan.directories);

        queue.complete_directory(directory_scan.discovered_directories)?;
    }

    Ok(WorkerResult {
        manifest,
        directories,
        directories_scanned,
        reparse_points_skipped,
    })
}

fn scan_directory(root: &Path, directory: &Path) -> io::Result<DirectoryScan> {
    let mut manifest = Vec::new();
    let mut directories = Vec::new();
    let mut discovered_directories = Vec::new();
    let mut reparse_points_skipped = 0_u64;

    for entry_result in fs::read_dir(directory)? {
        let entry = entry_result?;
        let metadata = entry.metadata()?;
        let attributes = metadata.file_attributes();

        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            reparse_points_skipped = reparse_points_skipped
                .checked_add(1)
                .ok_or_else(|| io::Error::other("reparse-point count overflowed"))?;

            continue;
        }

        let path = entry.path();

        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            let relative_path = relative_path_from_root(root, &path)?;

            directories.push(DirectoryEntry { relative_path });
            discovered_directories.push(path);
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        let relative_path = relative_path_from_root(root, &path)?;

        let file_size = metadata.len();

        manifest.push(ManifestEntry {
            relative_path,
            file_size,
            last_write_time: metadata.last_write_time(),
            file_attributes: attributes,
            class: classify_file(file_size),
        });
    }

    Ok(DirectoryScan {
        manifest,
        directories,
        discovered_directories,
        reparse_points_skipped,
    })
}

fn relative_path_from_root(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let relative_path = path
        .strip_prefix(root)
        .map_err(|_| io::Error::other(format!("entry escaped scan root: {}", path.display())))?
        .to_path_buf();

    validate_relative_path(&relative_path)?;

    Ok(relative_path)
}

pub(crate) fn validate_relative_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("path is not a safe relative path: {}", path.display()),
        ));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "relative path contains an unsafe component: {}",
                    path.display()
                ),
            ));
        }
    }

    Ok(())
}

pub(crate) fn validate_tree_entries(
    manifest: &[ManifestEntry],
    directories: &[DirectoryEntry],
) -> io::Result<()> {
    let mut file_paths: HashSet<&Path> = HashSet::with_capacity(manifest.len());

    for entry in manifest {
        validate_relative_path(&entry.relative_path)?;

        if !file_paths.insert(entry.relative_path.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "duplicate file path in manifest: {}",
                    entry.relative_path.display()
                ),
            ));
        }
    }

    let mut directory_paths: HashSet<&Path> = HashSet::with_capacity(directories.len());

    for entry in directories {
        validate_relative_path(&entry.relative_path)?;

        let directory_path = entry.relative_path.as_path();

        if !directory_paths.insert(directory_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "duplicate directory path in manifest: {}",
                    directory_path.display()
                ),
            ));
        }

        if let Some(file_path) = directory_path
            .ancestors()
            .find(|ancestor| !ancestor.as_os_str().is_empty() && file_paths.contains(ancestor))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "manifest directory {} collides with file {}",
                    directory_path.display(),
                    file_path.display()
                ),
            ));
        }
    }

    Ok(())
}

fn classify_file(file_size: u64) -> FileClass {
    if file_size <= TINY_FILE_MAX_BYTES {
        FileClass::Tiny
    } else if file_size >= LARGE_FILE_MIN_BYTES {
        FileClass::Large
    } else {
        FileClass::Medium
    }
}

fn build_report(
    manifest: &[ManifestEntry],
    worker_count: usize,
    directories_scanned: u64,
    reparse_points_skipped: u64,
    elapsed: Duration,
) -> io::Result<ScanReport> {
    let mut report = ScanReport {
        worker_count,
        files_scanned: manifest.len() as u64,
        directories_scanned,
        reparse_points_skipped,
        total_bytes: 0,
        tiny_files: 0,
        tiny_bytes: 0,
        medium_files: 0,
        medium_bytes: 0,
        large_files: 0,
        large_bytes: 0,
        sparse_files: 0,
        compressed_files: 0,
        relative_path_utf16_units: 0,
        elapsed,
    };

    for entry in manifest {
        report.total_bytes = report
            .total_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("total manifest byte count overflowed"))?;

        let path_units = entry.relative_path.as_os_str().encode_wide().count() as u64;

        report.relative_path_utf16_units = report
            .relative_path_utf16_units
            .checked_add(path_units)
            .ok_or_else(|| io::Error::other("manifest path-size count overflowed"))?;

        if entry.file_attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            report.sparse_files += 1;
        }

        if entry.file_attributes & FILE_ATTRIBUTE_COMPRESSED != 0 {
            report.compressed_files += 1;
        }

        match entry.class {
            FileClass::Tiny => {
                report.tiny_files += 1;
                report.tiny_bytes = report
                    .tiny_bytes
                    .checked_add(entry.file_size)
                    .ok_or_else(|| io::Error::other("tiny-file byte count overflowed"))?;
            }

            FileClass::Medium => {
                report.medium_files += 1;
                report.medium_bytes = report
                    .medium_bytes
                    .checked_add(entry.file_size)
                    .ok_or_else(|| io::Error::other("medium-file byte count overflowed"))?;
            }

            FileClass::Large => {
                report.large_files += 1;
                report.large_bytes = report
                    .large_bytes
                    .checked_add(entry.file_size)
                    .ok_or_else(|| io::Error::other("large-file byte count overflowed"))?;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{
        DirectoryEntry, FileClass, LARGE_FILE_MIN_BYTES, ManifestEntry, TINY_FILE_MAX_BYTES,
        classify_file, run, validate_relative_path, validate_tree_entries, validate_worker_count,
    };
    use std::collections::HashSet;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn manifest_entry(relative_path: PathBuf) -> ManifestEntry {
        ManifestEntry {
            relative_path,
            file_size: 0,
            last_write_time: 0,
            file_attributes: 0,
            class: FileClass::Tiny,
        }
    }

    #[test]
    fn classifies_file_size_boundaries() {
        assert_eq!(classify_file(0), FileClass::Tiny);
        assert_eq!(classify_file(TINY_FILE_MAX_BYTES), FileClass::Tiny);
        assert_eq!(classify_file(TINY_FILE_MAX_BYTES + 1), FileClass::Medium);
        assert_eq!(classify_file(LARGE_FILE_MIN_BYTES - 1), FileClass::Medium);
        assert_eq!(classify_file(LARGE_FILE_MIN_BYTES), FileClass::Large);
    }

    #[test]
    fn rejects_invalid_worker_counts() {
        assert!(validate_worker_count(0).is_err());
        assert!(validate_worker_count(1).is_ok());
        assert!(validate_worker_count(64).is_ok());
        assert!(validate_worker_count(65).is_err());
    }

    #[test]
    fn builds_recursive_relative_manifest() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let root = env::temp_dir().join(format!("networkcopy-manifest-{}-{unique}", process::id()));

        let nested = root.join("alpha").join("beta");

        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("root.txt"), b"root").unwrap();
        fs::write(nested.join("nested.bin"), vec![7_u8; 300 * 1024]).unwrap();

        let scan_result = run(&root, 4);
        let cleanup_result = fs::remove_dir_all(&root);

        let result = scan_result.unwrap();
        cleanup_result.unwrap();

        let paths: HashSet<PathBuf> = result
            .manifest
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect();

        assert_eq!(result.report.files_scanned, 2);
        assert_eq!(result.report.directories_scanned, 3);
        assert_eq!(result.report.tiny_files, 1);
        assert_eq!(result.report.medium_files, 1);
        assert_eq!(result.report.large_files, 0);

        assert!(paths.contains(&PathBuf::from("root.txt")));
        assert!(paths.contains(&PathBuf::from("alpha").join("beta").join("nested.bin")));
    }

    #[test]
    fn manifest_entries_have_deterministic_order() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let root = env::temp_dir().join(format!(
            "networkcopy-manifest-order-{}-{unique}",
            process::id()
        ));

        fs::create_dir_all(&root).unwrap();

        fs::write(root.join("zeta.txt"), b"zeta").unwrap();

        fs::write(root.join("árvíz.txt"), b"unicode").unwrap();

        fs::write(root.join("alpha.txt"), b"alpha").unwrap();

        let scan_result = run(&root, 4);
        let cleanup_result = fs::remove_dir_all(&root);

        let result = scan_result.unwrap();
        cleanup_result.unwrap();

        let paths: Vec<PathBuf> = result
            .manifest
            .into_iter()
            .map(|entry| entry.relative_path)
            .collect();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("alpha.txt"),
                PathBuf::from("zeta.txt"),
                PathBuf::from("árvíz.txt"),
            ]
        );
    }

    #[test]
    fn captures_empty_directories_in_deterministic_order() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let root = env::temp_dir().join(format!(
            "networkcopy-directory-manifest-{}-{unique}",
            process::id()
        ));

        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::create_dir_all(root.join("alpha").join("beta")).unwrap();
        fs::create_dir_all(root.join("árvíztűrő").join("üres")).unwrap();

        fs::write(root.join("alpha").join("payload.txt"), b"payload").unwrap();

        let scan_result = run(&root, 4);
        let cleanup_result = fs::remove_dir_all(&root);

        let result = scan_result.unwrap();
        cleanup_result.unwrap();

        assert_eq!(result.report.files_scanned, 1);
        assert_eq!(result.report.directories_scanned, 6);

        let directory_paths: Vec<PathBuf> = result
            .directories
            .into_iter()
            .map(|entry| entry.relative_path)
            .collect();

        assert_eq!(
            directory_paths,
            vec![
                PathBuf::from("alpha"),
                PathBuf::from("alpha").join("beta"),
                PathBuf::from("zeta"),
                PathBuf::from("árvíztűrő"),
                PathBuf::from("árvíztűrő").join("üres"),
            ]
        );
    }

    #[test]
    fn accepts_unique_file_and_directory_entries() {
        let manifest = vec![
            manifest_entry(PathBuf::from("root.txt")),
            manifest_entry(PathBuf::from("alpha").join("payload.bin")),
        ];

        let directories = vec![
            DirectoryEntry {
                relative_path: PathBuf::from("alpha"),
            },
            DirectoryEntry {
                relative_path: PathBuf::from("empty"),
            },
            DirectoryEntry {
                relative_path: PathBuf::from("nested").join("empty"),
            },
        ];

        validate_tree_entries(&manifest, &directories).unwrap();
    }

    #[test]
    fn rejects_duplicate_file_entries() {
        let manifest = vec![
            manifest_entry(PathBuf::from("duplicate.txt")),
            manifest_entry(PathBuf::from("duplicate.txt")),
        ];

        assert!(validate_tree_entries(&manifest, &[]).is_err());
    }

    #[test]
    fn rejects_duplicate_directory_entries() {
        let directories = vec![
            DirectoryEntry {
                relative_path: PathBuf::from("duplicate"),
            },
            DirectoryEntry {
                relative_path: PathBuf::from("duplicate"),
            },
        ];

        assert!(validate_tree_entries(&[], &directories).is_err());
    }

    #[test]
    fn rejects_file_directory_path_collision() {
        let manifest = vec![manifest_entry(PathBuf::from("collision"))];

        let directories = vec![DirectoryEntry {
            relative_path: PathBuf::from("collision"),
        }];

        assert!(validate_tree_entries(&manifest, &directories).is_err());
    }

    #[test]
    fn rejects_directory_nested_beneath_file_path() {
        let manifest = vec![manifest_entry(PathBuf::from("blocked"))];

        let directories = vec![DirectoryEntry {
            relative_path: PathBuf::from("blocked").join("nested"),
        }];

        assert!(validate_tree_entries(&manifest, &directories).is_err());
    }

    #[test]
    fn rejects_unsafe_relative_paths() {
        assert!(validate_relative_path(Path::new("alpha")).is_ok());
        assert!(validate_relative_path(Path::new(r"alpha\beta")).is_ok());
        assert!(validate_relative_path(Path::new(r"árvíztűrő\üres")).is_ok());

        for unsafe_path in [
            Path::new(""),
            Path::new(r"C:\absolute"),
            Path::new(r"C:drive-relative"),
            Path::new(r"\rooted"),
            Path::new(r"..\escape"),
            Path::new(r"alpha\..\escape"),
        ] {
            assert!(
                validate_relative_path(unsafe_path).is_err(),
                "unsafe path was accepted: {}",
                unsafe_path.display()
            );
        }
    }
}
