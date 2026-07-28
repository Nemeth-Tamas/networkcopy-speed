use crate::copy_bench::{
    binary_mebibytes_per_second, buffer_bytes_from_mib, decimal_megabytes_per_second, format_bytes,
};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

pub const DEFAULT_BUFFER_MIB: usize = 8;

#[derive(Clone, Debug)]
pub(crate) struct ContentHasher {
    inner: blake3::Hasher,
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }
}

impl ContentHasher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    pub(crate) fn finalize(self) -> [u8; 32] {
        *self.inner.finalize().as_bytes()
    }
}

#[derive(Debug)]
pub struct HashReport {
    pub bytes_hashed: u64,
    pub buffer_bytes: usize,
    pub digest: [u8; 32],
    pub setup_elapsed: Duration,
    pub hash_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl HashReport {
    pub fn print(&self) {
        println!("BLAKE3 file hash complete");
        println!("  Bytes hashed:   {}", format_bytes(self.bytes_hashed));
        println!(
            "  Buffer size:    {} MiB",
            self.buffer_bytes / (1024 * 1024)
        );
        println!("  Digest:         {}", format_digest(&self.digest));
        println!(
            "  Setup time:     {:.6} s",
            self.setup_elapsed.as_secs_f64()
        );
        println!("  Hash time:      {:.6} s", self.hash_elapsed.as_secs_f64());
        println!(
            "  Total time:     {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Hash throughput:{:>9.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_hashed, self.hash_elapsed,),
            binary_mebibytes_per_second(self.bytes_hashed, self.hash_elapsed,)
        );
    }
}

pub fn run(source: &Path, buffer_mib: usize) -> io::Result<HashReport> {
    let total_started = Instant::now();
    let buffer_bytes = buffer_bytes_from_mib(buffer_mib)?;

    let mut source_file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(source)?;

    let source_metadata = source_file.metadata()?;

    if !source_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let source_len = source_metadata.len();
    let mut buffer = vec![0_u8; buffer_bytes];
    let mut hasher = ContentHasher::new();

    let setup_elapsed = total_started.elapsed();
    let hash_started = Instant::now();
    let mut bytes_hashed = 0_u64;

    loop {
        let read = read_retry_interrupted(&mut source_file, &mut buffer)?;

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);

        bytes_hashed = bytes_hashed
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("hashed byte count overflowed"))?;
    }

    let digest = hasher.finalize();
    let hash_elapsed = hash_started.elapsed();

    if bytes_hashed != source_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "source length changed while hashing: expected \
                 {source_len} bytes, hashed {bytes_hashed} bytes"
            ),
        ));
    }

    Ok(HashReport {
        bytes_hashed,
        buffer_bytes,
        digest,
        setup_elapsed,
        hash_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

pub(crate) fn format_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut result = String::with_capacity(digest.len() * 2);

    for byte in digest {
        write!(result, "{byte:02x}").expect("writing to a String cannot fail");
    }

    result
}

fn read_retry_interrupted(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentHasher, format_digest, run};
    use std::env;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn streaming_chunks_match_one_shot_hash() {
        let mut hasher = ContentHasher::new();

        hasher.update(b"NetworkCopy ");
        hasher.update(b"Speed Edition");
        hasher.update(b" BLAKE3 test");

        let actual = hasher.finalize();

        let expected = blake3::hash(b"NetworkCopy Speed Edition BLAKE3 test");

        assert_eq!(actual, *expected.as_bytes());

        assert_eq!(format_digest(&actual).len(), 64);
    }

    #[test]
    fn hashes_file_from_disk() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let path = env::temp_dir().join(format!("networkcopy-hash-{}-{unique}.bin", process::id()));

        let contents = b"measured cryptographic integrity";

        fs::write(&path, contents).unwrap();

        let hash_result = run(&path, 1);
        let cleanup_result = fs::remove_file(path);

        let report = hash_result.unwrap();
        cleanup_result.unwrap();

        assert_eq!(report.bytes_hashed, contents.len() as u64);

        assert_eq!(report.digest, *blake3::hash(contents).as_bytes());
    }
}
