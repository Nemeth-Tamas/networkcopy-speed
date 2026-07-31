use std::io;
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

const WINDOWS_DRIVE_COUNT: u8 = 26;

pub(crate) fn list_roots() -> io::Result<Vec<String>> {
    let drive_mask = unsafe { GetLogicalDrives() };

    if drive_mask == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut roots = Vec::new();

    for index in 0_u8..WINDOWS_DRIVE_COUNT {
        let bit = 1_u32 << u32::from(index);

        if drive_mask & bit == 0 {
            continue;
        }

        let drive_letter = char::from(b'A' + index);

        roots.push(format!("{drive_letter}:\\"));
    }

    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::list_roots;

    #[test]
    fn logical_roots_are_valid_windows_drive_paths() {
        let roots = list_roots().unwrap();

        assert!(!roots.is_empty());

        assert!(roots.iter().all(|root| {
            let bytes = root.as_bytes();

            bytes.len() == 3
                && bytes[0].is_ascii_uppercase()
                && bytes[1] == b':'
                && bytes[2] == b'\\'
        }));
    }
}
