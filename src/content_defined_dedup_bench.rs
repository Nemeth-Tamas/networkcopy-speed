use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::mem::size_of;
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_AVERAGE_KIB: usize = 64;

const MIN_AVERAGE_KIB: usize = 4;
const MAX_AVERAGE_KIB: usize = 4 * 1024;

const BOUNDARY_HISTORY_BYTES: usize = u64::BITS as usize;

const READ_BUFFER_BYTES: usize = 1024 * 1024;

const GEAR_TABLE: [u64; 256] = build_gear_table();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ChunkKey {
    pub(crate) digest: [u8; 32],
    pub(crate) length: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkConfig {
    pub(crate) average_bytes: usize,
    pub(crate) minimum_bytes: usize,
    pub(crate) maximum_bytes: usize,
    pub(crate) boundary_mask: u64,
}

#[derive(Debug)]
struct BasisIndex {
    by_offset: HashMap<u64, ChunkKey>,
    first_locations: HashMap<ChunkKey, u64>,
    chunking: ChunkingStats,
}

#[derive(Debug, Default)]
pub(crate) struct ChunkingStats {
    pub(crate) bytes: u64,
    pub(crate) chunks: u64,
    pub(crate) minimum_chunk_bytes: u64,
    pub(crate) maximum_chunk_bytes: u64,
    pub(crate) elapsed: Duration,
}

#[derive(Debug, Default)]
struct CandidateStats {
    chunking: ChunkingStats,

    positional_chunks: u64,
    positional_bytes: u64,

    relocated_chunks: u64,
    relocated_bytes: u64,

    literal_chunks: u64,
    literal_bytes: u64,
}

#[derive(Debug)]
pub struct ContentDefinedDedupReport {
    pub target_average_bytes: usize,
    pub minimum_chunk_bytes: usize,
    pub maximum_chunk_bytes: usize,
    pub boundary_history_bytes: usize,

    pub basis_bytes: u64,
    pub basis_chunks: u64,
    pub unique_basis_chunks: u64,
    pub basis_minimum_chunk_bytes: u64,
    pub basis_maximum_chunk_bytes: u64,

    pub candidate_bytes: u64,
    pub candidate_chunks: u64,
    pub candidate_minimum_chunk_bytes: u64,
    pub candidate_maximum_chunk_bytes: u64,

    pub positional_chunks: u64,
    pub positional_bytes: u64,

    pub relocated_chunks: u64,
    pub relocated_bytes: u64,

    pub literal_chunks: u64,
    pub literal_bytes: u64,

    pub index_payload_bytes: u64,
    pub basis_elapsed: Duration,
    pub candidate_elapsed: Duration,
}

impl ChunkConfig {
    pub(crate) fn from_average_kib(average_kib: usize) -> io::Result<Self> {
        validate_average_kib(average_kib)?;

        let average_bytes = average_kib
            .checked_mul(1024)
            .ok_or_else(|| io::Error::other("content-defined average chunk size overflowed"))?;

        let minimum_bytes = average_bytes / 2;

        let maximum_bytes = average_bytes
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("content-defined maximum chunk size overflowed"))?;

        let boundary_mask = u64::try_from(minimum_bytes - 1)
            .map_err(|_| io::Error::other("content-defined boundary mask cannot be represented"))?;

        Ok(Self {
            average_bytes,
            minimum_bytes,
            maximum_bytes,
            boundary_mask,
        })
    }
}

