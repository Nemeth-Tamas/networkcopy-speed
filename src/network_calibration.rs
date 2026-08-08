use crate::console_progress::ProgressCounter;
use crate::control_plane;
use crate::copy_bench::{binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes};
use crate::multistream_copy;
use socket2::SockRef;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const CONTROL_MAGIC: [u8; 4] = *b"NCB1";
const DATA_MAGIC: [u8; 4] = *b"NCD1";
const ACK_MAGIC: [u8; 4] = *b"NCA1";
const PROTOCOL_VERSION: u32 = 1;
const RECEIVER_READY: u8 = 0xA1;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

pub const DEFAULT_TOTAL_MIB: u64 = 1024;
pub const DEFAULT_DATA_STREAMS: usize = 1;
pub const BUFFER_BYTES: usize = 1024 * 1024;
const MATRIX_STREAM_COUNTS: [usize; 4] = [1, 2, 4, 8];
const MATRIX_RECOMMENDATION_FRACTION: f64 = 0.90;

#[derive(Clone, Copy, Debug)]
pub struct NetworkCalibrationReport {
    pub data_stream_count: usize,
    pub total_bytes: u64,
    pub buffer_bytes_per_lane: u64,
    pub process_buffer_bytes: u64,
    pub socket_send_buffer_bytes: u64,
    pub socket_receive_buffer_bytes: u64,
    pub elapsed: Duration,
}

impl NetworkCalibrationReport {
    pub fn print(&self, direction: &str) {
        println!("Raw TCP calibration {direction} complete");

        println!("  TCP data streams:     {}", self.data_stream_count);

        println!(
            "  Payload transferred:  {} bytes",
            format_bytes(self.total_bytes,)
        );

        println!(
            "  Buffer per lane:      {} bytes",
            format_bytes(self.buffer_bytes_per_lane,)
        );

        println!(
            "  Process buffers:      {} bytes",
            format_bytes(self.process_buffer_bytes,)
        );

        println!(
            "  Socket send buffer:   {} bytes",
            format_bytes(self.socket_send_buffer_bytes,)
        );

        println!(
            "  Socket receive buffer:{} bytes",
            format_bytes(self.socket_receive_buffer_bytes,)
        );

        println!(
            "  Elapsed:              {:.6} s",
            self.elapsed.as_secs_f64()
        );

        println!(
            "  Raw throughput:       {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.total_bytes, self.elapsed,),
            binary_mebibytes_per_second(self.total_bytes, self.elapsed,)
        );

        println!(
            "  Link throughput:      {:.3} Gbit/s",
            gigabits_per_second(self.total_bytes, self.elapsed,)
        );
    }
}

#[derive(Debug)]
pub struct NetworkCalibrationMatrixReport {
    pub reports: Vec<NetworkCalibrationReport>,

    pub best: NetworkCalibrationReport,

    pub recommended: NetworkCalibrationReport,

    pub recommended_percent_of_best: f64,
}

