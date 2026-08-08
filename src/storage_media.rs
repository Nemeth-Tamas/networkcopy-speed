use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;

const PROPERTY_STANDARD_QUERY: u32 = 0;

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[repr(C)]
#[derive(Default)]
struct DeviceSeekPenaltyDescriptor {
    version: u32,
    size: u32,
    incurs_seek_penalty: u8,
    padding: [u8; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageMediaKind {
    SeekPenalty,
    NoSeekPenalty,
    Unknown,
}

impl fmt::Display for StorageMediaKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeekPenalty => {
                write!(formatter, "seek-penalty / rotational-sensitive",)
            }

            Self::NoSeekPenalty => {
                write!(formatter, "no seek penalty",)
            }

            Self::Unknown => {
                write!(formatter, "unknown")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageMediaReport {
    pub drive_letter: Option<char>,
    pub kind: StorageMediaKind,
}

impl StorageMediaReport {
    pub fn incurs_seek_penalty(self) -> Option<bool> {
        match self.kind {
            StorageMediaKind::SeekPenalty => Some(true),

            StorageMediaKind::NoSeekPenalty => Some(false),

            StorageMediaKind::Unknown => None,
        }
    }
}

struct VolumeHandle(HANDLE);

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY:
        // The handle was returned successfully by
        // CreateFileW and is owned by this wrapper.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn inspect_path(path: &Path) -> io::Result<StorageMediaReport> {
    let Some(drive_letter) = drive_letter_from_path(path) else {
        return Ok(StorageMediaReport {
            drive_letter: None,
            kind: StorageMediaKind::Unknown,
        });
    };

    let incurs_seek_penalty = query_drive_seek_penalty(drive_letter)?;

    Ok(StorageMediaReport {
        drive_letter: Some(char::from(drive_letter)),

        kind: if incurs_seek_penalty {
            StorageMediaKind::SeekPenalty
        } else {
            StorageMediaKind::NoSeekPenalty
        },
    })
}

fn drive_letter_from_path(path: &Path) -> Option<u8> {
    let component = path.components().next()?;

    let Component::Prefix(prefix) = component else {
        return None;
    };

    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,

        _ => return None,
    };

    Some(letter.to_ascii_uppercase())
}

fn query_drive_seek_penalty(drive_letter: u8) -> io::Result<bool> {
    let device_path = format!(r"\\.\{}:", char::from(drive_letter),);

    let wide_path: Vec<u16> = OsStr::new(&device_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY:
    // wide_path is NUL-terminated and remains alive
    // for the duration of the call. We request no
    // read/write data access, only a device handle
    // suitable for a storage-property query.
    let raw_handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if raw_handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!(
                "failed to open storage volume {device_path}: {}",
                io::Error::last_os_error(),
            ),
        ));
    }

    let handle = VolumeHandle(raw_handle);

    let query = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_SEEK_PENALTY_PROPERTY,

        query_type: PROPERTY_STANDARD_QUERY,

        additional_parameters: [0],
    };

    let mut descriptor = DeviceSeekPenaltyDescriptor::default();

    let mut bytes_returned = 0_u32;

    let input_bytes = u32::try_from(mem::size_of::<StoragePropertyQuery>())
        .map_err(|_| io::Error::other("storage property query size cannot be represented"))?;

    let output_bytes = u32::try_from(mem::size_of::<DeviceSeekPenaltyDescriptor>())
        .map_err(|_| io::Error::other("seek-penalty descriptor size cannot be represented"))?;

    // SAFETY:
    // handle is a valid open volume handle.
    // query and descriptor are repr(C) buffers that
    // remain alive and correctly sized for the call.
    let succeeded = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            ptr::from_ref(&query).cast(),
            input_bytes,
            ptr::from_mut(&mut descriptor).cast(),
            output_bytes,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if succeeded == 0 {
        let error = io::Error::last_os_error();

        return Err(io::Error::new(
            error.kind(),
            format!("storage seek-penalty query failed for {device_path}: {error}",),
        ));
    }

    if bytes_returned < output_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "storage seek-penalty query returned {bytes_returned} bytes, expected at least {output_bytes}",
            ),
        ));
    }

    Ok(descriptor.incurs_seek_penalty != 0)
}

#[cfg(test)]
mod tests {
    use super::drive_letter_from_path;
    use std::path::Path;

    #[test]
    fn drive_letter_paths_are_recognized() {
        assert_eq!(
            drive_letter_from_path(Path::new(r"C:\Windows"),),
            Some(b'C'),
        );

        assert_eq!(drive_letter_from_path(Path::new(r"f:\data"),), Some(b'F'),);

        assert_eq!(
            drive_letter_from_path(Path::new(r"\\?\D:\benchmark",),),
            Some(b'D'),
        );
    }

    #[test]
    fn unsupported_paths_have_no_drive_letter() {
        assert_eq!(drive_letter_from_path(Path::new(r".\relative"),), None,);

        assert_eq!(
            drive_letter_from_path(Path::new(r"\\server\share\folder",),),
            None,
        );
    }
}
