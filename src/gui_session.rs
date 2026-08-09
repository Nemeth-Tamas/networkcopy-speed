use crate::destination_layout::DestinationLayout;
use crate::gui_transfer::{GuiConnectionMode, GuiTransferRequest};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const SESSION_MAGIC: &str = "NETWORKCOPY_GUI_SESSION_V1";

const SEND_SESSION_FILE: &str = "networkcopy-speed-send.resume";

const RECEIVE_SESSION_FILE: &str = "networkcopy-speed-receive.resume";

const FALLBACK_DIRECTORY: &str = "NetworkCopy Speed Edition";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionKind {
    Send,
    Receive,
}

impl SessionKind {
    const fn file_name(self) -> &'static str {
        match self {
            Self::Send => SEND_SESSION_FILE,

            Self::Receive => RECEIVE_SESSION_FILE,
        }
    }
}

pub fn save(request: &GuiTransferRequest) -> io::Result<PathBuf> {
    let contents = encode_request(request)?;

    let kind = session_kind(request);

    let paths = session_paths(kind)?;

    let mut last_error = None;

    for path in &paths {
        match write_session_file(path, &contents) {
            Ok(()) => {
                for other in &paths {
                    if other == path {
                        continue;
                    }

                    let _ = remove_if_present(other);
                }

                return Ok(path.clone());
            }

            Err(error) => {
                last_error = Some(io::Error::new(
                    error.kind(),
                    format!("failed to save {}: {error}", path.display(),),
                ));
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| io::Error::other("no GUI session storage location is available")))
}

pub fn load_latest() -> io::Result<Option<GuiTransferRequest>> {
    load_latest_for_kinds(&[SessionKind::Send, SessionKind::Receive])
}

pub fn load_receive() -> io::Result<Option<GuiTransferRequest>> {
    load_latest_for_kinds(&[SessionKind::Receive])
}

fn load_latest_for_kinds(kinds: &[SessionKind]) -> io::Result<Option<GuiTransferRequest>> {
    let mut candidates = Vec::new();

    for &kind in kinds {
        for path in session_paths(kind)? {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);

                    candidates.push((modified, kind, path));
                }

                Ok(_) => {}

                Err(error) if error.kind() == io::ErrorKind::NotFound => {}

                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("failed to inspect GUI session record: {error}",),
                    ));
                }
            }
        }
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

    let mut first_error = None;

    for (_modified, expected_kind, path) in candidates {
        let result = fs::read_to_string(&path)
            .and_then(|contents| decode_request(&contents))
            .and_then(|request| {
                if session_kind(&request) != expected_kind {
                    return Err(invalid_data(
                        "GUI session direction does not match its file name",
                    ));
                }

                Ok(request)
            });

        match result {
            Ok(request) => {
                return Ok(Some(request));
            }

            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(io::Error::new(
                        error.kind(),
                        format!("failed to load {}: {error}", path.display(),),
                    ));
                }
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(None)
}

pub fn clear(request: &GuiTransferRequest) -> io::Result<()> {
    let kind = session_kind(request);

    let mut first_error = None;

    for path in session_paths(kind)? {
        if let Err(error) = remove_if_present(&path)
            && first_error.is_none()
        {
            first_error = Some(io::Error::new(
                error.kind(),
                format!("failed to remove {}: {error}", path.display(),),
            ));
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(())
}

fn session_kind(request: &GuiTransferRequest) -> SessionKind {
    match request {
        GuiTransferRequest::Send { .. } => SessionKind::Send,

        GuiTransferRequest::Receive { .. } => SessionKind::Receive,
    }
}

fn session_paths(kind: SessionKind) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();

    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        paths.push(parent.join(kind.file_name()));
    }

    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let fallback = PathBuf::from(local_app_data)
            .join(FALLBACK_DIRECTORY)
            .join(kind.file_name());

        if !paths.contains(&fallback) {
            paths.push(fallback);
        }
    }

    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "neither the executable directory nor LOCALAPPDATA is available",
        ));
    }

    Ok(paths)
}

fn write_session_file(path: &Path, contents: &str) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "GUI session path has no parent directory",
        )
    })?;

    fs::create_dir_all(parent)?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "GUI session path has no file name",
        )
    })?;

    let temporary = path.with_file_name(format!("{}.tmp", file_name.to_string_lossy(),));

    let _ = remove_if_present(&temporary);

    fs::write(&temporary, contents)?;

    if path.exists()
        && let Err(error) = fs::remove_file(path)
    {
        let _ = remove_if_present(&temporary);

        return Err(error);
    }

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = remove_if_present(&temporary);

        return Err(error);
    }

    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),

        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),

        Err(error) => Err(error),
    }
}