impl NetworkCalibrationMatrixReport {
    pub fn print(&self, direction: &str) {
        println!("Raw TCP path matrix {direction} complete");

        println!(
            "  Selection policy: smallest lane count reaching {:.0}% of the best result",
            MATRIX_RECOMMENDATION_FRACTION * 100.0
        );

        println!();

        println!("  Streams        MB/s        MiB/s      Gbit/s");

        for report in &self.reports {
            println!(
                "  {:>7}  {:>10.2}  {:>11.2}  {:>10.3}",
                report.data_stream_count,
                decimal_megabytes_per_second(report.total_bytes, report.elapsed,),
                binary_mebibytes_per_second(report.total_bytes, report.elapsed,),
                gigabits_per_second(report.total_bytes, report.elapsed,)
            );
        }

        println!();

        println!(
            "  Best result:          {} streams at {:.2} MB/s",
            self.best.data_stream_count,
            decimal_megabytes_per_second(self.best.total_bytes, self.best.elapsed,)
        );

        println!(
            "  Recommended streams: {}",
            self.recommended.data_stream_count
        );

        println!(
            "  Recommended speed:   {:.2} MB/s",
            decimal_megabytes_per_second(self.recommended.total_bytes, self.recommended.elapsed,)
        );

        println!(
            "  Percent of best:      {:.2}%",
            self.recommended_percent_of_best
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct CalibrationConfig {
    session_id: u64,
    data_stream_count: usize,
    total_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketBufferSizes {
    send_bytes: usize,
    receive_bytes: usize,
}

pub(crate) fn validate_matrix_stream_count(data_stream_count: usize) -> io::Result<()> {
    if MATRIX_STREAM_COUNTS.contains(&data_stream_count) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "calibrated transfer stream count {data_stream_count} is not present in the calibration matrix {MATRIX_STREAM_COUNTS:?}"
        ),
    ))
}

pub(crate) fn socket_buffer_bytes_from_kib(buffer_kib: u64) -> io::Result<usize> {
    if buffer_kib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket buffer size must not be zero",
        ));
    }

    let bytes = buffer_kib.checked_mul(KIB).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket buffer size overflowed")
    })?;

    usize::try_from(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket buffer size cannot be represented",
        )
    })
}

pub fn bytes_from_mib(total_mib: u64) -> io::Result<u64> {
    let total_bytes = total_mib.checked_mul(MIB).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration byte count overflowed",
        )
    })?;

    if total_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration size must not be zero",
        ));
    }

    if total_bytes > MAX_TOTAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("network calibration size exceeds the {MAX_TOTAL_BYTES}-byte limit"),
        ));
    }

    Ok(total_bytes)
}

pub fn send(
    receiver_address: SocketAddr,
    total_bytes: u64,
    data_stream_count: usize,
) -> io::Result<NetworkCalibrationReport> {
    send_one(receiver_address, total_bytes, data_stream_count, None, None)
}

pub(crate) fn send_with_socket_send_buffer(
    receiver_address: SocketAddr,
    total_bytes: u64,
    data_stream_count: usize,
    socket_send_buffer_bytes: usize,
) -> io::Result<NetworkCalibrationReport> {
    if socket_send_buffer_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket send buffer size must not be zero",
        ));
    }

    send_one(
        receiver_address,
        total_bytes,
        data_stream_count,
        None,
        Some(socket_send_buffer_bytes),
    )
}

fn send_one(
    receiver_address: SocketAddr,
    total_bytes: u64,
    data_stream_count: usize,
    progress: Option<ProgressCounter>,
    socket_send_buffer_bytes: Option<usize>,
) -> io::Result<NetworkCalibrationReport> {
    validate_config(total_bytes, data_stream_count)?;

    let session_id = control_plane::create_session_id();

    let config = CalibrationConfig {
        session_id,
        data_stream_count,
        total_bytes,
    };

    let mut control_stream = multistream_copy::connect_with_retry(receiver_address)?;

    control_plane::configure_stream(&control_stream)?;

    write_control_header(&mut control_stream, config)?;

    let mut data_streams = Vec::with_capacity(data_stream_count);

    for stream_id in 0..data_stream_count {
        let mut stream = multistream_copy::connect_with_retry(receiver_address)?;

        control_plane::configure_stream(&stream)?;

        if let Some(buffer_bytes) = socket_send_buffer_bytes {
            SockRef::from(&stream).set_send_buffer_size(buffer_bytes)?;
        }

        let bytes = lane_bytes(total_bytes, data_stream_count, stream_id)?;

        write_data_header(&mut stream, config, stream_id, bytes)?;

        data_streams.push(stream);
    }

    let socket_buffers = socket_buffer_sizes(
        data_streams
            .first()
            .ok_or_else(|| io::Error::other("network calibration established no data streams"))?,
    )?;

    read_receiver_ready(&mut control_stream)?;

    let payload = Arc::new(build_payload_buffer());

    let started = Instant::now();

    let sent_bytes = thread::scope(|scope| -> io::Result<u64> {
        let mut handles = Vec::with_capacity(data_stream_count);

        for (stream_id, stream) in data_streams.into_iter().enumerate() {
            let bytes = lane_bytes(total_bytes, data_stream_count, stream_id)?;

            let payload = Arc::clone(&payload);

            let lane_progress = progress.clone();

            handles.push(scope.spawn(move || send_lane(stream, payload, bytes, lane_progress)));
        }

        join_lane_threads(handles, "network calibration sender lane panicked")
    })?;

    let acknowledged_bytes = read_ack(&mut control_stream)?;

    let elapsed = started.elapsed();

    if sent_bytes != total_bytes {
        return Err(io::Error::other(
            "network calibration sender did not send the requested byte count",
        ));
    }

    if acknowledged_bytes != total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "network calibration receiver acknowledged {acknowledged_bytes} bytes instead of {total_bytes}"
            ),
        ));
    }

    build_report(data_stream_count, total_bytes, socket_buffers, elapsed)
}

