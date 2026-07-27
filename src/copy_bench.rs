use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

pub const DEFAULT_BUFFER_MIB: usize = 8;
pub const MAX_BUFFER_MIB: usize = 1024;

#[derive(Debug)]
pub struct CopyReport {
    pub bytes_copied: u64,
    pub buffer_bytes: usize,
    pub setup_elapsed: Duration,
    pub io_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl CopyReport {
    pub fn print(&self) {
        println!("Copy complete");
        println!("  Bytes copied:  {}", format_bytes(self.bytes_copied));
        println!("  Buffer size:   {} MiB", self.buffer_bytes / (1024 * 1024));
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

pub fn run(source: &Path, destination: &Path, buffer_mib: usize) -> io::Result<CopyReport> {
    let total_started = Instant::now();
    let buffer_bytes = buffer_bytes_from_mib(buffer_mib)?;

    reject_same_file(source, destination)?;

    let mut source_file = OpenOptions::new()
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

    // Establish the final logical file length before transfer begins.
    destination_file.set_len(source_len)?;

    let mut buffer = vec![0_u8; buffer_bytes];
    let setup_elapsed = total_started.elapsed();
    let io_started = Instant::now();
    let mut bytes_copied = 0_u64;

    loop {
        let bytes_read = read_retry_interrupted(&mut source_file, &mut buffer)?;

        if bytes_read == 0 {
            break;
        }

        destination_file.write_all(&buffer[..bytes_read])?;

        bytes_copied = bytes_copied
            .checked_add(bytes_read as u64)
            .ok_or_else(|| io::Error::other("copied byte count overflowed"))?;
    }

    destination_file.flush()?;

    let io_elapsed = io_started.elapsed();
    let total_elapsed = total_started.elapsed();

    if bytes_copied != source_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "source length changed during transfer: expected {source_len} bytes, copied \
                 {bytes_copied} bytes"
            ),
        ));
    }

    Ok(CopyReport {
        bytes_copied,
        buffer_bytes,
        setup_elapsed,
        io_elapsed,
        total_elapsed,
    })
}

pub fn buffer_bytes_from_mib(buffer_mib: usize) -> io::Result<usize> {
    if !(1..=MAX_BUFFER_MIB).contains(&buffer_mib) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("buffer size must be between 1 and {MAX_BUFFER_MIB} MiB"),
        ));
    }

    buffer_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer size overflowed"))
}

pub(crate) fn reject_same_file(source: &Path, destination: &Path) -> io::Result<()> {
    let source = source.canonicalize()?;

    let Ok(destination) = destination.canonicalize() else {
        return Ok(());
    };

    if source == destination {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination refer to the same file",
        ));
    }

    Ok(())
}

fn read_retry_interrupted(source: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match source.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

pub(crate) fn decimal_megabytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    throughput(bytes, elapsed, 1_000_000.0)
}

pub(crate) fn binary_mebibytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    throughput(bytes, elapsed, 1024.0 * 1024.0)
}

fn throughput(bytes: u64, elapsed: Duration, unit_size: f64) -> f64 {
    let seconds = elapsed.as_secs_f64();

    if seconds == 0.0 {
        return 0.0;
    }

    bytes as f64 / unit_size / seconds
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }

        formatted.push(char::from(byte));
    }

    formatted
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_BUFFER_MIB, binary_mebibytes_per_second, buffer_bytes_from_mib,
        decimal_megabytes_per_second, format_bytes,
    };
    use std::time::Duration;

    #[test]
    fn converts_mebibytes_to_bytes() {
        assert_eq!(buffer_bytes_from_mib(1).unwrap(), 1_048_576);
        assert_eq!(buffer_bytes_from_mib(8).unwrap(), 8_388_608);
    }

    #[test]
    fn rejects_invalid_buffer_sizes() {
        assert!(buffer_bytes_from_mib(0).is_err());
        assert!(buffer_bytes_from_mib(MAX_BUFFER_MIB + 1).is_err());
    }

    #[test]
    fn calculates_decimal_throughput() {
        let result = decimal_megabytes_per_second(200_000_000, Duration::from_secs(2));

        assert!((result - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn calculates_binary_throughput() {
        let result = binary_mebibytes_per_second(200 * 1024 * 1024, Duration::from_secs(2));

        assert!((result - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn formats_large_byte_counts() {
        assert_eq!(format_bytes(0), "0");
        assert_eq!(format_bytes(999), "999");
        assert_eq!(format_bytes(1_000), "1,000");
        assert_eq!(format_bytes(12_345_678), "12,345,678");
    }
}
