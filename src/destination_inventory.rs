use crate::manifest_scan::ManifestEntry;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DestinationInventory {
    pub(crate) unchanged_file_ids: BTreeSet<usize>,

    pub(crate) unchanged_bytes: u64,

    pub(crate) changed_files: u64,

    pub(crate) missing_files: u64,

    pub(crate) conflicting_entries: u64,
}

impl DestinationInventory {
    pub(crate) fn unchanged_files(&self) -> u64 {
        u64::try_from(self.unchanged_file_ids.len())
            .expect("destination inventory cannot contain more file IDs than u64 can represent")
    }
}

pub(crate) fn compare_fast(
    destination_root: &Path,
    manifest: &[ManifestEntry],
) -> io::Result<DestinationInventory> {
    let root_metadata = fs::symlink_metadata(destination_root).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to inspect destination root {}: {error}",
                destination_root.display(),
            ),
        )
    })?;

    if root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "destination root must not be a reparse point: {}",
                destination_root.display(),
            ),
        ));
    }

    if !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "destination root is not a directory: {}",
                destination_root.display(),
            ),
        ));
    }

    let mut inventory = DestinationInventory::default();

    for (file_id, entry) in manifest.iter().enumerate() {
        let path = destination_root.join(&entry.relative_path);

        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,

            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                increment(
                    &mut inventory.missing_files,
                    "missing destination file count",
                )?;

                continue;
            }

            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "failed to inspect destination entry {}: {error}",
                        path.display(),
                    ),
                ));
            }
        };

        let attributes = metadata.file_attributes();

        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
            increment(
                &mut inventory.conflicting_entries,
                "conflicting destination entry count",
            )?;

            continue;
        }

        let unchanged = metadata.len() == entry.file_size
            && metadata.last_write_time() == entry.last_write_time;

        if unchanged {
            if !inventory.unchanged_file_ids.insert(file_id) {
                return Err(io::Error::other(
                    "destination inventory produced a duplicate file ID",
                ));
            }

            inventory.unchanged_bytes = inventory
                .unchanged_bytes
                .checked_add(entry.file_size)
                .ok_or_else(|| io::Error::other("unchanged destination byte count overflowed"))?;
        } else {
            increment(
                &mut inventory.changed_files,
                "changed destination file count",
            )?;
        }
    }

    Ok(inventory)
}

fn increment(value: &mut u64, description: &str) -> io::Result<()> {
    *value = value
        .checked_add(1)
        .ok_or_else(|| io::Error::other(format!("{description} overflowed")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::compare_fast;
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::env;
    use std::fs;
    use std::os::windows::fs::MetadataExt;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn exact_file_metadata_is_unchanged() {
        let root = temporary_root("exact");

        fs::create_dir_all(&root).unwrap();

        let path = root.join("matching.bin");

        fs::write(&path, b"matching contents").unwrap();

        fs::write(root.join("unrelated.bin"), b"leave me alone").unwrap();

        let metadata = fs::metadata(&path).unwrap();

        let manifest = vec![entry(
            "matching.bin",
            metadata.len(),
            metadata.last_write_time(),
        )];

        let inventory = compare_fast(&root, &manifest).unwrap();

        assert_eq!(inventory.unchanged_files(), 1,);

        assert_eq!(inventory.unchanged_bytes, metadata.len(),);

        assert_eq!(
            inventory.unchanged_file_ids.into_iter().collect::<Vec<_>>(),
            vec![0],
        );

        assert_eq!(inventory.changed_files, 0,);

        assert_eq!(inventory.missing_files, 0,);

        assert_eq!(inventory.conflicting_entries, 0,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_missing_and_conflicting_entries_are_classified() {
        let root = temporary_root("differences");

        fs::create_dir_all(&root).unwrap();

        let changed_path = root.join("changed.bin");

        fs::write(&changed_path, b"same size").unwrap();

        let changed_metadata = fs::metadata(&changed_path).unwrap();

        fs::create_dir_all(root.join("conflict.bin")).unwrap();

        let manifest = vec![
            entry(
                "changed.bin",
                changed_metadata.len(),
                changed_metadata.last_write_time().checked_add(1).unwrap(),
            ),
            entry("missing.bin", 123, 456),
            entry("conflict.bin", 789, 101_112),
        ];

        let inventory = compare_fast(&root, &manifest).unwrap();

        assert_eq!(inventory.unchanged_files(), 0,);

        assert_eq!(inventory.changed_files, 1,);

        assert_eq!(inventory.missing_files, 1,);

        assert_eq!(inventory.conflicting_entries, 1,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_directory_root_is_rejected() {
        let root = temporary_root("not-directory");

        fs::write(&root, b"not a directory").unwrap();

        let error = compare_fast(&root, &[]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput,);

        fs::remove_file(root).unwrap();
    }

    fn entry(relative_path: &str, file_size: u64, last_write_time: u64) -> ManifestEntry {
        ManifestEntry {
            relative_path: PathBuf::from(relative_path),

            file_size,

            last_write_time,

            file_attributes: 0,

            class: FileClass::Tiny,
        }
    }

    fn temporary_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-destination-inventory-{name}-{}-{unique}",
            process::id(),
        ))
    }
}