pub fn send_matrix(
    receiver_address: SocketAddr,
    total_bytes: u64,
) -> io::Result<NetworkCalibrationMatrixReport> {
    send_matrix_internal(receiver_address, total_bytes, None)
}

pub(crate) fn send_matrix_with_progress(
    receiver_address: SocketAddr,
    total_bytes: u64,
    progress: ProgressCounter,
) -> io::Result<NetworkCalibrationMatrixReport> {
    prepare_matrix_progress(&progress, total_bytes)?;

    send_matrix_internal(receiver_address, total_bytes, Some(progress))
}

fn send_matrix_internal(
    receiver_address: SocketAddr,
    total_bytes: u64,
    progress: Option<ProgressCounter>,
) -> io::Result<NetworkCalibrationMatrixReport> {
    let mut reports = Vec::with_capacity(MATRIX_STREAM_COUNTS.len());

    for data_stream_count in MATRIX_STREAM_COUNTS {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;

            progress.set_label(format!("Calibration send - {data_stream_count} streams"));
        }

        reports.push(send_one(
            receiver_address,
            total_bytes,
            data_stream_count,
            progress.clone(),
            None,
        )?);
    }

    build_matrix_report(reports)
}

pub fn receive_once(listener: TcpListener) -> io::Result<NetworkCalibrationReport> {
    receive_one(&listener, None, None)
}

pub(crate) fn receive_once_with_socket_receive_buffer(
    listener: TcpListener,
    socket_receive_buffer_bytes: usize,
) -> io::Result<NetworkCalibrationReport> {
    if socket_receive_buffer_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket receive buffer size must not be zero",
        ));
    }

    receive_one(&listener, None, Some(socket_receive_buffer_bytes))
}

pub fn receive_matrix(listener: TcpListener) -> io::Result<NetworkCalibrationMatrixReport> {
    receive_matrix_internal(&listener, None)
}

pub(crate) fn receive_matrix_on_listener_with_progress(
    listener: &TcpListener,
    progress: ProgressCounter,
) -> io::Result<NetworkCalibrationMatrixReport> {
    progress.set_completed(0);
    progress.set_total(0);

    receive_matrix_internal(listener, Some(progress))
}

fn receive_matrix_internal(
    listener: &TcpListener,
    progress: Option<ProgressCounter>,
) -> io::Result<NetworkCalibrationMatrixReport> {
    let mut reports = Vec::with_capacity(MATRIX_STREAM_COUNTS.len());

    for expected_stream_count in MATRIX_STREAM_COUNTS {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;

            progress.set_label(format!(
                "Calibration receive - {expected_stream_count} streams"
            ));
        }

        let report = receive_one(listener, progress.clone(), None)?;

        if report.data_stream_count != expected_stream_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "network calibration matrix expected {expected_stream_count} streams, received {}",
                    report.data_stream_count
                ),
            ));
        }

        reports.push(report);
    }

    build_matrix_report(reports)
}

