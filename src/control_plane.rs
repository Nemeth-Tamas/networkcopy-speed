use crate::copy_bench::format_bytes;
use crate::manifest_scan::{self, FileClass, ManifestEntry};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Keep the historical family magic stable so older peers reach the
// explicit protocol-version check instead of failing as invalid data.
const PROTOCOL_MAGIC: [u8; 4] = *b"NCS4";

const PROTOCOL_VERSION: u16 = 10;

const ROLE_CONTROL: u8 = 1;
const ROLE_DATA: u8 = 2;

const MESSAGE_MANIFEST: u8 = 0x10;
const MESSAGE_MANIFEST_ACK: u8 = 0x11;

const CONTROL_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_PATH_UTF16_UNITS: usize = 1024 * 1024;
const MAX_DATA_STREAMS: usize = 32;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const ERROR_INVALID_FUNCTION: i32 = 1;

static UNSUPPORTED_SOCKET_OPTION_WARNING: Once = Once::new();

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub const DEFAULT_DATA_STREAMS: usize = 4;

#[derive(Debug)]
pub struct ControlPlaneReport {
    pub worker_count: usize,
    pub data_stream_count: usize,
    pub manifest_entries: u64,
    pub manifest_file_bytes: u64,
    pub manifest_wire_bytes: u64,
    pub scan_elapsed: Duration,
    pub connect_elapsed: Duration,
    pub manifest_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl ControlPlaneReport {
    pub fn print(&self) {
        let manifest_seconds = self.manifest_elapsed.as_secs_f64();

        let wire_megabytes_per_second = if manifest_seconds == 0.0 {
            0.0
        } else {
            self.manifest_wire_bytes as f64 / 1_000_000.0 / manifest_seconds
        };

        let entries_per_second = if manifest_seconds == 0.0 {
            0.0
        } else {
            self.manifest_entries as f64 / manifest_seconds
        };

        println!("TCP control-plane probe complete");
        println!("  Scanner workers:      {}", self.worker_count);
        println!("  TCP data streams:     {}", self.data_stream_count);
        println!(
            "  Manifest entries:     {}",
            format_bytes(self.manifest_entries)
        );
        println!(
            "  Represented data:     {} bytes",
            format_bytes(self.manifest_file_bytes)
        );
        println!(
            "  Manifest wire size:   {} bytes",
            format_bytes(self.manifest_wire_bytes)
        );
        println!(
            "  Scan time:            {:.6} s",
            self.scan_elapsed.as_secs_f64()
        );
        println!(
            "  Connection time:      {:.6} s",
            self.connect_elapsed.as_secs_f64()
        );
        println!("  Manifest time:        {:.6} s", manifest_seconds);
        println!(
            "  Total time:           {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Manifest throughput:  {:.2} MB/s",
            wire_megabytes_per_second
        );
        println!(
            "  Manifest entry rate:  {:.0} entries/s",
            entries_per_second
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionRole {
    Control,
    Data,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Handshake {
    pub(crate) role: ConnectionRole,
    pub(crate) session_id: u64,
    pub(crate) stream_id: u32,
    pub(crate) stream_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManifestSummary {
    pub(crate) entries: u64,
    pub(crate) total_file_bytes: u64,
    pub(crate) fingerprint: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManifestAck {
    summary: ManifestSummary,
    manifest_wire_bytes: u64,
    data_stream_count: u32,
}

pub fn validate_data_stream_count(data_stream_count: usize) -> io::Result<()> {
    if !(1..=MAX_DATA_STREAMS).contains(&data_stream_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "data stream count must be between 1 and \
                 {MAX_DATA_STREAMS}"
            ),
        ));
    }

    Ok(())
}

pub fn run(
    root: &Path,
    worker_count: usize,
    data_stream_count: usize,
) -> io::Result<ControlPlaneReport> {
    manifest_scan::validate_worker_count(worker_count)?;
    validate_data_stream_count(data_stream_count)?;

    let total_started = Instant::now();

    let scan_result = manifest_scan::run(root, worker_count)?;

    let scan_elapsed = scan_result.report.elapsed;
    let expected_summary = summarize_manifest(&scan_result.manifest)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;

    let address = listener.local_addr()?;
    let session_id = create_session_id();

    let server = thread::Builder::new()
        .name("networkcopy-control-server".to_string())
        .spawn(move || run_server(listener, session_id, data_stream_count))?;

    let connect_started = Instant::now();

    let mut control_stream = connect_stream(address)?;

    write_handshake(
        &mut control_stream,
        Handshake {
            role: ConnectionRole::Control,
            session_id,
            stream_id: 0,
            stream_count: data_stream_count as u32,
        },
    )?;

    let mut data_streams = Vec::with_capacity(data_stream_count);

    for stream_id in 0..data_stream_count {
        let mut stream = connect_stream(address)?;

        write_handshake(
            &mut stream,
            Handshake {
                role: ConnectionRole::Data,
                session_id,
                stream_id: stream_id as u32,
                stream_count: data_stream_count as u32,
            },
        )?;

        data_streams.push(stream);
    }

    let connect_elapsed = connect_started.elapsed();
    let manifest_started = Instant::now();

    let manifest_wire_bytes = send_manifest(&mut control_stream, &scan_result.manifest)?;

    let client_ack = read_manifest_ack(&mut control_stream)?;

    let manifest_elapsed = manifest_started.elapsed();

    drop(data_streams);
    drop(control_stream);

    let server_ack = server
        .join()
        .map_err(|_| io::Error::other("TCP control server thread panicked"))??;

    if client_ack != server_ack {
        return Err(io::Error::other(
            "client and server disagree about the manifest acknowledgement",
        ));
    }

    if client_ack.summary != expected_summary {
        return Err(io::Error::other(
            "receiver manifest summary differs from sender manifest",
        ));
    }

    if client_ack.manifest_wire_bytes != manifest_wire_bytes {
        return Err(io::Error::other(format!(
            "manifest wire byte count differs: sender counted \
             {manifest_wire_bytes}, receiver counted {}",
            client_ack.manifest_wire_bytes
        )));
    }

    if client_ack.data_stream_count != data_stream_count as u32 {
        return Err(io::Error::other(format!(
            "receiver registered {} data streams instead of \
             {data_stream_count}",
            client_ack.data_stream_count
        )));
    }

    Ok(ControlPlaneReport {
        worker_count,
        data_stream_count,
        manifest_entries: expected_summary.entries,
        manifest_file_bytes: expected_summary.total_file_bytes,
        manifest_wire_bytes,
        scan_elapsed,
        connect_elapsed,
        manifest_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

fn run_server(
    listener: TcpListener,
    session_id: u64,
    expected_data_streams: usize,
) -> io::Result<ManifestAck> {
    let expected_connections = expected_data_streams
        .checked_add(1)
        .ok_or_else(|| io::Error::other("expected TCP connection count overflowed"))?;

    let mut control_stream: Option<TcpStream> = None;
    let mut data_streams = Vec::with_capacity(expected_data_streams);
    let mut data_stream_ids = HashSet::with_capacity(expected_data_streams);

    for _ in 0..expected_connections {
        let (mut stream, _) = listener.accept()?;
        configure_stream(&stream)?;

        let handshake = read_handshake(&mut stream)?;

        if handshake.session_id != session_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection used an unexpected session ID",
            ));
        }

        if handshake.stream_count != expected_data_streams as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "connection announced {} streams, expected \
                     {expected_data_streams}",
                    handshake.stream_count
                ),
            ));
        }

        match handshake.role {
            ConnectionRole::Control => {
                if handshake.stream_id != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control connection used a nonzero stream ID",
                    ));
                }

