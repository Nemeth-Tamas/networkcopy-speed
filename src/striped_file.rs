use crate::control_plane;
use crate::copy_bench::{
    binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes, reject_same_file,
};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const STRIPE_MAGIC: [u8; 4] = *b"NCS5";
const STRIPE_VERSION: u16 = 1;
const COPY_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub const DEFAULT_DATA_STREAMS: usize = 4;

#[derive(Debug)]
pub struct StripedFileReport {
    pub bytes_copied: u64,
    pub data_stream_count: usize,
    pub smallest_stripe: u64,
    pub largest_stripe: u64,
    pub connection_elapsed: Duration,
    pub data_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl StripedFileReport {
    pub fn print(&self) {
        println!("Striped TCP file copy complete");
        println!("  Bytes copied:       {}", format_bytes(self.bytes_copied));
        println!("  TCP data streams:   {}", self.data_stream_count);
        println!(
            "  Smallest stripe:    {} bytes",
            format_bytes(self.smallest_stripe)
        );
        println!(
            "  Largest stripe:     {} bytes",
            format_bytes(self.largest_stripe)
        );
        println!(
            "  Connection time:    {:.6} s",
            self.connection_elapsed.as_secs_f64()
        );
        println!(
            "  Transfer time:      {:.6} s",
            self.data_elapsed.as_secs_f64()
        );
        println!(
            "  Total time:         {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Payload throughput: {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_copied, self.data_elapsed),
            binary_mebibytes_per_second(self.bytes_copied, self.data_elapsed)
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StripeHandshake {
    stream_id: u32,
    stream_count: u32,
    file_len: u64,
    offset: u64,
    length: u64,
}

pub fn run(
    source: &Path,
    destination: &Path,
    data_stream_count: usize,
) -> io::Result<StripedFileReport> {
    control_plane::validate_data_stream_count(data_stream_count)?;
    reject_same_file(source, destination)?;

    let total_started = Instant::now();

    let source_file = Arc::new(File::open(source)?);
    let source_metadata = source_file.metadata()?;

    if !source_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let file_len = source_metadata.len();

    let destination_file = Arc::new(
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(destination)?,
    );

    destination_file.set_len(file_len)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;

    let receiver_file = Arc::clone(&destination_file);

    let receiver = thread::Builder::new()
        .name("networkcopy-striped-receiver".to_string())
        .spawn(move || receive_all_stripes(listener, receiver_file, file_len, data_stream_count))?;

    let connection_started = Instant::now();
    let mut sender_streams = Vec::with_capacity(data_stream_count);
    let mut smallest_stripe = u64::MAX;
    let mut largest_stripe = 0_u64;

    for stream_id in 0..data_stream_count {
        let (offset, length) = stripe_range(file_len, stream_id, data_stream_count)?;

        smallest_stripe = smallest_stripe.min(length);
        largest_stripe = largest_stripe.max(length);

        let mut stream = TcpStream::connect(address)?;
        control_plane::configure_stream(&stream)?;

        write_handshake(
            &mut stream,
            StripeHandshake {
                stream_id: u32::try_from(stream_id).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "stream ID cannot be represented as u32",
                    )
                })?,
                stream_count: u32::try_from(data_stream_count).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "stream count cannot be represented as u32",
                    )
                })?,
                file_len,
                offset,
                length,
            },
        )?;

        sender_streams.push((stream, offset, length));
    }

    if data_stream_count == 0 {
        smallest_stripe = 0;
    }

    let connection_elapsed = connection_started.elapsed();
    let data_started = Instant::now();

    let sender_file = Arc::clone(&source_file);

    let sender_results = thread::scope(|scope| -> io::Result<Vec<u64>> {
        let mut handles = Vec::with_capacity(data_stream_count);

        for (stream, offset, length) in sender_streams {
            let lane_file = Arc::clone(&sender_file);

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-striped-sender-lane".to_string())
                    .spawn_scoped(scope, move || {
                        send_stripe(stream, lane_file, offset, length)
                    })?,
            );
        }

        join_lane_threads(handles)
    })?;

    let bytes_sent = sum_lane_bytes(sender_results)?;

    let bytes_received = receiver
        .join()
        .map_err(|_| io::Error::other("striped receiver thread panicked"))??;

    let data_elapsed = data_started.elapsed();

    if bytes_sent != file_len {
        return Err(io::Error::other(format!(
            "striped send was incomplete: expected {file_len} bytes, sent {bytes_sent}"
        )));
    }

    if bytes_received != file_len {
        return Err(io::Error::other(format!(
            "striped receive was incomplete: expected {file_len} bytes, received \
             {bytes_received}"
        )));
    }

    Ok(StripedFileReport {
        bytes_copied: file_len,
        data_stream_count,
        smallest_stripe,
        largest_stripe,
        connection_elapsed,
        data_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

fn receive_all_stripes(
    listener: TcpListener,
    destination: Arc<File>,
    file_len: u64,
    data_stream_count: usize,
) -> io::Result<u64> {
    let mut streams: Vec<Option<(TcpStream, StripeHandshake)>> = std::iter::repeat_with(|| None)
        .take(data_stream_count)
        .collect();

    for _ in 0..data_stream_count {
        let (mut stream, _) = listener.accept()?;
        control_plane::configure_stream(&stream)?;

        let handshake = read_handshake(&mut stream)?;
        validate_handshake(handshake, file_len, data_stream_count)?;

        let stream_id = usize::try_from(handshake.stream_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "received stream ID cannot be represented",
            )
        })?;

        let slot = streams.get_mut(stream_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "received stream ID is outside the negotiated range",
            )
        })?;

        if slot.replace((stream, handshake)).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate stripe stream ID {stream_id}"),
            ));
        }
    }

    let receiver_results = thread::scope(|scope| -> io::Result<Vec<u64>> {
        let mut handles = Vec::with_capacity(data_stream_count);

        for (stream_id, stream) in streams.into_iter().enumerate() {
            let (stream, handshake) = stream.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("stripe stream {stream_id} was not established"),
                )
            })?;

            let lane_file = Arc::clone(&destination);

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-striped-receiver-lane".to_string())
                    .spawn_scoped(scope, move || receive_stripe(stream, lane_file, handshake))?,
            );
        }

        join_lane_threads(handles)
    })?;

    sum_lane_bytes(receiver_results)
}