fn encode_request(request: &GuiTransferRequest) -> io::Result<String> {
    let (
        direction,
        connection,
        path,
        worker_count,
        calibration_mib,
        update_existing,
        destination_layout,
        preserve_desktop_layout,
        forced_data_stream_count,
    ) = match request {
        GuiTransferRequest::Send {
            connection,
            source_root,
            worker_count,
            calibration_mib,
            forced_data_stream_count,
            preserve_desktop_layout,
        } => (
            "send",
            *connection,
            source_root.as_path(),
            *worker_count,
            *calibration_mib,
            false,
            DestinationLayout::Exact,
            *preserve_desktop_layout,
            *forced_data_stream_count,
        ),

        GuiTransferRequest::Receive {
            connection,
            destination_root,
            destination_layout,
            update_existing,
        } => (
            "receive",
            *connection,
            destination_root.as_path(),
            0,
            0,
            *update_existing,
            *destination_layout,
            false,
            None,
        ),
    };

    let (connection_name, address) = match connection {
        GuiConnectionMode::Direct => ("direct", String::new()),

        GuiConnectionMode::Address(address) => ("address", address.to_string()),
    };

    let forced_data_stream_count = forced_data_stream_count
        .map(|value| value.to_string())
        .unwrap_or_default();

    Ok(format!(
        "{SESSION_MAGIC}\n\
         direction={direction}\n\
         connection={connection_name}\n\
         address={address}\n\
         path_utf16={}\n\
         worker_count={worker_count}\n\
         calibration_mib={calibration_mib}\n\
         forced_data_stream_count={forced_data_stream_count}\n\
         update_existing={update_existing}\n\
         destination_layout={}\n\
         preserve_desktop_layout={preserve_desktop_layout}\n",
        encode_path(path),
        destination_layout.code(),
    ))
}

fn decode_request(contents: &str) -> io::Result<GuiTransferRequest> {
    let mut lines = contents.lines();

    if lines.next() != Some(SESSION_MAGIC) {
        return Err(invalid_data("unsupported GUI session record"));
    }

    let mut fields = BTreeMap::new();

    for line in lines {
        if line.is_empty() {
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid_data("GUI session field is missing '='"))?;

        if fields.insert(key.to_string(), value.to_string()).is_some() {
            return Err(invalid_data("GUI session contains a duplicate field"));
        }
    }

    let direction = required_field(&fields, "direction")?;

    let connection = match required_field(&fields, "connection")? {
        "direct" => GuiConnectionMode::Direct,

        "address" => {
            let address = required_field(&fields, "address")?
                .parse::<SocketAddr>()
                .map_err(|error| invalid_data(format!("invalid saved socket address: {error}",)))?;

            GuiConnectionMode::Address(address)
        }

        _ => {
            return Err(invalid_data("unknown GUI session connection mode"));
        }
    };

    let path = decode_path(required_field(&fields, "path_utf16")?)?;

    let update_existing = match fields.get("update_existing") {
        Some(value) => value.parse::<bool>().map_err(|error| {
            invalid_data(format!(
                "invalid GUI session field 'update_existing': {error}",
            ))
        })?,

        None => false,
    };

    let destination_layout = match fields.get("destination_layout") {
        Some(value) => {
            let code = value.parse::<u8>().map_err(|error| {
                invalid_data(format!(
                    "invalid GUI session field 'destination_layout': {error}",
                ))
            })?;

            DestinationLayout::from_code(code).map_err(|error| {
                invalid_data(format!("invalid saved destination layout: {error}",))
            })?
        }

        None => DestinationLayout::Exact,
    };

    let preserve_desktop_layout = match fields.get("preserve_desktop_layout") {
        Some(value) => value.parse::<bool>().map_err(|error| {
            invalid_data(format!(
                "invalid GUI session field 'preserve_desktop_layout': {error}",
            ))
        })?,

        None => false,
    };

    let forced_data_stream_count = match fields.get("forced_data_stream_count").map(String::as_str)
    {
        None | Some("") => None,

        Some(value) => {
            let value = value.parse::<usize>().map_err(|error| {
                invalid_data(format!(
                    "invalid GUI session field 'forced_data_stream_count': {error}",
                ))
            })?;

            if value == 0 {
                return Err(invalid_data("saved transfer stream count must not be zero"));
            }

            Some(value)
        }
    };

    match direction {
        "send" => {
            let worker_count = parse_field::<usize>(&fields, "worker_count")?;

            let calibration_mib = parse_field::<u64>(&fields, "calibration_mib")?;

            if worker_count == 0 {
                return Err(invalid_data("saved scanner worker count must not be zero"));
            }

            if calibration_mib == 0 {
                return Err(invalid_data("saved calibration size must not be zero"));
            }

            Ok(GuiTransferRequest::Send {
                connection,
                source_root: path,
                worker_count,
                calibration_mib,
                forced_data_stream_count,
                preserve_desktop_layout,
            })
        }

        "receive" => Ok(GuiTransferRequest::Receive {
            connection,

            destination_root: path,

            destination_layout,

            update_existing,
        }),

        _ => Err(invalid_data("unknown GUI session direction")),
    }
}

