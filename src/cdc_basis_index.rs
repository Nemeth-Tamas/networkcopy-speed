use crate::content_defined_dedup_bench::{ChunkConfig, ChunkKey, chunk_key, chunk_reader};
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

const WIRE_MAGIC: [u8; 4] = *b"NCI1";

const WIRE_HEADER_BYTES: usize = 4 + 4 + 8 + 8;

const WIRE_RECORD_BYTES: usize = 8 + 4 + 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BasisChunk {
    pub(crate) offset: u64,
    pub(crate) key: ChunkKey,
}

#[derive(Debug)]
pub(crate) struct BasisFileIndex {
    average_kib: usize,
    file_bytes: u64,
    chunks: Vec<BasisChunk>,
    first_locations: HashMap<ChunkKey, u64>,
    build_elapsed: Duration,
}

#[derive(Debug)]
pub struct BasisIndexBenchReport {
    pub average_kib: usize,
    pub file_bytes: u64,
    pub chunks: u64,
    pub unique_chunks: u64,
    pub wire_bytes: u64,
    pub lookup_hits: u64,

    pub build_elapsed: Duration,
    pub encode_elapsed: Duration,
    pub decode_elapsed: Duration,
    pub lookup_elapsed: Duration,
}

impl BasisFileIndex {
    pub(crate) fn build(path: &Path, average_kib: usize) -> io::Result<Self> {
        let file = File::open(path)?;
        let expected_bytes = file.metadata()?.len();

        Self::build_from_reader(file, expected_bytes, average_kib)
    }

    fn build_from_reader(
        reader: impl Read,
        expected_bytes: u64,
        average_kib: usize,
    ) -> io::Result<Self> {
        let config = ChunkConfig::from_average_kib(average_kib)?;

        let mut chunks = Vec::new();

        let mut first_locations = HashMap::new();

        let chunking = chunk_reader(reader, config, |offset, contents| {
            let key = chunk_key(contents)?;

            chunks.push(BasisChunk { offset, key });

            first_locations.entry(key).or_insert(offset);

            Ok(())
        })?;

        if chunking.bytes != expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "basis file changed while being indexed: \
                     expected {expected_bytes} bytes, read {}",
                    chunking.bytes,
                ),
            ));
        }

        Ok(Self {
            average_kib,
            file_bytes: chunking.bytes,
            chunks,
            first_locations,
            build_elapsed: chunking.elapsed,
        })
    }

    pub(crate) fn average_kib(&self) -> usize {
        self.average_kib
    }

    pub(crate) fn file_bytes(&self) -> u64 {
        self.file_bytes
    }

    pub(crate) fn chunks(&self) -> &[BasisChunk] {
        &self.chunks
    }

    pub(crate) fn unique_chunk_count(&self) -> usize {
        self.first_locations.len()
    }

    pub(crate) fn build_elapsed(&self) -> Duration {
        self.build_elapsed
    }

    pub(crate) fn find(&self, key: &ChunkKey) -> Option<u64> {
        self.first_locations.get(key).copied()
    }

    pub(crate) fn encode_wire(&self) -> io::Result<Vec<u8>> {
        let chunk_count = u64::try_from(self.chunks.len())
            .map_err(|_| io::Error::other("basis chunk count cannot be represented"))?;

        let average_kib = u32::try_from(self.average_kib)
            .map_err(|_| io::Error::other("average chunk size cannot be represented"))?;

        let capacity = wire_size_for_chunks(self.chunks.len())?;

        let mut encoded = Vec::with_capacity(capacity);

        encoded.extend_from_slice(&WIRE_MAGIC);

        encoded.extend_from_slice(&average_kib.to_le_bytes());

        encoded.extend_from_slice(&self.file_bytes.to_le_bytes());

        encoded.extend_from_slice(&chunk_count.to_le_bytes());

        for chunk in &self.chunks {
            encoded.extend_from_slice(&chunk.offset.to_le_bytes());

            encoded.extend_from_slice(&chunk.key.length.to_le_bytes());

            encoded.extend_from_slice(&chunk.key.digest);
        }

        debug_assert_eq!(encoded.len(), capacity,);

        Ok(encoded)
    }

    pub(crate) fn decode_wire(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() < WIRE_HEADER_BYTES {
            return Err(invalid_wire("basis index header is truncated"));
        }

        if encoded[..4] != WIRE_MAGIC {
            return Err(invalid_wire("basis index magic is invalid"));
        }

        let mut cursor = 4_usize;

        let average_kib = usize::try_from(read_u32(encoded, &mut cursor)?)
            .map_err(|_| invalid_wire("average chunk size cannot be represented"))?;

        let config = ChunkConfig::from_average_kib(average_kib)
            .map_err(|error| invalid_wire(format!("invalid average chunk size: {error}",)))?;

        let file_bytes = read_u64(encoded, &mut cursor)?;

        let chunk_count_u64 = read_u64(encoded, &mut cursor)?;

        let chunk_count = usize::try_from(chunk_count_u64)
            .map_err(|_| invalid_wire("basis chunk count cannot be represented"))?;

        let expected_wire_bytes = wire_size_for_chunks(chunk_count)?;

        if encoded.len() != expected_wire_bytes {
            return Err(invalid_wire(format!(
                "basis index length is {}, expected {}",
                encoded.len(),
                expected_wire_bytes,
            )));
        }

        let mut chunks = Vec::with_capacity(chunk_count);

        let mut first_locations = HashMap::with_capacity(chunk_count);

        let mut expected_offset = 0_u64;

        for chunk_index in 0..chunk_count {
            let offset = read_u64(encoded, &mut cursor)?;

            let length = read_u32(encoded, &mut cursor)?;

            let digest = read_digest(encoded, &mut cursor)?;

            if length == 0 {
                return Err(invalid_wire("basis chunk length must not be zero"));
            }

            if offset != expected_offset {
                return Err(invalid_wire(format!(
                    "basis chunk {chunk_index} starts at \
                     {offset}, expected {expected_offset}",
                )));
            }

            let length_usize = usize::try_from(length)
                .map_err(|_| invalid_wire("basis chunk length cannot be represented"))?;

            if length_usize > config.maximum_bytes {
                return Err(invalid_wire(format!(
                    "basis chunk {chunk_index} exceeds \
                     the configured maximum size",
                )));
            }

            let is_final = chunk_index + 1 == chunk_count;

            if !is_final && length_usize < config.minimum_bytes {
                return Err(invalid_wire(format!(
                    "non-final basis chunk {chunk_index} \
                     is smaller than the configured minimum",
                )));
            }

            let key = ChunkKey { digest, length };

            chunks.push(BasisChunk { offset, key });

            first_locations.entry(key).or_insert(offset);

            expected_offset = expected_offset
                .checked_add(u64::from(length))
                .ok_or_else(|| invalid_wire("basis chunk offsets overflowed"))?;
        }

        if expected_offset != file_bytes {
            return Err(invalid_wire(format!(
                "basis chunks describe {expected_offset} bytes, \
                 but the header declares {file_bytes}",
            )));
        }

        Ok(Self {
            average_kib,
            file_bytes,
            chunks,
            first_locations,
            build_elapsed: Duration::ZERO,
        })
    }
}