impl ContentDefinedDedupReport {
    pub fn print(&self) {
        let reusable_chunks = self.positional_chunks + self.relocated_chunks;

        let reusable_bytes = self.positional_bytes + self.relocated_bytes;

        println!("Content-defined deduplication prototype complete",);

        println!(
            "  Target average:         {} KiB",
            self.target_average_bytes / 1024,
        );

        println!(
            "  Chunk limits:           {} KiB minimum / {} KiB maximum",
            self.minimum_chunk_bytes / 1024,
            self.maximum_chunk_bytes / 1024,
        );

        println!(
            "  Boundary history:       {} bytes",
            self.boundary_history_bytes,
        );

        println!();

        println!(
            "  Basis data:             {} bytes",
            format_bytes(self.basis_bytes),
        );

        println!(
            "  Basis chunks:           {} total / {} unique / {} duplicate",
            format_bytes(self.basis_chunks),
            format_bytes(self.unique_basis_chunks),
            format_bytes(self.basis_chunks.saturating_sub(self.unique_basis_chunks),),
        );

        println!(
            "  Basis actual chunks:    {:.2} KiB average / {} min / {} max",
            average_chunk_bytes(self.basis_bytes, self.basis_chunks,) / 1024.0,
            format_bytes(self.basis_minimum_chunk_bytes),
            format_bytes(self.basis_maximum_chunk_bytes),
        );

        println!(
            "  Candidate data:         {} bytes",
            format_bytes(self.candidate_bytes),
        );

        println!(
            "  Candidate chunks:       {}",
            format_bytes(self.candidate_chunks),
        );

        println!(
            "  Candidate chunks:       {:.2} KiB average / {} min / {} max",
            average_chunk_bytes(self.candidate_bytes, self.candidate_chunks,) / 1024.0,
            format_bytes(self.candidate_minimum_chunk_bytes),
            format_bytes(self.candidate_maximum_chunk_bytes),
        );

        println!();

        println!(
            "  Same-position reuse:    {} chunks / {} bytes ({:.2}%)",
            format_bytes(self.positional_chunks),
            format_bytes(self.positional_bytes),
            percent(self.positional_bytes, self.candidate_bytes,),
        );

        println!(
            "  Relocated reuse:        {} chunks / {} bytes ({:.2}%)",
            format_bytes(self.relocated_chunks),
            format_bytes(self.relocated_bytes),
            percent(self.relocated_bytes, self.candidate_bytes,),
        );

        println!(
            "  Total reusable:         {} chunks / {} bytes ({:.2}%)",
            format_bytes(reusable_chunks),
            format_bytes(reusable_bytes),
            percent(reusable_bytes, self.candidate_bytes),
        );

        println!(
            "  Literal transmission:   {} chunks / {} bytes ({:.2}%)",
            format_bytes(self.literal_chunks),
            format_bytes(self.literal_bytes),
            percent(self.literal_bytes, self.candidate_bytes),
        );

        println!(
            "  Index payload minimum:  {} bytes",
            format_bytes(self.index_payload_bytes),
        );

        println!();

        println!(
            "  Basis indexing:         {:.6} s / {:.2} MB/s",
            self.basis_elapsed.as_secs_f64(),
            decimal_megabytes_per_second(self.basis_bytes, self.basis_elapsed,),
        );

        println!(
            "  Candidate scanning:     {:.6} s / {:.2} MB/s",
            self.candidate_elapsed.as_secs_f64(),
            decimal_megabytes_per_second(self.candidate_bytes, self.candidate_elapsed,),
        );

        println!(
            "  Estimated wire payload: {} bytes before reference metadata",
            format_bytes(self.literal_bytes),
        );

        println!("  Boundary model:         rolling content-defined boundaries",);
    }
}

pub fn validate_average_kib(average_kib: usize) -> io::Result<()> {
    if !(MIN_AVERAGE_KIB..=MAX_AVERAGE_KIB).contains(&average_kib) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "content-defined average chunk size must be between \
                 {MIN_AVERAGE_KIB} and {MAX_AVERAGE_KIB} KiB",
            ),
        ));
    }

    if !average_kib.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "content-defined average chunk size must be a power of two",
        ));
    }

    Ok(())
}