fn accept_with_progress(
    listener: &TcpListener,
    progress: Option<&ProgressCounter>,
) -> io::Result<(TcpStream, SocketAddr)> {
    let Some(progress) = progress else {
        return listener.accept();
    };

    listener.set_nonblocking(true)?;

    let result = loop {
        if let Err(error) = progress.check_cancelled() {
            break Err(error);
        }

        match listener.accept() {
            Ok(accepted) => {
                break Ok(accepted);
            }

            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }

            Err(error) => {
                break Err(error);
            }
        }
    };

    let restore = listener.set_nonblocking(false);

    match (result, restore) {
        (Ok((stream, address)), Ok(())) => {
            stream.set_nonblocking(false)?;

            Ok((stream, address))
        }

        (Err(error), _) => Err(error),

        (Ok(_), Err(error)) => Err(error),
    }
}

fn receive_one(
    listener: &TcpListener,
    progress: Option<ProgressCounter>,
    socket_receive_buffer_bytes: Option<usize>,
) -> io::Result<NetworkCalibrationReport> {
    let (mut control_stream, _control_peer) = accept_with_progress(listener, progress.as_ref())?;

    control_plane::configure_stream(&control_stream)?;

    let config = read_control_header(&mut control_stream)?;

    validate_config(config.total_bytes, config.data_stream_count)?;

    if let Some(progress) = &progress
        && progress.total() == 0
    {
        prepare_matrix_progress(progress, config.total_bytes)?;
    }

    let mut data_streams: Vec<Option<TcpStream>> = std::iter::repeat_with(|| None)
        .take(config.data_stream_count)
        .collect();

    for _ in 0..config.data_stream_count {
        let (mut stream, _data_peer) = accept_with_progress(listener, progress.as_ref())?;

        control_plane::configure_stream(&stream)?;

        if let Some(buffer_bytes) = socket_receive_buffer_bytes {
            SockRef::from(&stream).set_recv_buffer_size(buffer_bytes)?;
        }

        let (stream_id, declared_bytes) = read_data_header(&mut stream, config)?;

        let expected_bytes = lane_bytes(config.total_bytes, config.data_stream_count, stream_id)?;

        if declared_bytes != expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "network calibration lane {stream_id} declared {declared_bytes} bytes instead of {expected_bytes}"
                ),
            ));
        }

        let slot = data_streams.get_mut(stream_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "network calibration stream ID is outside the negotiated range",
            )
        })?;

        if slot.replace(stream).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate network calibration stream ID",
            ));
        }
    }

    let mut ordered_streams = Vec::with_capacity(config.data_stream_count);

    for (stream_id, stream) in data_streams.into_iter().enumerate() {
        ordered_streams.push(stream.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("network calibration stream {stream_id} was not established"),
            )
        })?);
    }

    let socket_buffers = socket_buffer_sizes(
        ordered_streams
            .first()
            .ok_or_else(|| io::Error::other("network calibration accepted no data streams"))?,
    )?;

    let started = Instant::now();

    write_receiver_ready(&mut control_stream)?;

    let received_bytes = thread::scope(|scope| -> io::Result<u64> {
        let mut handles = Vec::with_capacity(config.data_stream_count);

        for (stream_id, stream) in ordered_streams.into_iter().enumerate() {
            let bytes = lane_bytes(config.total_bytes, config.data_stream_count, stream_id)?;

            let lane_progress = progress.clone();

            handles.push(scope.spawn(move || receive_lane(stream, bytes, lane_progress)));
        }

        join_lane_threads(handles, "network calibration receiver lane panicked")
    })?;

    let elapsed = started.elapsed();

    if received_bytes != config.total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "network calibration received {received_bytes} bytes instead of {}",
                config.total_bytes
            ),
        ));
    }

    write_ack(&mut control_stream, received_bytes)?;

    build_report(
        config.data_stream_count,
        received_bytes,
        socket_buffers,
        elapsed,
    )
}

