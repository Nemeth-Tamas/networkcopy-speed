use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::mem::size_of;
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_BLOCK_KIB: usize = 64;

const MIN_BLOCK_KIB: usize = 4;
const MAX_BLOCK_KIB: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct BlockKey {
    digest: [u8; 32],
    length: u32,
}

#[derive(Debug)]
struct BasisIndex {
    ordered: Vec<BlockKey>,
    first_locations: HashMap<BlockKey, u64>,
    bytes: u64,
    elapsed: Duration,
}

#[derive(Debug, Default)]
struct CandidateStats {
    bytes: u64,
    blocks: u64,
    positional_blocks: u64,
    positional_bytes: u64,
    relocated_blocks: u64,
    relocated_bytes: u64,
    literal_blocks: u64,
    literal_bytes: u64,
    elapsed: Duration,
}

#[derive(Debug)]
pub struct FixedBlockDedupReport {
    pub block_bytes: usize,
    pub basis_bytes: u64,
    pub basis_blocks: u64,
    pub unique_basis_blocks: u64,
    pub candidate_bytes: u64,
    pub candidate_blocks: u64,
    pub positional_blocks: u64,
    pub positional_bytes: u64,
    pub relocated_blocks: u64,
    pub relocated_bytes: u64,
    pub literal_blocks: u64,
    pub literal_bytes: u64,
    pub index_payload_bytes: u64,
    pub basis_elapsed: Duration,
    pub candidate_elapsed: Duration,
}

impl FixedBlockDedupReport {
    pub fn print(&self) {
        let reusable_blocks = self.positional_blocks + self.relocated_blocks;

        let reusable_bytes = self.positional_bytes + self.relocated_bytes;

        println!("Fixed-block deduplication baseline complete");
        println!("  Block size:             {} KiB", self.block_bytes / 1024,);
        println!(
            "  Basis data:             {} bytes",
            format_bytes(self.basis_bytes),
        );
        println!(
            "  Basis blocks:           {} total / {} unique / {} duplicate",
            format_bytes(self.basis_blocks),
            format_bytes(self.unique_basis_blocks),
            format_bytes(self.basis_blocks.saturating_sub(self.unique_basis_blocks),),
        );
        println!(
            "  Candidate data:         {} bytes",
            format_bytes(self.candidate_bytes),
        );
        println!(
            "  Candidate blocks:       {}",
            format_bytes(self.candidate_blocks),
        );
        println!();
        println!(
            "  Same-position reuse:    {} blocks / {} bytes ({:.2}%)",
            format_bytes(self.positional_blocks),
            format_bytes(self.positional_bytes),
            percent(self.positional_bytes, self.candidate_bytes),
        );
        println!(
            "  Relocated reuse:        {} blocks / {} bytes ({:.2}%)",
            format_bytes(self.relocated_blocks),
            format_bytes(self.relocated_bytes),
            percent(self.relocated_bytes, self.candidate_bytes),
        );
        println!(
            "  Total reusable:         {} blocks / {} bytes ({:.2}%)",
            format_bytes(reusable_blocks),
            format_bytes(reusable_bytes),
            percent(reusable_bytes, self.candidate_bytes),
        );
        println!(
            "  Literal transmission:   {} blocks / {} bytes ({:.2}%)",
            format_bytes(self.literal_blocks),
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
        println!("  Boundary model:         fixed alignment from byte zero",);
    }
}

pub fn validate_block_kib(block_kib: usize) -> io::Result<()> {
    if !(MIN_BLOCK_KIB..=MAX_BLOCK_KIB).contains(&block_kib) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "fixed dedup block size must be between \
                 {MIN_BLOCK_KIB} and {MAX_BLOCK_KIB} KiB",
            ),
        ));
    }

    if !block_kib.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixed dedup block size must be a power of two",
        ));
    }

    Ok(())
}