                if control_stream.replace(stream).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "session opened more than one control connection",
                    ));
                }
            }

            ConnectionRole::Data => {
                if handshake.stream_id >= expected_data_streams as u32 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "data stream ID {} is outside the \
                             negotiated range",
                            handshake.stream_id
                        ),
                    ));
                }

                if !data_stream_ids.insert(handshake.stream_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("duplicate data stream ID {}", handshake.stream_id),
                    ));
                }

                data_streams.push(stream);
            }
        }
    }

    if data_streams.len() != expected_data_streams {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "registered {} data streams, expected \
                 {expected_data_streams}",
                data_streams.len()
            ),
        ));
    }

    let mut control_stream = control_stream.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "session did not open a control connection",
        )
    })?;

    let (summary, manifest_wire_bytes) = receive_manifest(&mut control_stream)?;

    let ack = ManifestAck {
        summary,
        manifest_wire_bytes,
        data_stream_count: data_streams.len() as u32,
    };

    write_manifest_ack(&mut control_stream, ack)?;

    Ok(ack)
}

fn connect_stream(address: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(address)?;
    configure_stream(&stream)?;
    Ok(stream)
}

pub(crate) fn configure_stream(stream: &TcpStream) -> io::Result<()> {
    apply_socket_setting(stream.set_nodelay(true), "TCP_NODELAY")?;

    apply_socket_setting(stream.set_read_timeout(Some(SOCKET_TIMEOUT)), "SO_RCVTIMEO")?;

    apply_socket_setting(
        stream.set_write_timeout(Some(SOCKET_TIMEOUT)),
        "SO_SNDTIMEO",
    )?;

    Ok(())
}