fn build_matrix_report(
    reports: Vec<NetworkCalibrationReport>,
) -> io::Result<NetworkCalibrationMatrixReport> {
    let best = reports
        .iter()
        .copied()
        .max_by(|left, right| {
            report_megabytes_per_second(left).total_cmp(&report_megabytes_per_second(right))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration matrix contained no reports",
            )
        })?;

    let best_megabytes_per_second = report_megabytes_per_second(&best);

    let recommendation_threshold = best_megabytes_per_second * MATRIX_RECOMMENDATION_FRACTION;

    let recommended = reports
        .iter()
        .copied()
        .filter(|report| report_megabytes_per_second(report) >= recommendation_threshold)
        .min_by_key(|report| report.data_stream_count)
        .ok_or_else(|| {
            io::Error::other("network calibration matrix could not select a recommendation")
        })?;

    let recommended_percent_of_best = if best_megabytes_per_second == 0.0 {
        0.0
    } else {
        report_megabytes_per_second(&recommended) / best_megabytes_per_second * 100.0
    };

    Ok(NetworkCalibrationMatrixReport {
        reports,
        best,
        recommended,
        recommended_percent_of_best,
    })
}

fn report_megabytes_per_second(report: &NetworkCalibrationReport) -> f64 {
    decimal_megabytes_per_second(report.total_bytes, report.elapsed)
}

fn prepare_matrix_progress(progress: &ProgressCounter, bytes_per_run: u64) -> io::Result<()> {
    let run_count = u64::try_from(MATRIX_STREAM_COUNTS.len())
        .map_err(|_| io::Error::other("calibration matrix run count cannot be represented"))?;

    let total_bytes = bytes_per_run.checked_mul(run_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration matrix progress size overflowed",
        )
    })?;

    progress.set_completed(0);
    progress.set_total(total_bytes);

    Ok(())
}

fn validate_config(total_bytes: u64, data_stream_count: usize) -> io::Result<()> {
    if total_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration byte count must not be zero",
        ));
    }

    if total_bytes > MAX_TOTAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("network calibration byte count exceeds the {MAX_TOTAL_BYTES}-byte limit"),
        ));
    }

    control_plane::validate_data_stream_count(data_stream_count)
}

fn lane_bytes(total_bytes: u64, data_stream_count: usize, stream_id: usize) -> io::Result<u64> {
    let stream_count = u64::try_from(data_stream_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration stream count cannot be represented",
        )
    })?;

    let stream_id_u64 = u64::try_from(stream_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration stream ID cannot be represented",
        )
    })?;

    if stream_id_u64 >= stream_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration stream ID is outside the configured range",
        ));
    }

    let base = total_bytes / stream_count;

    let remainder = total_bytes % stream_count;

    Ok(base + u64::from(stream_id_u64 < remainder))
}

fn build_payload_buffer() -> Vec<u8> {
    let mut payload = vec![0_u8; BUFFER_BYTES];

    let mut state = 0x1234_5678_u32;

    for byte in &mut payload {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;

        *byte = state as u8;
    }

    payload
}

fn send_lane(
    mut stream: TcpStream,
    payload: Arc<Vec<u8>>,
    total_bytes: u64,
    progress: Option<ProgressCounter>,
) -> io::Result<u64> {
    let mut remaining = total_bytes;

    while remaining > 0 {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let count = usize::try_from(remaining.min(payload.len() as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration write size cannot be represented",
            )
        })?;

        stream.write_all(&payload[..count])?;

        if let Some(progress) = &progress {
            progress.add(count as u64);
        }

        remaining -= count as u64;
    }

    stream.shutdown(Shutdown::Write)?;

    Ok(total_bytes)
}

