use crate::management_protocol::MAX_MANAGEMENT_PAYLOAD_BYTES;
use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

const DIRECTORY_REQUEST_VERSION: u16 = 1;
const DIRECTORY_REQUEST_HEADER_BYTES: usize = 4;

const DIRECTORY_RESPONSE_VERSION: u16 = 1;
const DIRECTORY_RESPONSE_HEADER_BYTES: usize = 8;
const DIRECTORY_ENTRY_HEADER_BYTES: usize = 20;

const MAX_REMOTE_PATH_BYTES: usize = 32 * 1024;
const MAX_ENTRY_NAME_BYTES: usize = 1024;
const MAX_DIRECTORY_ENTRIES: usize = 50_000;

const UNKNOWN_MODIFIED_TIME: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementEntryKind {
    Directory = 1,
    File = 2,
    Other = 3,
}

impl ManagementEntryKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Directory => "DIR",
            Self::File => "FILE",
            Self::Other => "OTHER",
        }
    }

    const fn sort_rank(self) -> u8 {
        match self {
            Self::Directory => 0,
            Self::File => 1,
            Self::Other => 2,
        }
    }
}

impl TryFrom<u8> for ManagementEntryKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Directory),
            2 => Ok(Self::File),
            3 => Ok(Self::Other),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management directory entry kind {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementDirectoryEntry {
    pub name: String,

    pub kind: ManagementEntryKind,

    pub size: u64,

    pub modified_unix_seconds: Option<u64>,
}

pub(crate) fn encode_request(path: &str) -> io::Result<Vec<u8>> {
    validate_text(path, MAX_REMOTE_PATH_BYTES, "remote directory path")?;

    let path_length = u16::try_from(path.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote directory path length cannot be represented",
        )
    })?;

    let total_length = DIRECTORY_REQUEST_HEADER_BYTES
        .checked_add(path.len())
        .ok_or_else(|| io::Error::other("directory request payload length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&DIRECTORY_REQUEST_VERSION.to_le_bytes());

    payload.extend_from_slice(&path_length.to_le_bytes());

    payload.extend_from_slice(path.as_bytes());

    Ok(payload)
}

pub(crate) fn decode_request(payload: &[u8]) -> io::Result<String> {
    if payload.len() < DIRECTORY_REQUEST_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory request payload has {} bytes, expected at least {DIRECTORY_REQUEST_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory request version was malformed",
        )
    })?);

    if version != DIRECTORY_REQUEST_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported directory request version {version}"),
        ));
    }

    let path_length = usize::from(u16::from_le_bytes(payload[2..4].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory request path length was malformed",
            )
        },
    )?));

    let expected_length = DIRECTORY_REQUEST_HEADER_BYTES
        .checked_add(path_length)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory request payload length overflowed",
            )
        })?;

    if payload.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory request payload has {} bytes, expected {expected_length}",
                payload.len(),
            ),
        ));
    }

    decode_text(
        &payload[DIRECTORY_REQUEST_HEADER_BYTES..],
        MAX_REMOTE_PATH_BYTES,
        "remote directory path",
    )
}

pub(crate) fn enumerate(path: &str) -> io::Result<Vec<ManagementDirectoryEntry>> {
    validate_text(path, MAX_REMOTE_PATH_BYTES, "remote directory path")?;

    let path = Path::new(path);

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote directory path must be absolute",
        ));
    }

    let metadata = fs::metadata(path)?;

    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote directory path does not identify a directory",
        ));
    }

    let mut entries = Vec::new();

    for entry in fs::read_dir(path)? {
        let entry = entry?;

        let name = entry.file_name().to_string_lossy().into_owned();

        validate_text(&name, MAX_ENTRY_NAME_BYTES, "directory entry name")?;

        let metadata = entry.metadata()?;

        let kind = if metadata.is_dir() {
            ManagementEntryKind::Directory
        } else if metadata.is_file() {
            ManagementEntryKind::File
        } else {
            ManagementEntryKind::Other
        };

        let size = if kind == ManagementEntryKind::File {
            metadata.len()
        } else {
            0
        };

        let modified_unix_seconds = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());

        entries.push(ManagementDirectoryEntry {
            name,
            kind,
            size,
            modified_unix_seconds,
        });

        if entries.len() > MAX_DIRECTORY_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory contains more than {MAX_DIRECTORY_ENTRIES} entries"),
            ));
        }
    }

    entries.sort_by(|left, right| {
        left.kind
            .sort_rank()
            .cmp(&right.kind.sort_rank())
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(entries)
}

pub(crate) fn encode_response(entries: &[ManagementDirectoryEntry]) -> io::Result<Vec<u8>> {
    if entries.len() > MAX_DIRECTORY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory response contains {} entries, exceeding the {MAX_DIRECTORY_ENTRIES} entry limit",
                entries.len(),
            ),
        ));
    }

    let entry_count = u32::try_from(entries.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory response entry count cannot be represented",
        )
    })?;

    let mut payload_length = DIRECTORY_RESPONSE_HEADER_BYTES;

    for entry in entries {
        validate_text(&entry.name, MAX_ENTRY_NAME_BYTES, "directory entry name")?;

        payload_length = payload_length
            .checked_add(DIRECTORY_ENTRY_HEADER_BYTES)
            .and_then(|length| length.checked_add(entry.name.len()))
            .ok_or_else(|| io::Error::other("directory response payload length overflowed"))?;
    }

    if payload_length > MAX_MANAGEMENT_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory response requires {payload_length} bytes, exceeding the {MAX_MANAGEMENT_PAYLOAD_BYTES} byte management limit"
            ),
        ));
    }

    let mut payload = Vec::with_capacity(payload_length);

    payload.extend_from_slice(&DIRECTORY_RESPONSE_VERSION.to_le_bytes());

    payload.extend_from_slice(&0_u16.to_le_bytes());

    payload.extend_from_slice(&entry_count.to_le_bytes());

    for entry in entries {
        let name_length = u16::try_from(entry.name.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry name length cannot be represented",
            )
        })?;

        payload.push(entry.kind as u8);
        payload.push(0);

        payload.extend_from_slice(&name_length.to_le_bytes());

        payload.extend_from_slice(&entry.size.to_le_bytes());

        payload.extend_from_slice(
            &entry
                .modified_unix_seconds
                .unwrap_or(UNKNOWN_MODIFIED_TIME)
                .to_le_bytes(),
        );

        payload.extend_from_slice(entry.name.as_bytes());
    }

    Ok(payload)
}

