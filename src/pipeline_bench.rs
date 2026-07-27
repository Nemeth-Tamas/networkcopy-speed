use crate::copy_bench::{
    binary_mebibytes_per_second, buffer_bytes_from_mib, decimal_megabytes_per_second, format_bytes,
    reject_same_file,
};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
const MIB: usize = 1024 * 1024;

pub const DEFAULT_CHUNK_MIB: usize = 8;
pub const DEFAULT_BUFFER_COUNT: usize = 8;
pub const MAX_BUFFER_COUNT: usize = 256;
pub const MAX_POOL_MIB: usize = 4096;

#[derive(Debug)]
pub struct PipelineReport {
    pub bytes_copied: u64,
    pub chunk_bytes: usize,
    pub buffer_count: usize,
    pub pool_bytes: usize,
    pub setup_elapsed: Duration,
    pub io_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl PipelineReport {
    pub fn print(&self) {
        println!("Pipeline copy complete");
        println!("  Bytes copied:  {}", format_bytes(self.bytes_copied));
        println!("  Chunk size:    {} MiB", self.chunk_bytes / MIB);
        println!("  Buffers:       {}", self.buffer_count);
        println!("  Buffer pool:   {} MiB", self.pool_bytes / MIB);
        println!("  Setup time:    {:.3} s", self.setup_elapsed.as_secs_f64());
        println!("  Transfer time: {:.3} s", self.io_elapsed.as_secs_f64());
        println!("  Total time:    {:.3} s", self.total_elapsed.as_secs_f64());
        println!(
            "  Throughput:    {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_copied, self.io_elapsed),
            binary_mebibytes_per_second(self.bytes_copied, self.io_elapsed)
        );
    }
}

#[derive(Debug)]
struct BufferChunk {
    buffer: Vec<u8>,
    length: usize,
}

pub fn run(
    source: &Path,
    destination: &Path,
    chunk_mib: usize,
    buffer_count: usize,
) -> io::Result<PipelineReport> {
    let total_started = Instant::now();
    let (chunk_bytes, pool_bytes) = validate_config(chunk_mib, buffer_count)?;

    reject_same_file(source, destination)?;

    let source_file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(source)?;

    let metadata = source_file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let source_len = metadata.len();

    let mut destination_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(destination)?;

    destination_file.set_len(source_len)?;

    let (empty_sender, empty_receiver) = mpsc::sync_channel(buffer_count);
    let (filled_sender, filled_receiver) = mpsc::sync_channel(buffer_count);

    for _ in 0..buffer_count {
        empty_sender
            .send(vec![0_u8; chunk_bytes])
            .map_err(|_| io::Error::other("failed to initialize the empty buffer queue"))?;
    }

    let setup_elapsed = total_started.elapsed();
    let io_started = Instant::now();

    let reader = thread::Builder::new()
        .name("networkcopy-reader".to_string())
        .spawn(move || read_source(source_file, empty_receiver, filled_sender))?;

    let write_result = write_destination(&mut destination_file, &filled_receiver, &empty_sender);

    drop(filled_receiver);
    drop(empty_sender);

    let read_result = join_reader(reader);

    let bytes_written = write_result?;
    let bytes_read = read_result?;

    if bytes_read != source_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "source length changed during transfer: expected {source_len} bytes, read \
                 {bytes_read} bytes"
            ),
        ));
    }

    if bytes_written != bytes_read {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "pipeline byte counts differ: read {bytes_read} bytes, wrote {bytes_written} bytes"
            ),
        ));
    }

    destination_file.flush()?;

    let io_elapsed = io_started.elapsed();
    let total_elapsed = total_started.elapsed();

    Ok(PipelineReport {
        bytes_copied: bytes_written,
        chunk_bytes,
        buffer_count,
        pool_bytes,
        setup_elapsed,
        io_elapsed,
        total_elapsed,
    })
}

pub fn validate_config(chunk_mib: usize, buffer_count: usize) -> io::Result<(usize, usize)> {
    let chunk_bytes = buffer_bytes_from_mib(chunk_mib)?;

    if !(1..=MAX_BUFFER_COUNT).contains(&buffer_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("buffer count must be between 1 and {MAX_BUFFER_COUNT}"),
        ));
    }

    let pool_mib = chunk_mib.checked_mul(buffer_count).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "buffer pool size overflowed")
    })?;

    if pool_mib > MAX_POOL_MIB {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("buffer pool must not exceed {MAX_POOL_MIB} MiB"),
        ));
    }

    let pool_bytes = chunk_bytes.checked_mul(buffer_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer pool byte size overflowed",
        )
    })?;

    Ok((chunk_bytes, pool_bytes))
}

fn read_source(
    mut source: File,
    empty_receiver: Receiver<Vec<u8>>,
    filled_sender: SyncSender<BufferChunk>,
) -> io::Result<u64> {
    let mut bytes_read = 0_u64;

    loop {
        let mut buffer = empty_receiver.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer stopped before the reader finished",
            )
        })?;

        let mut length = 0_usize;
        let mut reached_eof = false;

        while length < buffer.len() {
            match source.read(&mut buffer[length..]) {
                Ok(0) => {
                    reached_eof = true;
                    break;
                }

                Ok(count) => {
                    length += count;
                    bytes_read = bytes_read
                        .checked_add(count as u64)
                        .ok_or_else(|| io::Error::other("read byte count overflowed"))?;
                }

                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

                Err(error) => return Err(error),
            }
        }

        if length > 0 {
            filled_sender
                .send(BufferChunk { buffer, length })
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "writer stopped while the reader was sending data",
                    )
                })?;
        }

        if reached_eof {
            return Ok(bytes_read);
        }
    }
}

fn write_destination(
    destination: &mut impl Write,
    filled_receiver: &Receiver<BufferChunk>,
    empty_sender: &SyncSender<Vec<u8>>,
) -> io::Result<u64> {
    let mut bytes_written = 0_u64;

    while let Ok(chunk) = filled_receiver.recv() {
        destination.write_all(&chunk.buffer[..chunk.length])?;

        bytes_written = bytes_written
            .checked_add(chunk.length as u64)
            .ok_or_else(|| io::Error::other("written byte count overflowed"))?;

        // On the final partial chunk the reader may already have exited,
        // so a disconnected empty-buffer queue is harmless here.
        let _ = empty_sender.send(chunk.buffer);
    }

    Ok(bytes_written)
}

fn join_reader(reader: JoinHandle<io::Result<u64>>) -> io::Result<u64> {
    reader
        .join()
        .map_err(|_| io::Error::other("reader thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BUFFER_COUNT, DEFAULT_CHUNK_MIB, MAX_BUFFER_COUNT, MAX_POOL_MIB, MIB,
        validate_config,
    };

    #[test]
    fn validates_default_pipeline_configuration() {
        let (chunk_bytes, pool_bytes) =
            validate_config(DEFAULT_CHUNK_MIB, DEFAULT_BUFFER_COUNT).unwrap();

        assert_eq!(chunk_bytes, 8 * MIB);
        assert_eq!(pool_bytes, 64 * MIB);
    }

    #[test]
    fn rejects_invalid_buffer_counts() {
        assert!(validate_config(8, 0).is_err());
        assert!(validate_config(8, MAX_BUFFER_COUNT + 1).is_err());
    }

    #[test]
    fn rejects_pool_larger_than_memory_budget() {
        assert!(validate_config(64, 65).is_err());
        assert!(validate_config(1024, 4).is_ok());
        assert_eq!(1024 * 4, MAX_POOL_MIB);
    }
}