pub fn run(
    basis_path: &Path,
    candidate_path: &Path,
    average_kib: usize,
) -> io::Result<ContentDefinedDedupReport> {
    let config = ChunkConfig::from_average_kib(average_kib)?;

    let basis_file = File::open(basis_path)?;
    let expected_basis_bytes = basis_file.metadata()?.len();

    let basis = index_basis(basis_file, config)?;

    if basis.chunking.bytes != expected_basis_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "basis file changed while being indexed: {}",
                basis_path.display(),
            ),
        ));
    }

    let candidate_file = File::open(candidate_path)?;

    let expected_candidate_bytes = candidate_file.metadata()?.len();

    let candidate = scan_candidate(candidate_file, config, &basis)?;

    if candidate.chunking.bytes != expected_candidate_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "candidate file changed while being scanned: {}",
                candidate_path.display(),
            ),
        ));
    }

    let unique_basis_chunks = u64::try_from(basis.first_locations.len())
        .map_err(|_| io::Error::other("unique basis chunk count cannot be represented"))?;

    let index_payload_bytes = index_payload_bytes(&basis)?;

    Ok(ContentDefinedDedupReport {
        target_average_bytes: config.average_bytes,
        minimum_chunk_bytes: config.minimum_bytes,
        maximum_chunk_bytes: config.maximum_bytes,
        boundary_history_bytes: BOUNDARY_HISTORY_BYTES,

        basis_bytes: basis.chunking.bytes,
        basis_chunks: basis.chunking.chunks,
        unique_basis_chunks,
        basis_minimum_chunk_bytes: basis.chunking.minimum_chunk_bytes,
        basis_maximum_chunk_bytes: basis.chunking.maximum_chunk_bytes,

        candidate_bytes: candidate.chunking.bytes,
        candidate_chunks: candidate.chunking.chunks,
        candidate_minimum_chunk_bytes: candidate.chunking.minimum_chunk_bytes,
        candidate_maximum_chunk_bytes: candidate.chunking.maximum_chunk_bytes,

        positional_chunks: candidate.positional_chunks,
        positional_bytes: candidate.positional_bytes,

        relocated_chunks: candidate.relocated_chunks,
        relocated_bytes: candidate.relocated_bytes,

        literal_chunks: candidate.literal_chunks,
        literal_bytes: candidate.literal_bytes,

        index_payload_bytes,
        basis_elapsed: basis.chunking.elapsed,
        candidate_elapsed: candidate.chunking.elapsed,
    })
}

fn index_basis(reader: impl Read, config: ChunkConfig) -> io::Result<BasisIndex> {
    let mut by_offset = HashMap::new();
    let mut first_locations = HashMap::new();

    let chunking = chunk_reader(reader, config, |offset, contents| {
        let key = chunk_key(contents)?;

        by_offset.insert(offset, key);

        first_locations.entry(key).or_insert(offset);

        Ok(())
    })?;

    Ok(BasisIndex {
        by_offset,
        first_locations,
        chunking,
    })
}

fn scan_candidate(
    reader: impl Read,
    config: ChunkConfig,
    basis: &BasisIndex,
) -> io::Result<CandidateStats> {
    let mut candidate = CandidateStats::default();

    candidate.chunking = chunk_reader(reader, config, |offset, contents| {
        let key = chunk_key(contents)?;

        let chunk_bytes = u64::try_from(contents.len())
            .map_err(|_| io::Error::other("candidate chunk length cannot be represented"))?;

        let same_position = basis
            .by_offset
            .get(&offset)
            .is_some_and(|basis_key| *basis_key == key);

        if same_position {
            candidate.positional_chunks += 1;
            candidate.positional_bytes += chunk_bytes;
        } else if basis.first_locations.contains_key(&key) {
            candidate.relocated_chunks += 1;
            candidate.relocated_bytes += chunk_bytes;
        } else {
            candidate.literal_chunks += 1;
            candidate.literal_bytes += chunk_bytes;
        }

        Ok(())
    })?;

    Ok(candidate)
}

pub(crate) fn chunk_reader(
    mut reader: impl Read,
    config: ChunkConfig,
    mut visitor: impl FnMut(u64, &[u8]) -> io::Result<()>,
) -> io::Result<ChunkingStats> {
    let started = Instant::now();

    let mut stats = ChunkingStats::default();

    let mut boundary_hash = GearHash::new();

    let mut read_buffer = vec![0_u8; READ_BUFFER_BYTES];

    let mut chunk = Vec::with_capacity(config.maximum_bytes);

    let mut chunk_start = 0_u64;

    loop {
        let read = match reader.read(&mut read_buffer) {
            Ok(0) => break,

            Ok(read) => read,

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue;
            }

            Err(error) => return Err(error),
        };

        for &byte in &read_buffer[..read] {
            boundary_hash.push(byte);
            chunk.push(byte);

            let chunk_bytes = chunk.len();

            let boundary = chunk_bytes >= config.minimum_bytes
                && (boundary_hash.value() & config.boundary_mask == 0
                    || chunk_bytes >= config.maximum_bytes);

            if boundary {
                emit_chunk(chunk_start, &chunk, &mut visitor, &mut stats)?;

                let emitted_bytes = u64::try_from(chunk.len())
                    .map_err(|_| io::Error::other("emitted chunk length cannot be represented"))?;

                chunk_start = chunk_start
                    .checked_add(emitted_bytes)
                    .ok_or_else(|| io::Error::other("content-defined chunk offset overflowed"))?;

                chunk.clear();
                boundary_hash.reset();
            }
        }
    }

    if !chunk.is_empty() {
        emit_chunk(chunk_start, &chunk, &mut visitor, &mut stats)?;
    }

    stats.elapsed = started.elapsed();

    Ok(stats)
}