pub fn run(
    basis_path: &Path,
    candidate_path: &Path,
    block_kib: usize,
) -> io::Result<FixedBlockDedupReport> {
    validate_block_kib(block_kib)?;

    let block_bytes = block_kib
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("block size overflowed"))?;

    let basis_file = File::open(basis_path)?;

    let expected_basis_bytes = basis_file.metadata()?.len();

    let basis = index_basis(
        BufReader::with_capacity(reader_capacity(block_bytes), basis_file),
        block_bytes,
    )?;

    if basis.bytes != expected_basis_bytes {
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

    let candidate = scan_candidate(
        BufReader::with_capacity(reader_capacity(block_bytes), candidate_file),
        block_bytes,
        &basis,
    )?;

    if candidate.bytes != expected_candidate_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "candidate file changed while being scanned: {}",
                candidate_path.display(),
            ),
        ));
    }

    let basis_blocks = u64::try_from(basis.ordered.len())
        .map_err(|_| io::Error::other("basis block count cannot be represented"))?;

    let unique_basis_blocks = u64::try_from(basis.first_locations.len())
        .map_err(|_| io::Error::other("unique basis block count cannot be represented"))?;

    let index_payload_bytes = index_payload_bytes(&basis)?;

    Ok(FixedBlockDedupReport {
        block_bytes,
        basis_bytes: basis.bytes,
        basis_blocks,
        unique_basis_blocks,
        candidate_bytes: candidate.bytes,
        candidate_blocks: candidate.blocks,
        positional_blocks: candidate.positional_blocks,
        positional_bytes: candidate.positional_bytes,
        relocated_blocks: candidate.relocated_blocks,
        relocated_bytes: candidate.relocated_bytes,
        literal_blocks: candidate.literal_blocks,
        literal_bytes: candidate.literal_bytes,
        index_payload_bytes,
        basis_elapsed: basis.elapsed,
        candidate_elapsed: candidate.elapsed,
    })
}

fn index_basis(mut reader: impl Read, block_bytes: usize) -> io::Result<BasisIndex> {
    let started = Instant::now();

    let mut ordered = Vec::new();
    let mut first_locations = HashMap::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; block_bytes];

    loop {
        let read = read_block(&mut reader, &mut buffer)?;

        if read == 0 {
            break;
        }

        let key = block_key(&buffer[..read])?;

        let block_index = u64::try_from(ordered.len())
            .map_err(|_| io::Error::other("basis block index cannot be represented"))?;

        first_locations.entry(key).or_insert(block_index);

        ordered.push(key);

        bytes = bytes
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| io::Error::other("basis block length cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("basis byte count overflowed"))?;
    }

    Ok(BasisIndex {
        ordered,
        first_locations,
        bytes,
        elapsed: started.elapsed(),
    })
}

fn scan_candidate(
    mut reader: impl Read,
    block_bytes: usize,
    basis: &BasisIndex,
) -> io::Result<CandidateStats> {
    let started = Instant::now();

    let mut stats = CandidateStats::default();
    let mut buffer = vec![0_u8; block_bytes];

    loop {
        let read = read_block(&mut reader, &mut buffer)?;

        if read == 0 {
            break;
        }

        let key = block_key(&buffer[..read])?;

        let block_bytes = u64::try_from(read)
            .map_err(|_| io::Error::other("candidate block length cannot be represented"))?;

        let block_index = stats.blocks;

        let same_position = usize::try_from(block_index)
            .ok()
            .and_then(|index| basis.ordered.get(index))
            .is_some_and(|basis_key| *basis_key == key);

        if same_position {
            stats.positional_blocks += 1;
            stats.positional_bytes += block_bytes;
        } else if basis.first_locations.contains_key(&key) {
            stats.relocated_blocks += 1;
            stats.relocated_bytes += block_bytes;
        } else {
            stats.literal_blocks += 1;
            stats.literal_bytes += block_bytes;
        }

        stats.blocks = stats
            .blocks
            .checked_add(1)
            .ok_or_else(|| io::Error::other("candidate block count overflowed"))?;

        stats.bytes = stats
            .bytes
            .checked_add(block_bytes)
            .ok_or_else(|| io::Error::other("candidate byte count overflowed"))?;
    }

    stats.elapsed = started.elapsed();

    Ok(stats)
}

