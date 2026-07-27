use crate::control_plane::{self, ConnectionRole, Handshake, ManifestSummary};
use crate::copy_bench::{binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes};
use crate::manifest_scan::{self, ManifestEntry};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MESSAGE_RECEIVER_READY: u8 = 0x30;
const MESSAGE_FILE: u8 = 0x31;
const MESSAGE_STREAM_END: u8 = 0x32;
const MESSAGE_TRANSFER_ACK: u8 = 0x33;

const NETWORK_BUFFER_BYTES: usize = 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub const DEFAULT_DATA_STREAMS: usize = 4;

#[derive(Debug)]
pub struct MultistreamCopyReport {
    pub worker_count: usize,
    pub data_stream_count: usize,
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub manifest_wire_bytes: u64,
    pub scan_elapsed: Duration,
    pub connection_elapsed: Duration,
    pub manifest_elapsed: Duration,
    pub data_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl MultistreamCopyReport {
    pub fn print(&self) {
        println!("Multistream TCP folder copy complete");
        println!("  Scanner workers:      {}", self.worker_count);
        println!("  TCP data streams:     {}", self.data_stream_count);
        println!(
            "  Files copied:         {}",
            format_bytes(self.files_copied)
        );
        println!(
            "  Data copied:          {} bytes",
            format_bytes(self.bytes_copied)
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
            self.connection_elapsed.as_secs_f64()
        );
        println!(
            "  Manifest time:        {:.6} s",
            self.manifest_elapsed.as_secs_f64()
        );
        println!(
            "  Data transfer time:   {:.6} s",
            self.data_elapsed.as_secs_f64()
        );
        println!(
            "  Total time:           {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Payload throughput:   {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_copied, self.data_elapsed,),
            binary_mebibytes_per_second(self.bytes_copied, self.data_elapsed,)
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TransferAck {
    files_copied: u64,
    bytes_copied: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct LaneReport {
    files_copied: u64,
    bytes_copied: u64,
}

pub fn run(
    source_root: &Path,
    destination_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
) -> io::Result<MultistreamCopyReport> {
    manifest_scan::validate_worker_count(worker_count)?;
    control_plane::validate_data_stream_count(data_stream_count)?;

    if destination_root.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "destination must not already exist: {}",
                destination_root.display()
            ),
        ));
    }

    let total_started = Instant::now();
    let source_root = source_root.canonicalize()?;

    let scan_result = manifest_scan::run(&source_root, worker_count)?;

    let scan_elapsed = scan_result.report.elapsed;
    let manifest = Arc::new(scan_result.manifest);
    let summary = control_plane::summarize_manifest(&manifest)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let session_id = control_plane::create_session_id();

    let server_destination = destination_root.to_path_buf();

    let server = thread::Builder::new()
        .name("networkcopy-transfer-server".to_string())
        .spawn(move || run_server(listener, session_id, data_stream_count, server_destination))?;

    let connection_started = Instant::now();

    let mut control_stream = TcpStream::connect(address)?;
    control_plane::configure_stream(&control_stream)?;

    control_plane::write_handshake(
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
        let mut stream = TcpStream::connect(address)?;
        control_plane::configure_stream(&stream)?;

        control_plane::write_handshake(
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

    let connection_elapsed = connection_started.elapsed();
    let manifest_started = Instant::now();

    let manifest_wire_bytes = control_plane::send_manifest(&mut control_stream, &manifest)?;

    let receiver_summary = read_receiver_ready(&mut control_stream)?;

    let manifest_elapsed = manifest_started.elapsed();

    if receiver_summary != summary {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver acknowledged a different manifest",
        ));
    }

    let assignments = build_assignments(&manifest, data_stream_count)?;

    let data_started = Instant::now();
    let source_root = Arc::new(source_root);

    let sender_reports = thread::scope(|scope| -> io::Result<Vec<LaneReport>> {
        let mut handles = Vec::with_capacity(data_stream_count);

        for (stream, file_ids) in data_streams.into_iter().zip(assignments) {
            let lane_root = Arc::clone(&source_root);
            let lane_manifest = Arc::clone(&manifest);

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-data-sender".to_string())
                    .spawn_scoped(scope, move || {
                        send_lane(stream, lane_root.as_path(), &lane_manifest, &file_ids)
                    })?,
            );
        }

        join_lane_threads(handles)
    })?;