impl BasisIndexBenchReport {
    pub fn print(&self) {
        let receiver_elapsed = self.build_elapsed + self.encode_elapsed;

        let sender_elapsed = self.decode_elapsed + self.lookup_elapsed;

        let total_elapsed = receiver_elapsed + sender_elapsed;

        let wire_bytes_per_chunk = if self.chunks == 0 {
            0.0
        } else {
            self.wire_bytes as f64 / self.chunks as f64
        };

        println!("Receiver basis-file index benchmark complete",);

        println!("  Target average:         {} KiB", self.average_kib,);

        println!(
            "  Basis data:             {} bytes",
            format_bytes(self.file_bytes),
        );

        println!(
            "  Basis chunks:           {} total / {} unique",
            format_bytes(self.chunks),
            format_bytes(self.unique_chunks),
        );

        println!(
            "  Wire index:             {} bytes / {:.2} bytes per chunk",
            format_bytes(self.wire_bytes),
            wire_bytes_per_chunk,
        );

        println!(
            "  Verified lookups:       {} / {}",
            format_bytes(self.lookup_hits),
            format_bytes(self.chunks),
        );

        println!();

        println!(
            "  Receiver index build:   {:.6} s / {:.2} MB/s",
            self.build_elapsed.as_secs_f64(),
            decimal_megabytes_per_second(self.file_bytes, self.build_elapsed,),
        );

        println!(
            "  Receiver wire encode:   {:.6} s",
            self.encode_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender wire decode:     {:.6} s",
            self.decode_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender lookup sweep:    {:.6} s",
            self.lookup_elapsed.as_secs_f64(),
        );

        println!(
            "  Receiver total:         {:.6} s",
            receiver_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender index total:     {:.6} s",
            sender_elapsed.as_secs_f64(),
        );

        println!(
            "  Measured pipeline:      {:.6} s",
            total_elapsed.as_secs_f64(),
        );
    }
}