fn apply_socket_setting(result: io::Result<()>, option: &str) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),

        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_FUNCTION) => {
            UNSUPPORTED_SOCKET_OPTION_WARNING
                .call_once(|| {
                    eprintln!(
                        "warning: {option} is unsupported by this Windows network environment; continuing without unsupported socket tuning"
                    );
                });

            Ok(())
        }

        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("failed to configure {option}: {error}"),
        )),
    }
}

pub(crate) fn create_session_id() -> u64 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    timestamp.rotate_left(17) ^ u64::from(process::id())
}

pub(crate) fn write_handshake(writer: &mut impl Write, handshake: Handshake) -> io::Result<()> {
    writer.write_all(&PROTOCOL_MAGIC)?;
    write_u16(writer, PROTOCOL_VERSION)?;

    let role = match handshake.role {
        ConnectionRole::Control => ROLE_CONTROL,
        ConnectionRole::Data => ROLE_DATA,
    };

    write_u8(writer, role)?;
    write_u8(writer, 0)?;
    write_u64(writer, handshake.session_id)?;
    write_u32(writer, handshake.stream_id)?;
    write_u32(writer, handshake.stream_count)?;
    writer.flush()
}

pub(crate) fn read_handshake(reader: &mut impl Read) -> io::Result<Handshake> {
    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;

    if magic != PROTOCOL_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "connection used an invalid protocol magic",
        ));
    }

    let version = read_u16(reader)?;

    if version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported NetworkCopy wire protocol version {version}; this build requires version {PROTOCOL_VERSION}"
            ),
        ));
    }

    let role = match read_u8(reader)? {
        ROLE_CONTROL => ConnectionRole::Control,
        ROLE_DATA => ConnectionRole::Data,

        unknown => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown connection role {unknown}"),
            ));
        }
    };

    let reserved = read_u8(reader)?;

    if reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handshake reserved byte was not zero",
        ));
    }

    Ok(Handshake {
        role,
        session_id: read_u64(reader)?,
        stream_id: read_u32(reader)?,
        stream_count: read_u32(reader)?,
    })
}

pub(crate) fn send_manifest(stream: &mut TcpStream, manifest: &[ManifestEntry]) -> io::Result<u64> {
    let expected_summary = summarize_manifest(manifest)?;

    let buffered = BufWriter::with_capacity(CONTROL_BUFFER_BYTES, stream);

    let mut writer = CountingWriter::new(buffered);

    write_u8(&mut writer, MESSAGE_MANIFEST)?;
    write_u64(&mut writer, expected_summary.entries)?;
    write_u64(&mut writer, expected_summary.total_file_bytes)?;
    write_u64(&mut writer, expected_summary.fingerprint)?;

    for entry in manifest {
        write_manifest_entry(&mut writer, entry)?;
    }

    writer.flush()?;
    Ok(writer.bytes_written())
}

pub(crate) fn receive_manifest_entries(
    stream: &mut TcpStream,
) -> io::Result<(Vec<ManifestEntry>, ManifestSummary, u64)> {
    let buffered = BufReader::with_capacity(CONTROL_BUFFER_BYTES, stream);

    let mut reader = CountingReader::new(buffered);

    let message_type = read_u8(&mut reader)?;

    if message_type != MESSAGE_MANIFEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected manifest message, received type \
                 0x{message_type:02X}"
            ),
        ));
    }

    let claimed_summary = ManifestSummary {
        entries: read_u64(&mut reader)?,
        total_file_bytes: read_u64(&mut reader)?,
        fingerprint: read_u64(&mut reader)?,
    };

    let mut actual_summary = ManifestSummary {
        entries: 0,
        total_file_bytes: 0,
        fingerprint: FNV_OFFSET_BASIS,
    };

    let mut manifest = Vec::new();

    for _ in 0..claimed_summary.entries {
        let entry = read_manifest_entry(&mut reader)?;

        actual_summary.entries = actual_summary
            .entries
            .checked_add(1)
            .ok_or_else(|| io::Error::other("received manifest entry count overflowed"))?;

        actual_summary.total_file_bytes = actual_summary
            .total_file_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("received manifest byte count overflowed"))?;

        actual_summary.fingerprint = hash_manifest_entry(actual_summary.fingerprint, &entry);

        manifest.push(entry);
    }

    if actual_summary != claimed_summary {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manifest summary mismatch: sender announced \
                 {claimed_summary:?}, receiver calculated \
                 {actual_summary:?}"
            ),
        ));
    }

    Ok((manifest, actual_summary, reader.bytes_read()))
}