fn emit_chunk(
    offset: u64,
    contents: &[u8],
    visitor: &mut impl FnMut(u64, &[u8]) -> io::Result<()>,
    stats: &mut ChunkingStats,
) -> io::Result<()> {
    visitor(offset, contents)?;

    let chunk_bytes = u64::try_from(contents.len())
        .map_err(|_| io::Error::other("content-defined chunk length cannot be represented"))?;

    stats.bytes = stats
        .bytes
        .checked_add(chunk_bytes)
        .ok_or_else(|| io::Error::other("content-defined byte count overflowed"))?;

    stats.chunks = stats
        .chunks
        .checked_add(1)
        .ok_or_else(|| io::Error::other("content-defined chunk count overflowed"))?;

    if stats.chunks == 1 {
        stats.minimum_chunk_bytes = chunk_bytes;
        stats.maximum_chunk_bytes = chunk_bytes;
    } else {
        stats.minimum_chunk_bytes = stats.minimum_chunk_bytes.min(chunk_bytes);

        stats.maximum_chunk_bytes = stats.maximum_chunk_bytes.max(chunk_bytes);
    }

    Ok(())
}

pub(crate) fn chunk_key(contents: &[u8]) -> io::Result<ChunkKey> {
    let length = u32::try_from(contents.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "content-defined chunk is larger than u32",
        )
    })?;

    Ok(ChunkKey {
        digest: *blake3::hash(contents).as_bytes(),
        length,
    })
}

fn index_payload_bytes(basis: &BasisIndex) -> io::Result<u64> {
    let position_bytes = basis
        .by_offset
        .len()
        .checked_mul(size_of::<(u64, ChunkKey)>())
        .ok_or_else(|| io::Error::other("content-defined position index size overflowed"))?;

    let lookup_bytes = basis
        .first_locations
        .len()
        .checked_mul(size_of::<(ChunkKey, u64)>())
        .ok_or_else(|| io::Error::other("content-defined lookup index size overflowed"))?;

    let total = position_bytes
        .checked_add(lookup_bytes)
        .ok_or_else(|| io::Error::other("content-defined index size overflowed"))?;

    u64::try_from(total)
        .map_err(|_| io::Error::other("content-defined index size cannot be represented"))
}

fn average_chunk_bytes(bytes: u64, chunks: u64) -> f64 {
    if chunks == 0 {
        0.0
    } else {
        bytes as f64 / chunks as f64
    }
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

#[derive(Debug, Default)]
struct GearHash {
    value: u64,
}

impl GearHash {
    fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn push(&mut self, byte: u8) {
        self.value = self
            .value
            .wrapping_shl(1)
            .wrapping_add(GEAR_TABLE[usize::from(byte)]);
    }

    #[inline]
    fn value(&self) -> u64 {
        self.value
    }

    #[inline]
    fn reset(&mut self) {
        self.value = 0;
    }
}

const fn build_gear_table() -> [u64; 256] {
    let mut table = [0_u64; 256];

    let mut state = 0x9E37_79B9_7F4A_7C15_u64;

    let mut index = 0_usize;

    while index < table.len() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;

        table[index] = state.wrapping_mul(0x2545_F491_4F6C_DD1D_u64);

        index += 1;
    }

    table
}

#[cfg(test)]
mod tests {
    use super::{
        ChunkConfig, GearHash, chunk_reader, index_basis, scan_candidate, validate_average_kib,
    };
    use std::io::Cursor;

    #[test]
    fn validates_power_of_two_average_sizes() {
        assert!(validate_average_kib(4).is_ok());
        assert!(validate_average_kib(64).is_ok());
        assert!(validate_average_kib(4096).is_ok());

        assert!(validate_average_kib(0).is_err());
        assert!(validate_average_kib(3).is_err());
        assert!(validate_average_kib(96).is_err());
        assert!(validate_average_kib(8192).is_err());
    }

    #[test]
    fn gear_hash_forgets_old_prefix_after_64_bytes() {
        let shared = deterministic_bytes(64, 0xCAFE_BABE_DEAD_BEEF);

        let mut first = GearHash::new();
        let mut second = GearHash::new();

        first.push(0x11);
        second.push(0xEE);

        for byte in shared {
            first.push(byte);
            second.push(byte);
        }

        assert_eq!(first.value(), second.value());
    }