fn required_field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> io::Result<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| invalid_data(format!("GUI session is missing field '{key}'",)))
}

fn parse_field<T>(fields: &BTreeMap<String, String>, key: &str) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_field(fields, key)?
        .parse::<T>()
        .map_err(|error| invalid_data(format!("invalid GUI session field '{key}': {error}",)))
}

fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();

    for unit in path.as_os_str().encode_wide() {
        write!(&mut encoded, "{unit:04X}",)
            .expect("writing UTF-16 path data into a String cannot fail");
    }

    encoded
}

fn decode_path(encoded: &str) -> io::Result<PathBuf> {
    if !encoded.len().is_multiple_of(4) {
        return Err(invalid_data("saved UTF-16 path has an invalid length"));
    }

    let mut units = Vec::with_capacity(encoded.len() / 4);

    for chunk in encoded.as_bytes().chunks_exact(4) {
        let text = std::str::from_utf8(chunk).map_err(|error| {
            invalid_data(format!("saved UTF-16 path is not hexadecimal: {error}",))
        })?;

        let unit = u16::from_str_radix(text, 16).map_err(|error| {
            invalid_data(format!("saved UTF-16 path is not hexadecimal: {error}",))
        })?;

        units.push(unit);
    }

    Ok(PathBuf::from(OsString::from_wide(&units)))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::{decode_request, encode_request};
    use crate::destination_layout::DestinationLayout;
    use crate::gui_transfer::{GuiConnectionMode, GuiTransferRequest};
    use std::net::SocketAddr;
    use std::path::PathBuf;

    #[test]
    fn direct_send_session_round_trips() {
        let request = GuiTransferRequest::Send {
            connection: GuiConnectionMode::Direct,

            source_root: PathBuf::from(r"C:\Teszt\árvíztűrő tükörfúrógép"),

            worker_count: 6,
            calibration_mib: 128,

            forced_data_stream_count: Some(1),

            preserve_desktop_layout: true,
        };

        let encoded = encode_request(&request).unwrap();

        let decoded = decode_request(&encoded).unwrap();

        assert_eq!(decoded, request,);
    }

    #[test]
    fn addressed_receive_session_round_trips() {
        let address = "127.0.0.1:7337".parse::<SocketAddr>().unwrap();

        let request = GuiTransferRequest::Receive {
            connection: GuiConnectionMode::Address(address),

            destination_root: PathBuf::from(r"C:\NetworkCopy Received"),

            destination_layout: DestinationLayout::SourceNameUnderRoot,

            update_existing: true,
        };

        let encoded = encode_request(&request).unwrap();

        let decoded = decode_request(&encoded).unwrap();

        assert_eq!(decoded, request,);

        let legacy = encoded
            .replace("update_existing=true\n", "")
            .replace("destination_layout=1\n", "");

        let legacy_decoded = decode_request(&legacy).unwrap();

        assert_eq!(
            legacy_decoded,
            GuiTransferRequest::Receive {
                connection: GuiConnectionMode::Address(address),

                destination_root: PathBuf::from(r"C:\NetworkCopy Received",),

                destination_layout: DestinationLayout::Exact,

                update_existing: false,
            },
        );
    }

    #[test]
    fn unknown_session_version_is_rejected() {
        let error = decode_request("NETWORKCOPY_GUI_SESSION_V999\n").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }
}