fn receive_manifest(stream: &mut TcpStream) -> io::Result<(ManifestSummary, u64)> {
    let (_, summary, wire_bytes) = receive_manifest_entries(stream)?;

    Ok((summary, wire_bytes))
}

fn write_manifest_entry(writer: &mut impl Write, entry: &ManifestEntry) -> io::Result<()> {
    validate_relative_path(&entry.relative_path)?;

    let path_units: Vec<u16> = entry.relative_path.as_os_str().encode_wide().collect();

    if path_units.is_empty() || path_units.len() > MAX_PATH_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "manifest path has invalid UTF-16 length: {}",
                entry.relative_path.display()
            ),
        ));
    }

    write_u32(
        writer,
        u32::try_from(path_units.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "manifest path is too long")
        })?,
    )?;

    for unit in path_units {
        write_u16(writer, unit)?;
    }

    write_u64(writer, entry.file_size)?;
    write_u64(writer, entry.last_write_time)?;
    write_u32(writer, entry.file_attributes)?;
    write_u8(writer, class_to_wire(entry.class))
}

fn read_manifest_entry(reader: &mut impl Read) -> io::Result<ManifestEntry> {
    let path_unit_count = read_u32(reader)? as usize;

    if path_unit_count == 0 || path_unit_count > MAX_PATH_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "received invalid UTF-16 path length \
                 {path_unit_count}"
            ),
        ));
    }

    let mut path_units = Vec::with_capacity(path_unit_count);

    for _ in 0..path_unit_count {
        path_units.push(read_u16(reader)?);
    }

    let relative_path = PathBuf::from(OsString::from_wide(&path_units));

    validate_relative_path(&relative_path)?;

    let file_size = read_u64(reader)?;
    let last_write_time = read_u64(reader)?;
    let file_attributes = read_u32(reader)?;
    let class = class_from_wire(read_u8(reader)?)?;

    Ok(ManifestEntry {
        relative_path,
        file_size,
        last_write_time,
        file_attributes,
        class,
    })
}

fn validate_relative_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "manifest path is not a safe relative path: {}",
                path.display()
            ),
        ));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "manifest path contains an unsafe component: {}",
                    path.display()
                ),
            ));
        }
    }

    Ok(())
}

pub(crate) fn summarize_manifest(manifest: &[ManifestEntry]) -> io::Result<ManifestSummary> {
    let mut summary = ManifestSummary {
        entries: 0,
        total_file_bytes: 0,
        fingerprint: FNV_OFFSET_BASIS,
    };

    for entry in manifest {
        summary.entries = summary
            .entries
            .checked_add(1)
            .ok_or_else(|| io::Error::other("manifest entry count overflowed"))?;

        summary.total_file_bytes = summary
            .total_file_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("manifest file byte count overflowed"))?;

        summary.fingerprint = hash_manifest_entry(summary.fingerprint, entry);
    }

    Ok(summary)
}

fn hash_manifest_entry(mut hash: u64, entry: &ManifestEntry) -> u64 {
    for unit in entry.relative_path.as_os_str().encode_wide() {
        hash = hash_bytes(hash, &unit.to_be_bytes());
    }

    hash = hash_bytes(hash, &entry.file_size.to_be_bytes());
    hash = hash_bytes(hash, &entry.last_write_time.to_be_bytes());
    hash = hash_bytes(hash, &entry.file_attributes.to_be_bytes());
    hash_bytes(hash, &[class_to_wire(entry.class)])
}

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    hash
}

fn class_to_wire(class: FileClass) -> u8 {
    match class {
        FileClass::Tiny => 1,
        FileClass::Medium => 2,
        FileClass::Large => 3,
    }
}

fn class_from_wire(value: u8) -> io::Result<FileClass> {
    match value {
        1 => Ok(FileClass::Tiny),
        2 => Ok(FileClass::Medium),
        3 => Ok(FileClass::Large),

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("received unknown file class {unknown}"),
        )),
    }
}