    let sender_report = merge_lane_reports(sender_reports)?;

    let transfer_ack = read_transfer_ack(&mut control_stream)?;

    let data_elapsed = data_started.elapsed();

    drop(control_stream);

    let server_ack = server
        .join()
        .map_err(|_| io::Error::other("multistream receiver thread panicked"))??;

    if transfer_ack != server_ack {
        return Err(io::Error::other(
            "client and server transfer acknowledgements differ",
        ));
    }

    if transfer_ack.files_copied != summary.entries
        || transfer_ack.bytes_copied != summary.total_file_bytes
    {
        return Err(io::Error::other(
            "receiver did not copy the complete manifest",
        ));
    }

    if sender_report.files_copied != transfer_ack.files_copied
        || sender_report.bytes_copied != transfer_ack.bytes_copied
    {
        return Err(io::Error::other("sender and receiver byte counts differ"));
    }

    Ok(MultistreamCopyReport {
        worker_count,
        data_stream_count,
        files_copied: transfer_ack.files_copied,
        bytes_copied: transfer_ack.bytes_copied,
        manifest_wire_bytes,
        scan_elapsed,
        connection_elapsed,
        manifest_elapsed,
        data_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

fn run_server(
    listener: TcpListener,
    session_id: u64,
    data_stream_count: usize,
    destination_root: PathBuf,
) -> io::Result<TransferAck> {
    let (mut control_stream, data_streams) =
        accept_session(listener, session_id, data_stream_count)?;

    let (manifest, summary, _) = control_plane::receive_manifest_entries(&mut control_stream)?;

    prepare_destination(&destination_root, &manifest)?;

    write_receiver_ready(&mut control_stream, summary)?;

    let manifest = Arc::new(manifest);
    let destination_root = Arc::new(destination_root);

    let received: Arc<Vec<AtomicBool>> = Arc::new(
        (0..manifest.len())
            .map(|_| AtomicBool::new(false))
            .collect(),
    );

    let lane_reports = thread::scope(|scope| -> io::Result<Vec<LaneReport>> {
        let mut handles = Vec::with_capacity(data_stream_count);

        for stream in data_streams {
            let lane_manifest = Arc::clone(&manifest);
            let lane_destination = Arc::clone(&destination_root);
            let lane_received = Arc::clone(&received);

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-data-receiver".to_string())
                    .spawn_scoped(scope, move || {
                        receive_lane(
                            stream,
                            lane_destination.as_path(),
                            &lane_manifest,
                            &lane_received,
                        )
                    })?,
            );
        }

        join_lane_threads(handles)
    })?;

    let report = merge_lane_reports(lane_reports)?;

    if received
        .iter()
        .any(|received| !received.load(Ordering::Acquire))
    {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "one or more manifest files were not received",
        ));
    }

    let ack = TransferAck {
        files_copied: report.files_copied,
        bytes_copied: report.bytes_copied,
    };

    if ack.files_copied != summary.entries || ack.bytes_copied != summary.total_file_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "received payload does not match the manifest summary",
        ));
    }

    write_transfer_ack(&mut control_stream, ack)?;
    Ok(ack)
}