fn read_block(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0_usize;

    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,

            Ok(read) => {
                filled = filled
                    .checked_add(read)
                    .ok_or_else(|| io::Error::other("block read length overflowed"))?;
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}

fn block_key(contents: &[u8]) -> io::Result<BlockKey> {
    let length = u32::try_from(contents.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixed dedup block is larger than u32",
        )
    })?;

    Ok(BlockKey {
        digest: *blake3::hash(contents).as_bytes(),
        length,
    })
}

fn reader_capacity(block_bytes: usize) -> usize {
    block_bytes.clamp(64 * 1024, 1024 * 1024)
}

fn index_payload_bytes(basis: &BasisIndex) -> io::Result<u64> {
    let ordered_bytes = basis
        .ordered
        .len()
        .checked_mul(size_of::<BlockKey>())
        .ok_or_else(|| io::Error::other("ordered basis index size overflowed"))?;

    let lookup_bytes = basis
        .first_locations
        .len()
        .checked_mul(size_of::<(BlockKey, u64)>())
        .ok_or_else(|| io::Error::other("basis lookup index size overflowed"))?;

    let total = ordered_bytes
        .checked_add(lookup_bytes)
        .ok_or_else(|| io::Error::other("basis index payload size overflowed"))?;

    u64::try_from(total)
        .map_err(|_| io::Error::other("basis index payload size cannot be represented"))
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::{index_basis, scan_candidate, validate_block_kib};
    use std::io::Cursor;

    #[test]
    fn validates_power_of_two_block_sizes() {
        assert!(validate_block_kib(4).is_ok());
        assert!(validate_block_kib(64).is_ok());
        assert!(validate_block_kib(16 * 1024).is_ok());

        assert!(validate_block_kib(0).is_err());
        assert!(validate_block_kib(3).is_err());
        assert!(validate_block_kib(96).is_err());
        assert!(validate_block_kib(32 * 1024).is_err());
    }

    #[test]
    fn exact_candidate_reuses_every_block_positionally() {
        let basis = index_basis(Cursor::new(b"AAAABBBBCCCC"), 4).unwrap();

        let candidate = scan_candidate(Cursor::new(b"AAAABBBBCCCC"), 4, &basis).unwrap();

        assert_eq!(candidate.blocks, 3);
        assert_eq!(candidate.positional_blocks, 3);
        assert_eq!(candidate.positional_bytes, 12);
        assert_eq!(candidate.relocated_blocks, 0);
        assert_eq!(candidate.literal_bytes, 0);
    }

    #[test]
    fn block_aligned_insertion_becomes_relocated_reuse() {
        let basis = index_basis(Cursor::new(b"AAAABBBBCCCC"), 4).unwrap();

        let candidate = scan_candidate(Cursor::new(b"XXXXAAAABBBBCCCC"), 4, &basis).unwrap();

        assert_eq!(candidate.positional_blocks, 0);
        assert_eq!(candidate.relocated_blocks, 3);
        assert_eq!(candidate.relocated_bytes, 12);
        assert_eq!(candidate.literal_bytes, 4);
    }

    #[test]
    fn unaligned_insertion_destroys_fixed_block_reuse() {
        let basis = index_basis(Cursor::new(b"AAAABBBBCCCC"), 4).unwrap();

        let candidate = scan_candidate(Cursor::new(b"XAAAABBBBCCCC"), 4, &basis).unwrap();

        assert_eq!(candidate.positional_blocks, 0);
        assert_eq!(candidate.relocated_blocks, 0);
        assert_eq!(candidate.literal_bytes, 13);
    }
}