fn receive_lane(
    mut stream: TcpStream,
    total_bytes: u64,
    progress: Option<ProgressCounter>,
) -> io::Result<u64> {
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    let mut remaining = total_bytes;

    while remaining > 0 {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let count = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration read size cannot be represented",
            )
        })?;

        stream.read_exact(&mut buffer[..count])?;

        if let Some(progress) = &progress {
            progress.add(count as u64);
        }

        remaining -= count as u64;
    }

    Ok(total_bytes)
}

fn join_lane_threads<'scope>(
    handles: Vec<thread::ScopedJoinHandle<'scope, io::Result<u64>>>,
    panic_message: &str,
) -> io::Result<u64> {
    let mut total = 0_u64;

    for handle in handles {
        let bytes = handle
            .join()
            .map_err(|_| io::Error::other(panic_message))??;

        total = total.checked_add(bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "network calibration byte count overflowed",
            )
        })?;
    }

    Ok(total)
}

fn write_control_header(writer: &mut impl Write, config: CalibrationConfig) -> io::Result<()> {
    writer.write_all(&CONTROL_MAGIC)?;

    write_u32(writer, PROTOCOL_VERSION)?;

    write_u64(writer, config.session_id)?;

    write_u32(
        writer,
        u32::try_from(config.data_stream_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration stream count cannot be represented",
            )
        })?,
    )?;

    write_u64(writer, config.total_bytes)?;

    writer.flush()
}

fn read_control_header(reader: &mut impl Read) -> io::Result<CalibrationConfig> {
    read_magic(reader, CONTROL_MAGIC, "network calibration control")?;

    read_version(reader)?;

    let session_id = read_u64(reader)?;

    let data_stream_count = usize::try_from(read_u32(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration stream count cannot be represented",
        )
    })?;

    let total_bytes = read_u64(reader)?;

    Ok(CalibrationConfig {
        session_id,
        data_stream_count,
        total_bytes,
    })
}

fn write_data_header(
    writer: &mut impl Write,
    config: CalibrationConfig,
    stream_id: usize,
    lane_bytes: u64,
) -> io::Result<()> {
    writer.write_all(&DATA_MAGIC)?;

    write_u32(writer, PROTOCOL_VERSION)?;

    write_u64(writer, config.session_id)?;

    write_u32(
        writer,
        u32::try_from(stream_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration stream ID cannot be represented",
            )
        })?,
    )?;

    write_u32(
        writer,
        u32::try_from(config.data_stream_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "network calibration stream count cannot be represented",
            )
        })?,
    )?;

    write_u64(writer, lane_bytes)?;

    writer.flush()
}

fn read_data_header(reader: &mut impl Read, config: CalibrationConfig) -> io::Result<(usize, u64)> {
    read_magic(reader, DATA_MAGIC, "network calibration data")?;

    read_version(reader)?;

    let session_id = read_u64(reader)?;

    if session_id != config.session_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration data lane used an incorrect session ID",
        ));
    }

    let stream_id = usize::try_from(read_u32(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration stream ID cannot be represented",
        )
    })?;

    let stream_count = usize::try_from(read_u32(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration stream count cannot be represented",
        )
    })?;

    if stream_count != config.data_stream_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration data lane used an incorrect stream count",
        ));
    }

    let lane_bytes = read_u64(reader)?;

    Ok((stream_id, lane_bytes))
}

fn write_receiver_ready(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(&[RECEIVER_READY])?;

    writer.flush()
}

fn read_receiver_ready(reader: &mut impl Read) -> io::Result<()> {
    let mut message = [0_u8; 1];

    reader.read_exact(&mut message).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("receiver disconnected before calibration became ready: {error}"),
        )
    })?;

    if message[0] != RECEIVER_READY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network calibration receiver returned an invalid ready message",
        ));
    }

    Ok(())
}

fn write_ack(writer: &mut impl Write, total_bytes: u64) -> io::Result<()> {
    writer.write_all(&ACK_MAGIC)?;

    write_u64(writer, total_bytes)?;

    writer.flush()
}