fn accept_session(
    listener: TcpListener,
    session_id: u64,
    data_stream_count: usize,
) -> io::Result<(TcpStream, Vec<TcpStream>)> {
    let expected_connections = data_stream_count
        .checked_add(1)
        .ok_or_else(|| io::Error::other("connection count overflowed"))?;

    let mut control_stream = None;

    let mut data_streams: Vec<Option<TcpStream>> = std::iter::repeat_with(|| None)
        .take(data_stream_count)
        .collect();

    for _ in 0..expected_connections {
        let (mut stream, _) = listener.accept()?;
        control_plane::configure_stream(&stream)?;

        let handshake = control_plane::read_handshake(&mut stream)?;

        if handshake.session_id != session_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection used an incorrect session ID",
            ));
        }

        if handshake.stream_count != data_stream_count as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection used an incorrect stream count",
            ));
        }

        match handshake.role {
            ConnectionRole::Control => {
                if handshake.stream_id != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control stream used a nonzero ID",
                    ));
                }

                if control_stream.replace(stream).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "duplicate control connection",
                    ));
                }
            }

            ConnectionRole::Data => {
                let stream_id = usize::try_from(handshake.stream_id).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "data stream ID cannot be represented",
                    )
                })?;

                let slot = data_streams.get_mut(stream_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "data stream ID is outside the negotiated range",
                    )
                })?;

                if slot.replace(stream).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "duplicate data stream ID",
                    ));
                }
            }
        }
    }

    let control_stream = control_stream.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "control connection was not established",
        )
    })?;

    let mut ordered_streams = Vec::with_capacity(data_stream_count);

    for (stream_id, stream) in data_streams.into_iter().enumerate() {
        ordered_streams.push(stream.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("data stream {stream_id} was not established"),
            )
        })?);
    }

    Ok((control_stream, ordered_streams))
}

fn prepare_destination(destination_root: &Path, manifest: &[ManifestEntry]) -> io::Result<()> {
    if destination_root.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("destination already exists: {}", destination_root.display()),
        ));
    }

    fs::create_dir_all(destination_root)?;

    for entry in manifest {
        if let Some(parent) = entry.relative_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(destination_root.join(parent))?;
            }
        }
    }

    Ok(())
}

fn build_assignments(
    manifest: &[ManifestEntry],
    data_stream_count: usize,
) -> io::Result<Vec<Vec<usize>>> {
    let mut file_ids: Vec<usize> = (0..manifest.len()).collect();

    file_ids
        .sort_unstable_by(|left, right| manifest[*right].file_size.cmp(&manifest[*left].file_size));

    let mut assignments = vec![Vec::new(); data_stream_count];
    let mut assigned_bytes = vec![0_u64; data_stream_count];

    for file_id in file_ids {
        let lane = assigned_bytes
            .iter()
            .enumerate()
            .min_by_key(|(_, bytes)| **bytes)
            .map(|(lane, _)| lane)
            .ok_or_else(|| io::Error::other("no TCP data lanes are available"))?;

        assignments[lane].push(file_id);

        assigned_bytes[lane] = assigned_bytes[lane]
            .checked_add(manifest[file_id].file_size)
            .ok_or_else(|| io::Error::other("data-lane assignment size overflowed"))?;
    }

    Ok(assignments)
}

fn send_lane(
    stream: TcpStream,
    source_root: &Path,
    manifest: &[ManifestEntry],
    file_ids: &[usize],
) -> io::Result<LaneReport> {
    let mut writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, stream);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut report = LaneReport::default();

    for &file_id in file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "scheduler returned an invalid file ID",
            )
        })?;

        write_u8(&mut writer, MESSAGE_FILE)?;
        write_u64(&mut writer, file_id as u64)?;
        write_u64(&mut writer, entry.file_size)?;

        let path = source_root.join(&entry.relative_path);
        let mut file = File::open(&path)?;

        let current_size = file.metadata()?.len();

        if current_size != entry.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("source changed after scanning: {}", path.display()),
            ));
        }

        copy_exact(&mut file, &mut writer, entry.file_size, &mut buffer)?;

        report.files_copied = report
            .files_copied
            .checked_add(1)
            .ok_or_else(|| io::Error::other("sender file count overflowed"))?;

        report.bytes_copied = report
            .bytes_copied
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("sender byte count overflowed"))?;
    }

    write_u8(&mut writer, MESSAGE_STREAM_END)?;
    writer.flush()?;

    Ok(report)
}

