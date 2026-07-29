use crate::manifest_scan::ManifestEntry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const HASH_BUFFER_BYTES: usize = 1024 * 1024;

pub(crate) const FILE_DIGEST_BYTES: usize = 32;

pub(crate) type FileDigest = [u8; FILE_DIGEST_BYTES];

pub(crate) fn hash_candidates(
    root: &Path,
    manifest: &[ManifestEntry],
    file_ids: &BTreeSet<usize>,
) -> io::Result<BTreeMap<usize, FileDigest>> {
    let mut digests = BTreeMap::new();

    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];

    for &file_id in file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("verification candidate references unknown file ID {file_id}",),
            )
        })?;

        let path = root.join(&entry.relative_path);

        let before = inspect_candidate(&path, entry.file_size)?;

        let digest = hash_file(&path, &mut buffer)?;

        let after = inspect_candidate(&path, entry.file_size)?;

        if before != after {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!(
                    "verification candidate changed while being hashed: {}",
                    path.display(),
                ),
            ));
        }

        if digests.insert(file_id, digest).is_some() {
            return Err(io::Error::other(format!(
                "verification candidate produced duplicate file ID {file_id}",
            )));
        }
    }

    Ok(digests)
}

pub(crate) fn matching_candidates(
    destination_digests: &BTreeMap<usize, FileDigest>,
    source_digests: &BTreeMap<usize, FileDigest>,
) -> io::Result<BTreeSet<usize>> {
    if !destination_digests.keys().eq(source_digests.keys()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source and destination verification candidate sets differ",
        ));
    }

    Ok(destination_digests
        .iter()
        .filter_map(|(&file_id, destination_digest)| {
            let source_digest = source_digests
                .get(&file_id)
                .expect("candidate key sets were validated as equal");

            (destination_digest == source_digest).then_some(file_id)
        })
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateSnapshot {
    file_size: u64,
    last_write_time: u64,
}

fn inspect_candidate(path: &Path, expected_size: u64) -> io::Result<CandidateSnapshot> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to inspect verification candidate {}: {error}",
                path.display(),
            ),
        )
    })?;

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "verification candidate is not a regular file: {}",
                path.display(),
            ),
        ));
    }

    if metadata.len() != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "verification candidate size changed for {}: expected {expected_size}, found {}",
                path.display(),
                metadata.len(),
            ),
        ));
    }

    Ok(CandidateSnapshot {
        file_size: metadata.len(),

        last_write_time: metadata.last_write_time(),
    })
}

fn hash_file(path: &Path, buffer: &mut [u8]) -> io::Result<FileDigest> {
    let mut file = File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to open verification candidate {}: {error}",
                path.display(),
            ),
        )
    })?;

    let mut hasher = blake3::Hasher::new();

    loop {
        let read = file.read(buffer).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to hash verification candidate {}: {error}",
                    path.display(),
                ),
            )
        })?;

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{hash_candidates, matching_candidates};
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::collections::BTreeSet;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn candidate_hash_matches_blake3() {
        let root = temporary_root("matching-hash");

        fs::create_dir_all(&root).unwrap();

        let contents = b"NetworkCopy verified update candidate";

        fs::write(root.join("candidate.bin"), contents).unwrap();

        let manifest = vec![entry("candidate.bin", contents.len() as u64)];

        let digests = hash_candidates(&root, &manifest, &BTreeSet::from([0_usize])).unwrap();

        assert_eq!(digests.get(&0,), Some(blake3::hash(contents,).as_bytes(),),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_size_different_contents_do_not_match() {
        let source_root = temporary_root("different-source");

        let destination_root = temporary_root("different-destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        fs::write(source_root.join("candidate.bin"), b"source-content").unwrap();

        fs::write(destination_root.join("candidate.bin"), b"other--content").unwrap();

        let manifest = vec![entry("candidate.bin", 14)];

        let candidates = BTreeSet::from([0_usize]);

        let source_digests = hash_candidates(&source_root, &manifest, &candidates).unwrap();

        let destination_digests =
            hash_candidates(&destination_root, &manifest, &candidates).unwrap();

        let matching = matching_candidates(&destination_digests, &source_digests).unwrap();

        assert!(matching.is_empty(),);

        fs::remove_dir_all(source_root).unwrap();

        fs::remove_dir_all(destination_root).unwrap();
    }

    #[test]
    fn equal_contents_are_selected() {
        let source_root = temporary_root("equal-source");

        let destination_root = temporary_root("equal-destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let contents = b"identical candidate";

        fs::write(source_root.join("candidate.bin"), contents).unwrap();

        fs::write(destination_root.join("candidate.bin"), contents).unwrap();

        let manifest = vec![entry("candidate.bin", contents.len() as u64)];

        let candidates = BTreeSet::from([0_usize]);

        let source_digests = hash_candidates(&source_root, &manifest, &candidates).unwrap();

        let destination_digests =
            hash_candidates(&destination_root, &manifest, &candidates).unwrap();

        assert_eq!(
            matching_candidates(&destination_digests, &source_digests,).unwrap(),
            candidates,
        );

        fs::remove_dir_all(source_root).unwrap();

        fs::remove_dir_all(destination_root).unwrap();
    }

    #[test]
    fn unknown_candidate_file_id_is_rejected() {
        let root = temporary_root("unknown-id");

        fs::create_dir_all(&root).unwrap();

        let error = hash_candidates(&root, &[], &BTreeSet::from([7_usize])).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);

        fs::remove_dir_all(root).unwrap();
    }

    fn entry(relative_path: &str, file_size: u64) -> ManifestEntry {
        ManifestEntry {
            relative_path: PathBuf::from(relative_path),

            file_size,

            last_write_time: 0,

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
            "networkcopy-update-verification-{name}-{}-{unique}",
            process::id(),
        ))
    }
}