    #[test]
    fn exact_candidate_reuses_every_chunk() {
        let config = ChunkConfig::from_average_kib(16).unwrap();

        let basis_bytes = deterministic_bytes(1024 * 1024, 0x1234_5678_9ABC_DEF0);

        let basis = index_basis(Cursor::new(&basis_bytes), config).unwrap();

        let candidate = scan_candidate(Cursor::new(&basis_bytes), config, &basis).unwrap();

        assert_eq!(candidate.chunking.bytes, basis.chunking.bytes,);

        assert_eq!(candidate.positional_bytes, candidate.chunking.bytes,);

        assert_eq!(candidate.relocated_bytes, 0);
        assert_eq!(candidate.literal_bytes, 0);
    }

    #[test]
    fn unaligned_insertion_resynchronizes() {
        let config = ChunkConfig::from_average_kib(64).unwrap();

        let basis_bytes = deterministic_bytes(1024 * 1024, 0x0123_4567_89AB_CDEF);

        let insertion_offset = 256 * 1024 + 123;

        let insertion = deterministic_bytes(4097, 0xFEDC_BA98_7654_3210);

        let mut candidate_bytes = Vec::with_capacity(basis_bytes.len() + insertion.len());

        candidate_bytes.extend_from_slice(&basis_bytes[..insertion_offset]);

        candidate_bytes.extend_from_slice(&insertion);

        candidate_bytes.extend_from_slice(&basis_bytes[insertion_offset..]);

        let basis = index_basis(Cursor::new(&basis_bytes), config).unwrap();

        let candidate = scan_candidate(Cursor::new(&candidate_bytes), config, &basis).unwrap();

        let maximum_literal_bytes =
            u64::try_from(config.maximum_bytes * 2 + insertion.len()).unwrap();

        assert!(
            candidate.relocated_bytes > candidate.chunking.bytes / 2,
            "CDC did not recover enough relocated content after insertion",
        );

        assert!(
            candidate.literal_bytes <= maximum_literal_bytes,
            "insertion caused {} literal bytes; expected at most {}",
            candidate.literal_bytes,
            maximum_literal_bytes,
        );

        assert!(candidate.relocated_bytes > 0);
        assert!(candidate.literal_bytes > 0);
    }

    #[test]
    fn unaligned_deletion_resynchronizes() {
        let config = ChunkConfig::from_average_kib(64).unwrap();

        let basis_bytes = deterministic_bytes(1024 * 1024, 0x1122_3344_5566_7788);

        let deletion_offset = 256 * 1024 + 123;

        let deletion_bytes = 4097;

        let mut candidate_bytes = Vec::with_capacity(basis_bytes.len() - deletion_bytes);

        candidate_bytes.extend_from_slice(&basis_bytes[..deletion_offset]);

        candidate_bytes.extend_from_slice(&basis_bytes[deletion_offset + deletion_bytes..]);

        let basis = index_basis(Cursor::new(&basis_bytes), config).unwrap();

        let candidate = scan_candidate(Cursor::new(&candidate_bytes), config, &basis).unwrap();

        let maximum_literal_bytes =
            u64::try_from(config.maximum_bytes * 2 + deletion_bytes).unwrap();

        assert!(
            candidate.relocated_bytes > candidate.chunking.bytes / 2,
            "CDC did not recover enough relocated content after deletion",
        );

        assert!(
            candidate.literal_bytes <= maximum_literal_bytes,
            "deletion caused {} literal bytes; expected at most {}",
            candidate.literal_bytes,
            maximum_literal_bytes,
        );

        assert!(candidate.relocated_bytes > 0);
        assert!(candidate.literal_bytes > 0);
    }

    #[test]
    fn chunks_obey_limits_except_for_final_tail() {
        let config = ChunkConfig::from_average_kib(16).unwrap();

        let bytes = deterministic_bytes(1024 * 1024, 0xA5A5_5A5A_1234_5678);

        let mut lengths = Vec::new();

        chunk_reader(Cursor::new(bytes), config, |_, contents| {
            lengths.push(contents.len());
            Ok(())
        })
        .unwrap();

        assert!(lengths.len() > 1);

        for length in &lengths[..lengths.len() - 1] {
            assert!(*length >= config.minimum_bytes);
            assert!(*length <= config.maximum_bytes);
        }

        let final_length = *lengths.last().unwrap();

        assert!(final_length > 0);
        assert!(final_length <= config.maximum_bytes);
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