fn read_ack(reader: &mut impl Read) -> io::Result<u64> {
    read_magic(reader, ACK_MAGIC, "network calibration acknowledgement")?;

    read_u64(reader)
}

fn read_version(reader: &mut impl Read) -> io::Result<()> {
    let version = read_u32(reader)?;

    if version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported network calibration protocol version {version}"),
        ));
    }

    Ok(())
}

fn read_magic(reader: &mut impl Read, expected: [u8; 4], description: &str) -> io::Result<()> {
    let mut actual = [0_u8; 4];

    reader.read_exact(&mut actual)?;

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {description} magic"),
        ));
    }

    Ok(())
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0_u8; 4];

    reader.read_exact(&mut value)?;

    Ok(u32::from_be_bytes(value))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];

    reader.read_exact(&mut value)?;

    Ok(u64::from_be_bytes(value))
}

fn socket_buffer_sizes(stream: &TcpStream) -> io::Result<SocketBufferSizes> {
    let socket = SockRef::from(stream);

    Ok(SocketBufferSizes {
        send_bytes: socket.send_buffer_size()?,
        receive_bytes: socket.recv_buffer_size()?,
    })
}

fn build_report(
    data_stream_count: usize,
    total_bytes: u64,
    socket_buffers: SocketBufferSizes,
    elapsed: Duration,
) -> io::Result<NetworkCalibrationReport> {
    let stream_count = u64::try_from(data_stream_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network calibration stream count cannot be represented",
        )
    })?;

    let buffer_bytes_per_lane = BUFFER_BYTES as u64;

    let process_buffer_bytes =
        buffer_bytes_per_lane
            .checked_mul(stream_count)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "network calibration buffer count overflowed",
                )
            })?;

    let socket_send_buffer_bytes = u64::try_from(socket_buffers.send_bytes)
        .map_err(|_| io::Error::other("socket send buffer size cannot be represented"))?;

    let socket_receive_buffer_bytes = u64::try_from(socket_buffers.receive_bytes)
        .map_err(|_| io::Error::other("socket receive buffer size cannot be represented"))?;

    Ok(NetworkCalibrationReport {
        data_stream_count,
        total_bytes,
        buffer_bytes_per_lane,
        process_buffer_bytes,
        socket_send_buffer_bytes,
        socket_receive_buffer_bytes,
        elapsed,
    })
}