fn send_stripe(
    mut stream: TcpStream,
    source: Arc<File>,
    offset: u64,
    length: u64,
) -> io::Result<u64> {
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut transferred = 0_u64;

    while transferred < length {
        let remaining = length - transferred;
        let requested = remaining.min(buffer.len() as u64) as usize;

        let read = read_at_retry(&source, &mut buffer[..requested], offset + transferred)?;

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "source ended with {} stripe bytes remaining",
                    length - transferred
                ),
            ));
        }

        stream.write_all(&buffer[..read])?;

        transferred = transferred
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("stripe send byte count overflowed"))?;
    }

    stream.shutdown(Shutdown::Write)?;

    let acknowledged = read_u64(&mut stream)?;

    if acknowledged != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("receiver acknowledged {acknowledged} bytes for a {length} byte stripe"),
        ));
    }

    Ok(transferred)
}

fn receive_stripe(
    mut stream: TcpStream,
    destination: Arc<File>,
    handshake: StripeHandshake,
) -> io::Result<u64> {
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut transferred = 0_u64;

    while transferred < handshake.length {
        let remaining = handshake.length - transferred;
        let requested = remaining.min(buffer.len() as u64) as usize;

        let read = stream.read(&mut buffer[..requested])?;

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "stripe stream ended with {} bytes remaining",
                    handshake.length - transferred
                ),
            ));
        }

        write_all_at(
            &destination,
            &buffer[..read],
            handshake.offset + transferred,
        )?;

        transferred = transferred
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("stripe receive byte count overflowed"))?;
    }

    expect_stream_eof(&mut stream)?;

    write_u64(&mut stream, transferred)?;
    stream.flush()?;

    Ok(transferred)
}

fn validate_handshake(
    handshake: StripeHandshake,
    file_len: u64,
    data_stream_count: usize,
) -> io::Result<()> {
    if handshake.file_len != file_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stripe connection announced an incorrect file length",
        ));
    }

    if handshake.stream_count != data_stream_count as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stripe connection announced an incorrect stream count",
        ));
    }

    let stream_id = usize::try_from(handshake.stream_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stripe stream ID cannot be represented",
        )
    })?;

    let expected = stripe_range(file_len, stream_id, data_stream_count)?;

    if expected != (handshake.offset, handshake.length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stripe {stream_id} announced offset {} length {}, expected offset {} length {}",
                handshake.offset, handshake.length, expected.0, expected.1
            ),
        ));
    }

    Ok(())
}

