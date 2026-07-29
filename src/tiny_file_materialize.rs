use crate::windows_file_replace;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) fn write_verified(
    destination_root: &Path,
    file_id: usize,
    relative_path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    let final_path = destination_root.join(relative_path);
    let temporary_path = temporary_path(&final_path, file_id);

    let file_size = u64::try_from(contents.len())
        .map_err(|_| io::Error::other("tiny-file length cannot be represented"))?;

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)?;

    let write_result = (|| -> io::Result<()> {
        file.set_len(file_size)?;
        file.write_all(contents)?;
        file.flush()
    })();

    if let Err(error) = write_result {
        drop(file);

        let _ = fs::remove_file(&temporary_path);

        return Err(error);
    }

    drop(file);

    windows_file_replace::replace(&temporary_path, &final_path)
}

fn temporary_path(final_path: &Path, file_id: usize) -> PathBuf {
    let mut temporary = OsString::from(final_path.as_os_str());

    temporary.push(format!(".ncs-part-{file_id}"));

    PathBuf::from(temporary)
}

#[cfg(test)]
mod tests {
    use super::write_verified;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn verified_tiny_file_is_materialized_atomically() {
        let root = temporary_root();

        fs::create_dir_all(root.join("nested")).unwrap();

        write_verified(
            &root,
            7,
            PathBuf::from("nested/file.bin").as_path(),
            b"first contents",
        )
        .unwrap();

        assert_eq!(
            fs::read(root.join("nested/file.bin")).unwrap(),
            b"first contents",
        );

        assert!(
            !root
                .join("nested/file.bin.ncs-part-7")
                .try_exists()
                .unwrap()
        );

        write_verified(
            &root,
            7,
            PathBuf::from("nested/file.bin").as_path(),
            b"replacement contents",
        )
        .unwrap();

        assert_eq!(
            fs::read(root.join("nested/file.bin")).unwrap(),
            b"replacement contents",
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-tiny-materialize-{}-{unique}",
            process::id(),
        ))
    }
}