fn receive_lane(
    stream: TcpStream,
    destination_root: &Path,
    manifest: &[ManifestEntry],
    received: &[AtomicBool],
) -> io::Result<LaneReport> {
    let mut reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, stream);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut report = LaneReport::default();

    loop {
        match read_u8(&mut reader)? {
            MESSAGE_FILE => {
                let file_id_u64 = read_u64(&mut reader)?;

                let file_id = usize::try_from(file_id_u64).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "received file ID is too large")
                })?;

                let announced_size = read_u64(&mut reader)?;

                let entry = manifest.get(file_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("received unknown file ID {file_id}"),
                    )
                })?;

                if announced_size != entry.file_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "file {file_id} announced {announced_size} \
                             bytes but manifest expects {}",
                            entry.file_size
                        ),
                    ));
                }

                if received[file_id].swap(true, Ordering::AcqRel) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("file {file_id} was received twice"),
                    ));
                }

                receive_file(&mut reader, destination_root, file_id, entry, &mut buffer)?;

                report.files_copied = report
                    .files_copied
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("receiver file count overflowed"))?;

                report.bytes_copied = report
                    .bytes_copied
                    .checked_add(entry.file_size)
                    .ok_or_else(|| io::Error::other("receiver byte count overflowed"))?;
            }

            MESSAGE_STREAM_END => break,

            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "received unknown data-lane message \
                         0x{unknown:02X}"
                    ),
                ));
            }
        }
    }

    Ok(report)
}

fn receive_file(
    reader: &mut impl Read,
    destination_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
    buffer: &mut [u8],
) -> io::Result<()> {
    let final_path = destination_root.join(&entry.relative_path);

    let temporary_path = temporary_path(&final_path, file_id);

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)?;

    file.set_len(entry.file_size)?;

    let copy_result = copy_exact(reader, &mut file, entry.file_size, buffer);

    if let Err(error) = copy_result {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    file.flush()?;
    drop(file);

    fs::rename(temporary_path, final_path)
}

fn temporary_path(final_path: &Path, file_id: usize) -> PathBuf {
    let mut temporary = OsString::from(final_path.as_os_str());

    temporary.push(format!(".ncs-part-{file_id}"));
    PathBuf::from(temporary)
}

fn copy_exact(
    reader: &mut impl Read,
    writer: &mut impl Write,
    byte_count: u64,
    buffer: &mut [u8],
) -> io::Result<()> {
    let mut remaining = byte_count;

    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;

        let read = reader.read(&mut buffer[..requested])?;

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("stream ended with {remaining} bytes remaining"),
            ));
        }

        writer.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }

    Ok(())
}

fn join_lane_threads<T>(
    handles: Vec<thread::ScopedJoinHandle<'_, io::Result<T>>>,
) -> io::Result<Vec<T>> {
    let mut results = Vec::with_capacity(handles.len());
    let mut first_error = None;

    for handle in handles {
        match handle.join() {
            Ok(Ok(result)) => results.push(result),

            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }

            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(io::Error::other("TCP data-lane thread panicked"));
                }
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(results)
}

fn merge_lane_reports(reports: Vec<LaneReport>) -> io::Result<LaneReport> {
    let mut merged = LaneReport::default();

    for report in reports {
        merged.files_copied = merged
            .files_copied
            .checked_add(report.files_copied)
            .ok_or_else(|| io::Error::other("merged file count overflowed"))?;

        merged.bytes_copied = merged
            .bytes_copied
            .checked_add(report.bytes_copied)
            .ok_or_else(|| io::Error::other("merged byte count overflowed"))?;
    }

    Ok(merged)
}