fn stripe_range(
    file_len: u64,
    stream_id: usize,
    data_stream_count: usize,
) -> io::Result<(u64, u64)> {
    if data_stream_count == 0 || stream_id >= data_stream_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stripe stream ID is outside the configured range",
        ));
    }

    if file_len == 0 {
        return Ok((0, 0));
    }

    let stripe_size = file_len.div_ceil(data_stream_count as u64);

    let offset = stripe_size
        .checked_mul(stream_id as u64)
        .ok_or_else(|| io::Error::other("stripe offset overflowed"))?;

    if offset >= file_len {
        return Ok((file_len, 0));
    }

    Ok((offset, (file_len - offset).min(stripe_size)))
}

fn read_at_retry(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    loop {
        match file.seek_read(buffer, offset) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match file.seek_write(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "positional stripe write transferred zero bytes",
                ));
            }

            Ok(written) => {
                buffer = &buffer[written..];

                offset = offset
                    .checked_add(written as u64)
                    .ok_or_else(|| io::Error::other("stripe write offset overflowed"))?;
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn expect_stream_eof(stream: &mut TcpStream) -> io::Result<()> {
    let mut extra = [0_u8; 1];

    loop {
        match stream.read(&mut extra) {
            Ok(0) => return Ok(()),

            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stripe stream contained excess payload data",
                ));
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            Err(error) => return Err(error),
        }
    }
}

fn write_handshake(writer: &mut impl Write, handshake: StripeHandshake) -> io::Result<()> {
    writer.write_all(&STRIPE_MAGIC)?;
    writer.write_all(&STRIPE_VERSION.to_be_bytes())?;
    writer.write_all(&handshake.stream_id.to_be_bytes())?;
    writer.write_all(&handshake.stream_count.to_be_bytes())?;
    writer.write_all(&handshake.file_len.to_be_bytes())?;
    writer.write_all(&handshake.offset.to_be_bytes())?;
    writer.write_all(&handshake.length.to_be_bytes())?;
    writer.flush()
}

fn read_handshake(reader: &mut impl Read) -> io::Result<StripeHandshake> {
    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;

    if magic != STRIPE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stripe connection used an invalid protocol magic",
        ));
    }

    let version = read_u16(reader)?;

    if version != STRIPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported stripe protocol version {version}"),
        ));
    }

    Ok(StripeHandshake {
        stream_id: read_u32(reader)?,
        stream_count: read_u32(reader)?,
        file_len: read_u64(reader)?,
        offset: read_u64(reader)?,
        length: read_u64(reader)?,
    })
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
                    first_error = Some(io::Error::other("striped TCP lane thread panicked"));
                }
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(results)
}

fn sum_lane_bytes(results: Vec<u64>) -> io::Result<u64> {
    results.into_iter().try_fold(0_u64, |total, bytes| {
        total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("stripe byte count overflowed"))
    })
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut value = [0_u8; 2];
    reader.read_exact(&mut value)?;
    Ok(u16::from_be_bytes(value))
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

#[cfg(test)]
mod tests {
    use super::{run, stripe_range};
    use std::env;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn stripe_ranges_cover_file_without_gaps() {
        for file_len in [0, 1, 17, 8 * 1024 * 1024 + 137] {
            for stream_count in [1, 2, 4, 8] {
                let mut expected_offset = 0_u64;
                let mut total_length = 0_u64;

                for stream_id in 0..stream_count {
                    let (offset, length) = stripe_range(file_len, stream_id, stream_count).unwrap();

                    assert_eq!(offset, expected_offset.min(file_len));

                    expected_offset = offset + length;
                    total_length += length;
                }

                assert_eq!(total_length, file_len);
            }
        }
    }

    #[test]
    fn loopback_striped_copy_matches_source() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent =
            env::temp_dir().join(format!("networkcopy-striped-{}-{unique}", process::id()));

        fs::create_dir_all(&parent).unwrap();

        let source = parent.join("source.bin");
        let destination = parent.join("destination.bin");

        let mut contents = vec![0_u8; 10 * 1024 * 1024 + 137];

        for (index, byte) in contents.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }

        fs::write(&source, &contents).unwrap();

        let copy_result = run(&source, &destination, 4);
        let copied = fs::read(&destination);

        let report = copy_result.unwrap();
        let copied = copied.unwrap();

        assert_eq!(report.bytes_copied, contents.len() as u64);
        assert_eq!(copied, contents);

        fs::remove_dir_all(parent).unwrap();
    }
}
