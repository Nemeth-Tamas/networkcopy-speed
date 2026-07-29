use std::io;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub(crate) fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source_wide = null_terminated(source);

    let destination_wide = null_terminated(destination);

    // SAFETY: Both strings are valid, NUL-terminated UTF-16 path buffers
    // that remain alive for the complete Windows API call.
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if result == 0 {
        let error = io::Error::last_os_error();

        return Err(io::Error::new(
            error.kind(),
            format!(
                "failed to replace {} with {}: {error}",
                destination.display(),
                source.display(),
            ),
        ));
    }

    Ok(())
}

fn null_terminated(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::replace;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn replacement_overwrites_existing_file() {
        let root = temporary_root();

        fs::create_dir_all(&root).unwrap();

        let source = root.join("replacement.tmp");

        let destination = root.join("destination.bin");

        fs::write(&source, b"new contents").unwrap();

        fs::write(&destination, b"old contents").unwrap();

        replace(&source, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new contents",);

        assert!(!source.exists(),);

        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-file-replace-{}-{unique}",
            process::id(),
        ))
    }
}