pub(crate) fn decode_response(payload: &[u8]) -> io::Result<Vec<ManagementDirectoryEntry>> {
    if payload.len() < DIRECTORY_RESPONSE_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory response payload has {} bytes, expected at least {DIRECTORY_RESPONSE_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory response version was malformed",
        )
    })?);

    if version != DIRECTORY_RESPONSE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported directory response version {version}"),
        ));
    }

    let reserved = u16::from_le_bytes(payload[2..4].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory response reserved field was malformed",
        )
    })?);

    if reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory response reserved field was not zero",
        ));
    }

    let entry_count = usize::try_from(u32::from_le_bytes(payload[4..8].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory response entry count was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory response entry count cannot be represented",
        )
    })?;

    if entry_count > MAX_DIRECTORY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory response contains {entry_count} entries, exceeding the {MAX_DIRECTORY_ENTRIES} entry limit"
            ),
        ));
    }

    let mut entries = Vec::with_capacity(entry_count);

    let mut cursor = DIRECTORY_RESPONSE_HEADER_BYTES;

    for _ in 0..entry_count {
        let header_end = cursor
            .checked_add(DIRECTORY_ENTRY_HEADER_BYTES)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry header position overflowed",
                )
            })?;

        if header_end > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "directory response ended inside an entry header",
            ));
        }

        let kind = ManagementEntryKind::try_from(payload[cursor])?;

        if payload[cursor + 1] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry reserved byte was not zero",
            ));
        }

        let name_length = usize::from(u16::from_le_bytes(
            payload[cursor + 2..cursor + 4].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry name length was malformed",
                )
            })?,
        ));

        let size =
            u64::from_le_bytes(payload[cursor + 4..cursor + 12].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry size was malformed",
                )
            })?);

        let modified =
            u64::from_le_bytes(payload[cursor + 12..cursor + 20].try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry modification time was malformed",
                )
            })?);

        cursor = header_end;

        let name_end = cursor.checked_add(name_length).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry name position overflowed",
            )
        })?;

        if name_end > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "directory response ended inside an entry name",
            ));
        }

        let name = decode_text(
            &payload[cursor..name_end],
            MAX_ENTRY_NAME_BYTES,
            "directory entry name",
        )?;

        entries.push(ManagementDirectoryEntry {
            name,
            kind,
            size,
            modified_unix_seconds: (modified != UNKNOWN_MODIFIED_TIME).then_some(modified),
        });

        cursor = name_end;
    }

    if cursor != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory response contains {} trailing bytes",
                payload.len() - cursor,
            ),
        ));
    }

    Ok(entries)
}

fn decode_text(bytes: &[u8], maximum_bytes: usize, description: &str) -> io::Result<String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} was not valid UTF-8: {error}"),
            )
        })?
        .to_owned();

    validate_text(&value, maximum_bytes, description)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    Ok(value)
}

fn validate_text(value: &str, maximum_bytes: usize, description: &str) -> io::Result<()> {
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must not be empty"),
        ));
    }

    if value.len() > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{description} contains {} bytes, exceeding the {maximum_bytes} byte limit",
                value.len(),
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ManagementDirectoryEntry, ManagementEntryKind, decode_request, decode_response,
        encode_request, encode_response, enumerate,
    };
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn directory_request_round_trips() {
        let expected = r"C:\Users\Public";

        let encoded = encode_request(expected).unwrap();

        let decoded = decode_request(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn directory_response_round_trips() {
        let expected = vec![
            ManagementDirectoryEntry {
                name: "Folder".to_string(),
                kind: ManagementEntryKind::Directory,
                size: 0,
                modified_unix_seconds: Some(123),
            },
            ManagementDirectoryEntry {
                name: "file.bin".to_string(),
                kind: ManagementEntryKind::File,
                size: 456,
                modified_unix_seconds: None,
            },
        ];

        let encoded = encode_response(&expected).unwrap();

        let decoded = decode_response(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn local_directory_lists_folders_before_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let root = std::env::temp_dir().join(format!(
            "networkcopy-management-directory-{}-{unique}",
            process::id(),
        ));

        fs::create_dir_all(root.join("Folder")).unwrap();

        fs::write(root.join("file.bin"), [1_u8, 2, 3, 4]).unwrap();

        let entries = enumerate(root.to_str().unwrap()).unwrap();

        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].name, "Folder",);

        assert_eq!(entries[0].kind, ManagementEntryKind::Directory,);

        assert_eq!(entries[1].name, "file.bin",);

        assert_eq!(entries[1].kind, ManagementEntryKind::File,);

        assert_eq!(entries[1].size, 4,);

        fs::remove_dir_all(root).unwrap();
    }
}