pub fn run(basis_path: &Path, average_kib: usize) -> io::Result<BasisIndexBenchReport> {
    let index = BasisFileIndex::build(basis_path, average_kib)?;

    let encode_started = Instant::now();

    let wire = index.encode_wire()?;

    let encode_elapsed = encode_started.elapsed();

    let decode_started = Instant::now();

    let decoded = BasisFileIndex::decode_wire(&wire)?;

    let decode_elapsed = decode_started.elapsed();

    if decoded.average_kib() != index.average_kib()
        || decoded.file_bytes() != index.file_bytes()
        || decoded.chunks() != index.chunks()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "basis index changed during wire round-trip",
        ));
    }

    let lookup_started = Instant::now();

    let mut lookup_hits = 0_u64;

    for chunk in index.chunks() {
        if decoded.find(&chunk.key).is_some() {
            lookup_hits = lookup_hits
                .checked_add(1)
                .ok_or_else(|| io::Error::other("basis index lookup count overflowed"))?;
        }
    }

    let lookup_elapsed = lookup_started.elapsed();

    let chunks = u64::try_from(index.chunks().len())
        .map_err(|_| io::Error::other("basis chunk count cannot be represented"))?;

    if lookup_hits != chunks {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "basis index lookup sweep found \
                 {lookup_hits} of {chunks} chunks",
            ),
        ));
    }

    Ok(BasisIndexBenchReport {
        average_kib,
        file_bytes: index.file_bytes(),
        chunks,
        unique_chunks: u64::try_from(index.unique_chunk_count())
            .map_err(|_| io::Error::other("unique chunk count cannot be represented"))?,
        wire_bytes: u64::try_from(wire.len())
            .map_err(|_| io::Error::other("wire index length cannot be represented"))?,
        lookup_hits,

        build_elapsed: index.build_elapsed(),

        encode_elapsed,
        decode_elapsed,
        lookup_elapsed,
    })
}

fn wire_size_for_chunks(chunk_count: usize) -> io::Result<usize> {
    chunk_count
        .checked_mul(WIRE_RECORD_BYTES)
        .and_then(|records| WIRE_HEADER_BYTES.checked_add(records))
        .ok_or_else(|| io::Error::other("basis index wire size overflowed"))
}

fn read_u32(encoded: &[u8], cursor: &mut usize) -> io::Result<u32> {
    let end = cursor
        .checked_add(4)
        .ok_or_else(|| invalid_wire("basis index cursor overflowed"))?;

    let bytes = encoded
        .get(*cursor..end)
        .ok_or_else(|| invalid_wire("basis index is truncated"))?;

    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);

    *cursor = end;

    Ok(u32::from_le_bytes(value))
}

fn read_u64(encoded: &[u8], cursor: &mut usize) -> io::Result<u64> {
    let end = cursor
        .checked_add(8)
        .ok_or_else(|| invalid_wire("basis index cursor overflowed"))?;

    let bytes = encoded
        .get(*cursor..end)
        .ok_or_else(|| invalid_wire("basis index is truncated"))?;

    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);

    *cursor = end;

    Ok(u64::from_le_bytes(value))
}

fn read_digest(encoded: &[u8], cursor: &mut usize) -> io::Result<[u8; 32]> {
    let end = cursor
        .checked_add(32)
        .ok_or_else(|| invalid_wire("basis index cursor overflowed"))?;

    let bytes = encoded
        .get(*cursor..end)
        .ok_or_else(|| invalid_wire("basis index digest is truncated"))?;

    let mut digest = [0_u8; 32];
    digest.copy_from_slice(bytes);

    *cursor = end;

    Ok(digest)
}

fn invalid_wire(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::{BasisFileIndex, WIRE_HEADER_BYTES, WIRE_RECORD_BYTES};
    use std::io::Cursor;

    #[test]
    fn wire_round_trip_preserves_basis_index() {
        let bytes = deterministic_bytes(1024 * 1024, 0x1234_5678_90AB_CDEF);

        let index =
            BasisFileIndex::build_from_reader(Cursor::new(&bytes), bytes.len() as u64, 64).unwrap();

        let wire = index.encode_wire().unwrap();

        let decoded = BasisFileIndex::decode_wire(&wire).unwrap();

        assert_eq!(decoded.average_kib(), index.average_kib(),);

        assert_eq!(decoded.file_bytes(), index.file_bytes(),);

        assert_eq!(decoded.chunks(), index.chunks(),);

        for chunk in index.chunks() {
            assert!(decoded.find(&chunk.key).is_some(),);
        }
    }

    #[test]
    fn decode_rejects_truncated_wire_index() {
        let bytes = deterministic_bytes(1024 * 1024, 0xCAFE_BABE_1234_5678);

        let index =
            BasisFileIndex::build_from_reader(Cursor::new(&bytes), bytes.len() as u64, 64).unwrap();

        let mut wire = index.encode_wire().unwrap();

        wire.pop();

        assert!(BasisFileIndex::decode_wire(&wire).is_err(),);
    }

    #[test]
    fn decode_rejects_noncontiguous_offsets() {
        let bytes = deterministic_bytes(1024 * 1024, 0xA5A5_5A5A_DEAD_BEEF);

        let index =
            BasisFileIndex::build_from_reader(Cursor::new(&bytes), bytes.len() as u64, 64).unwrap();

        assert!(index.chunks().len() > 1);

        let mut wire = index.encode_wire().unwrap();

        let second_offset = WIRE_HEADER_BYTES + WIRE_RECORD_BYTES;

        wire[second_offset..second_offset + 8].copy_from_slice(&0_u64.to_le_bytes());

        assert!(BasisFileIndex::decode_wire(&wire).is_err(),);
    }

    fn deterministic_bytes(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;

        let mut bytes = Vec::with_capacity(length);

        for _ in 0..length {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;

            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D_u64);

            bytes.push(state as u8);
        }

        bytes
    }
}