fn write_manifest_ack(writer: &mut impl Write, ack: ManifestAck) -> io::Result<()> {
    write_u8(writer, MESSAGE_MANIFEST_ACK)?;
    write_u64(writer, ack.summary.entries)?;
    write_u64(writer, ack.summary.total_file_bytes)?;
    write_u64(writer, ack.summary.fingerprint)?;
    write_u64(writer, ack.manifest_wire_bytes)?;
    write_u32(writer, ack.data_stream_count)?;
    writer.flush()
}

fn read_manifest_ack(reader: &mut impl Read) -> io::Result<ManifestAck> {
    let message_type = read_u8(reader)?;

    if message_type != MESSAGE_MANIFEST_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected manifest acknowledgement, received \
                 type 0x{message_type:02X}"
            ),
        ));
    }

    Ok(ManifestAck {
        summary: ManifestSummary {
            entries: read_u64(reader)?,
            total_file_bytes: read_u64(reader)?,
            fingerprint: read_u64(reader)?,
        },
        manifest_wire_bytes: read_u64(reader)?,
        data_stream_count: read_u32(reader)?,
    })
}

#[derive(Debug)]
struct CountingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;

        self.bytes_written = self
            .bytes_written
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("manifest wire byte count overflowed"))?;

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
struct CountingReader<R> {
    inner: R,
    bytes_read: u64,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            bytes_read: 0,
        }
    }

    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;

        self.bytes_read = self
            .bytes_read
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("manifest wire read count overflowed"))?;

        Ok(read)
    }
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn write_u16(writer: &mut impl Write, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectionRole, ERROR_INVALID_FUNCTION, Handshake, PROTOCOL_MAGIC, PROTOCOL_VERSION,
        apply_socket_setting, read_handshake, run, validate_data_stream_count, write_handshake,
    };
    use std::env;
    use std::fs;
    use std::io::{self, Cursor};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn unsupported_socket_setting_is_tolerated() {
        let unsupported = io::Error::from_raw_os_error(ERROR_INVALID_FUNCTION);

        assert!(apply_socket_setting(Err(unsupported), "SO_RCVTIMEO",).is_ok());

        let real_failure = io::Error::from_raw_os_error(10022);

        let error = apply_socket_setting(Err(real_failure), "SO_RCVTIMEO").unwrap_err();

        assert!(error.to_string().contains("SO_RCVTIMEO",));
    }

    #[test]
    fn handshake_round_trips() {
        let expected = Handshake {
            role: ConnectionRole::Data,
            session_id: 0x1234_5678_9ABC_DEF0,
            stream_id: 3,
            stream_count: 8,
        };

        let mut bytes = Vec::new();
        write_handshake(&mut bytes, expected).unwrap();

        let mut cursor = Cursor::new(bytes);
        let actual = read_handshake(&mut cursor).unwrap();

        assert_eq!(actual, expected);
        assert_eq!(PROTOCOL_VERSION, 10);
    }

    #[test]
    fn previous_wire_protocol_is_rejected_cleanly() {
        let mut bytes = Vec::new();

        bytes.extend_from_slice(&PROTOCOL_MAGIC);

        bytes.extend_from_slice(&9_u16.to_be_bytes());

        let error = read_handshake(&mut Cursor::new(bytes)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let message = error.to_string();

        assert!(message.contains("version 9"),);

        assert!(message.contains("requires version 10"),);
    }

    #[test]
    fn validates_data_stream_counts() {
        assert!(validate_data_stream_count(0).is_err());
        assert!(validate_data_stream_count(1).is_ok());
        assert!(validate_data_stream_count(32).is_ok());
        assert!(validate_data_stream_count(33).is_err());
    }

    #[test]
    fn loopback_session_transfers_manifest() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let root = env::temp_dir().join(format!("networkcopy-control-{}-{unique}", process::id()));

        let nested = root.join("árvíztűrő");

        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("root.txt"), b"root").unwrap();
        fs::write(nested.join("tükörfúrógép.bin"), vec![0xA5_u8; 300 * 1024]).unwrap();

        let session_result = run(&root, 2, 3);
        let cleanup_result = fs::remove_dir_all(&root);

        let report = session_result.unwrap();
        cleanup_result.unwrap();

        assert_eq!(report.manifest_entries, 2);
        assert_eq!(report.data_stream_count, 3);
        assert!(report.manifest_wire_bytes > 0);
    }
}
