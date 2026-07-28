use crate::manifest_scan::ManifestEntry;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    FILE_WRITE_ATTRIBUTES, SetFileAttributesW, SetFileTime,
};

const RESTORABLE_FILE_ATTRIBUTES: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

pub(crate) fn restore_manifest_files(
    destination_root: &Path,
    manifest: &[ManifestEntry],
) -> io::Result<()> {
    for entry in manifest {
        let path = destination_root.join(&entry.relative_path);

        restore_file(&path, entry.last_write_time, entry.file_attributes).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to restore metadata for {}: {error}", path.display()),
            )
        })?;
    }

    Ok(())
}

pub(crate) fn restore_file(
    path: &Path,
    last_write_time: u64,
    source_attributes: u32,
) -> io::Result<()> {
    restore_last_write_time(path, last_write_time)?;

    restore_attributes(path, source_attributes)
}

fn restore_last_write_time(path: &Path, last_write_time: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .open(path)?;

    let last_write_time = filetime_from_u64(last_write_time);

    let succeeded = unsafe {
        SetFileTime(
            file.as_raw_handle() as HANDLE,
            ptr::null(),
            ptr::null(),
            &last_write_time,
        )
    };

    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn restore_attributes(path: &Path, source_attributes: u32) -> io::Result<()> {
    let attributes = restorable_file_attributes(source_attributes);

    let path = wide_null(path);

    let succeeded = unsafe { SetFileAttributesW(path.as_ptr(), attributes) };

    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn restorable_file_attributes(source_attributes: u32) -> u32 {
    let attributes = source_attributes & RESTORABLE_FILE_ATTRIBUTES;

    if attributes == 0 {
        FILE_ATTRIBUTE_NORMAL
    } else {
        attributes
    }
}

fn filetime_from_u64(value: u64) -> FILETIME {
    FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    }
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{RESTORABLE_FILE_ATTRIBUTES, restorable_file_attributes, restore_file};
    use std::env;
    use std::fs;
    use std::os::windows::fs::MetadataExt;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_SPARSE_FILE,
    };

    #[test]
    fn masks_unsupported_file_attributes() {
        assert_eq!(
            restorable_file_attributes(FILE_ATTRIBUTE_SPARSE_FILE,),
            FILE_ATTRIBUTE_NORMAL
        );

        assert_eq!(
            restorable_file_attributes(
                FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_SPARSE_FILE,
            ),
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_ARCHIVE
        );
    }

    #[test]
    fn restores_timestamp_and_safe_attributes() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let path = env::temp_dir().join(format!(
            "networkcopy-metadata-{}-{unique}.bin",
            process::id()
        ));

        fs::write(&path, b"metadata restoration").unwrap();

        let expected_last_write_time = 132_537_600_123_456_789;

        let expected_attributes = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_ARCHIVE;

        restore_file(&path, expected_last_write_time, expected_attributes).unwrap();

        let metadata = fs::metadata(&path).unwrap();

        assert_eq!(metadata.last_write_time(), expected_last_write_time);

        assert_eq!(
            metadata.file_attributes() & RESTORABLE_FILE_ATTRIBUTES,
            expected_attributes
        );

        restore_file(&path, expected_last_write_time, FILE_ATTRIBUTE_NORMAL).unwrap();

        fs::remove_file(path).unwrap();
    }
}