fn gigabits_per_second(bytes: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();

    if seconds == 0.0 {
        return f64::INFINITY;
    }

    bytes as f64 * 8.0 / seconds / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::{
        MATRIX_STREAM_COUNTS, NetworkCalibrationReport, build_matrix_report, lane_bytes,
        receive_matrix, receive_once, receive_once_with_socket_receive_buffer, send, send_matrix,
        socket_buffer_bytes_from_kib,
    };
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn socket_buffer_kib_converts_to_bytes() {
        assert_eq!(socket_buffer_bytes_from_kib(256).unwrap(), 256 * 1024,);

        assert_eq!(socket_buffer_bytes_from_kib(1024).unwrap(), 1024 * 1024,);

        assert_eq!(socket_buffer_bytes_from_kib(4096).unwrap(), 4 * 1024 * 1024,);

        assert!(socket_buffer_bytes_from_kib(0).is_err());

        assert!(socket_buffer_bytes_from_kib(u64::MAX).is_err(),);
    }

    #[test]
    fn lane_ranges_cover_total_bytes() {
        assert_eq!(lane_bytes(10, 3, 0).unwrap(), 4);

        assert_eq!(lane_bytes(10, 3, 1).unwrap(), 3);

        assert_eq!(lane_bytes(10, 3, 2).unwrap(), 3);
    }

    #[test]
    fn matrix_recommends_smallest_near_best_count() {
        let report = |data_stream_count, milliseconds| NetworkCalibrationReport {
            data_stream_count,
            total_bytes: 100 * 1024 * 1024,

            buffer_bytes_per_lane: 1024 * 1024,

            process_buffer_bytes: data_stream_count as u64 * 1024 * 1024,

            socket_send_buffer_bytes: 64 * 1024,

            socket_receive_buffer_bytes: 64 * 1024,

            elapsed: Duration::from_millis(milliseconds),
        };

        let matrix = build_matrix_report(vec![
            report(1, 200),
            report(2, 120),
            report(4, 55),
            report(8, 50),
        ])
        .unwrap();

        assert_eq!(matrix.best.data_stream_count, 8);

        assert_eq!(matrix.recommended.data_stream_count, 4);

        assert!(matrix.recommended_percent_of_best >= 90.0);
    }

    #[test]
    fn loopback_calibration_round_trips() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let receiver = thread::spawn(move || receive_once(listener));

        let total_bytes = 8 * 1024 * 1024 + 137;

        let sender_report = send(address, total_bytes, 2).unwrap();

        let receiver_report = receiver.join().unwrap().unwrap();

        assert_eq!(sender_report.total_bytes, total_bytes);

        assert_eq!(receiver_report.total_bytes, total_bytes);

        assert_eq!(sender_report.data_stream_count, 2);

        assert_eq!(receiver_report.data_stream_count, 2);

        assert!(sender_report.socket_send_buffer_bytes > 0);

        assert!(sender_report.socket_receive_buffer_bytes > 0);

        assert!(receiver_report.socket_send_buffer_bytes > 0);

        assert!(receiver_report.socket_receive_buffer_bytes > 0);
    }

    #[test]
    fn loopback_explicit_receive_buffer_round_trips() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let requested_buffer_bytes = 256 * 1024;

        let receiver = thread::spawn(move || {
            receive_once_with_socket_receive_buffer(listener, requested_buffer_bytes)
        });

        let total_bytes = 8 * 1024 * 1024 + 137;

        let sender_report = send(address, total_bytes, 2).unwrap();

        let receiver_report = receiver.join().unwrap().unwrap();

        assert_eq!(sender_report.total_bytes, total_bytes);

        assert_eq!(receiver_report.total_bytes, total_bytes);

        assert_eq!(
            receiver_report.socket_receive_buffer_bytes,
            requested_buffer_bytes as u64,
        );
    }

    #[test]
    fn loopback_matrix_round_trips() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let receiver = thread::spawn(move || receive_matrix(listener));

        let total_bytes = 8 * 1024 * 1024 + 137;

        let sender_matrix = send_matrix(address, total_bytes).unwrap();

        let receiver_matrix = receiver.join().unwrap().unwrap();

        assert_eq!(sender_matrix.reports.len(), MATRIX_STREAM_COUNTS.len());

        assert_eq!(receiver_matrix.reports.len(), MATRIX_STREAM_COUNTS.len());

        let sender_counts: Vec<usize> = sender_matrix
            .reports
            .iter()
            .map(|report| report.data_stream_count)
            .collect();

        let receiver_counts: Vec<usize> = receiver_matrix
            .reports
            .iter()
            .map(|report| report.data_stream_count)
            .collect();

        assert_eq!(sender_counts, MATRIX_STREAM_COUNTS);

        assert_eq!(receiver_counts, MATRIX_STREAM_COUNTS);

        for report in sender_matrix.reports {
            assert_eq!(report.total_bytes, total_bytes);
        }

        for report in receiver_matrix.reports {
            assert_eq!(report.total_bytes, total_bytes);
        }
    }

    #[test]
    fn matrix_stream_count_validation_accepts_supported_counts() {
        for data_stream_count in [1, 2, 4, 8] {
            super::validate_matrix_stream_count(data_stream_count).unwrap();
        }

        for data_stream_count in [0, 3, 16] {
            assert!(super::validate_matrix_stream_count(data_stream_count,).is_err(),);
        }
    }
}