fn write_receiver_ready(writer: &mut impl Write, summary: ManifestSummary) -> io::Result<()> {
    write_u8(writer, MESSAGE_RECEIVER_READY)?;
    write_u64(writer, summary.entries)?;
    write_u64(writer, summary.total_file_bytes)?;
    write_u64(writer, summary.fingerprint)?;
    writer.flush()
}

fn read_receiver_ready(reader: &mut impl Read) -> io::Result<ManifestSummary> {
    let message = read_u8(reader)?;

    if message != MESSAGE_RECEIVER_READY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected receiver-ready message, received \
                 0x{message:02X}"
            ),
        ));
    }

    Ok(ManifestSummary {
        entries: read_u64(reader)?,
        total_file_bytes: read_u64(reader)?,
        fingerprint: read_u64(reader)?,
    })
}

fn write_transfer_ack(writer: &mut impl Write, ack: TransferAck) -> io::Result<()> {
    write_u8(writer, MESSAGE_TRANSFER_ACK)?;
    write_u64(writer, ack.files_copied)?;
    write_u64(writer, ack.bytes_copied)?;
    writer.flush()
}

fn read_transfer_ack(reader: &mut impl Read) -> io::Result<TransferAck> {
    let message = read_u8(reader)?;

    if message != MESSAGE_TRANSFER_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected transfer acknowledgement, received \
                 0x{message:02X}"
            ),
        ));
    }

    Ok(TransferAck {
        files_copied: read_u64(reader)?,
        bytes_copied: read_u64(reader)?,
    })
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut value = [0_u8; 1];
    reader.read_exact(&mut value)?;
    Ok(value[0])
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::{build_assignments, run};
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scheduler_spreads_large_files_between_lanes() {
        let manifest = vec![
            entry("a.bin", 100),
            entry("b.bin", 90),
            entry("c.bin", 80),
            entry("d.bin", 70),
        ];

        let assignments = build_assignments(&manifest, 2).unwrap();

        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments.iter().map(Vec::len).sum::<usize>(), 4);
    }

    #[test]
    fn loopback_session_copies_complete_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-multistream-{}-{unique}",
            process::id()
        ));

        let source = parent.join("source");
        let destination = parent.join("destination");
        let nested = source.join("árvíztűrő");

        fs::create_dir_all(&nested).unwrap();
        fs::write(source.join("empty.bin"), []).unwrap();
        fs::write(source.join("small.txt"), b"NetworkCopy Speed Edition").unwrap();

        fs::write(nested.join("medium.bin"), vec![0xA5_u8; 300 * 1024]).unwrap();

        fs::write(
            nested.join("large.bin"),
            vec![0x5A_u8; 2 * 1024 * 1024 + 137],
        )
        .unwrap();

        let transfer_result = run(&source, &destination, 4, 3);

        let report = transfer_result.unwrap();

        assert_eq!(report.files_copied, 4);

        assert_eq!(
            fs::read(source.join("empty.bin")).unwrap(),
            fs::read(destination.join("empty.bin")).unwrap()
        );

        assert_eq!(
            fs::read(source.join("small.txt")).unwrap(),
            fs::read(destination.join("small.txt")).unwrap()
        );

        assert_eq!(
            fs::read(nested.join("medium.bin")).unwrap(),
            fs::read(destination.join("árvíztűrő").join("medium.bin")).unwrap()
        );

        assert_eq!(
            fs::read(nested.join("large.bin")).unwrap(),
            fs::read(destination.join("árvíztűrő").join("large.bin")).unwrap()
        );

        fs::remove_dir_all(parent).unwrap();
    }

    fn entry(path: &str, file_size: u64) -> ManifestEntry {
        ManifestEntry {
            relative_path: PathBuf::from(path),
            file_size,
            last_write_time: 0,
            file_attributes: 0,
            class: FileClass::Tiny,
        }
    }
}
