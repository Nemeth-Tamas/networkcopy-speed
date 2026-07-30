use crate::cdc_basis_index::BasisFileIndex;
use crate::content_defined_dedup_bench::{ChunkConfig, chunk_key, chunk_reader};
use crate::content_hash::{ContentHasher, format_digest};
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::windows_file_replace;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PLAN_MAGIC: [u8; 4] = *b"NCP1";

const PLAN_HEADER_BYTES: usize = 4 + 8 + 32 + 8;

const BASIS_RECORD_BYTES: usize = 1 + 8 + 8;

const LITERAL_HEADER_BYTES: usize = 1 + 8;

const OP_BASIS: u8 = 0;
const OP_LITERAL: u8 = 1;

const RECONSTRUCTION_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct LiteralLimitExceeded {
    limit_bytes: u64,
}

impl fmt::Display for LiteralLimitExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "reconstruction literal staging exceeded {} bytes",
            self.limit_bytes,
        )
    }
}

impl Error for LiteralLimitExceeded {}

pub(crate) fn is_literal_limit_exceeded(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<LiteralLimitExceeded>())
        .is_some()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReconstructionOp {
    Basis { offset: u64, length: u64 },

    Literal(Vec<u8>),
}

#[derive(Clone, Debug)]
pub(crate) struct ReconstructionPlan {
    target_bytes: u64,
    target_digest: [u8; 32],

    operations: Vec<ReconstructionOp>,

    candidate_chunks: u64,
    referenced_bytes: u64,
    literal_bytes: u64,

    build_elapsed: Duration,
}

#[derive(Debug)]
struct ReconstructionStats {
    digest: [u8; 32],
    elapsed: Duration,
}

#[derive(Debug)]
pub struct ReconstructionBenchReport {
    pub average_kib: usize,

    pub basis_bytes: u64,
    pub candidate_bytes: u64,

    pub index_chunks: u64,
    pub candidate_chunks: u64,

    pub basis_ranges: u64,
    pub literal_ranges: u64,

    pub referenced_bytes: u64,
    pub literal_bytes: u64,

    pub index_wire_bytes: u64,
    pub plan_wire_bytes: u64,
    pub total_wire_bytes: u64,

    pub final_digest: [u8; 32],

    pub index_build_elapsed: Duration,
    pub index_encode_elapsed: Duration,
    pub index_decode_elapsed: Duration,

    pub plan_build_elapsed: Duration,
    pub plan_encode_elapsed: Duration,
    pub plan_decode_elapsed: Duration,

    pub reconstruction_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl ReconstructionPlan {
    fn build(candidate_path: &Path, index: &BasisFileIndex) -> io::Result<Self> {
        Self::build_bounded(candidate_path, index, u64::MAX)
    }

    pub(crate) fn build_bounded(
        candidate_path: &Path,
        index: &BasisFileIndex,
        maximum_literal_bytes: u64,
    ) -> io::Result<Self> {
        let candidate_file = File::open(candidate_path)?;

        let expected_bytes = candidate_file.metadata()?.len();

        let config = ChunkConfig::from_average_kib(index.average_kib())?;

        let mut operations = Vec::new();

        let mut hasher = ContentHasher::new();

        let mut referenced_bytes = 0_u64;
        let mut literal_bytes = 0_u64;

        let chunking = chunk_reader(candidate_file, config, |_candidate_offset, contents| {
            hasher.update(contents);

            let key = chunk_key(contents)?;

            let chunk_bytes = u64::try_from(contents.len()).map_err(|_| {
                io::Error::other(
                    "candidate chunk length \
                                 cannot be represented",
                )
            })?;

            if let Some(basis_offset) = index.find(&key) {
                append_basis(&mut operations, basis_offset, chunk_bytes)?;

                referenced_bytes = referenced_bytes.checked_add(chunk_bytes).ok_or_else(|| {
                    io::Error::other(
                        "referenced byte count \
                                     overflowed",
                    )
                })?;
            } else {
                let next_literal_bytes = literal_bytes
                    .checked_add(chunk_bytes)
                    .ok_or_else(|| io::Error::other("literal byte count overflowed"))?;

                if next_literal_bytes > maximum_literal_bytes {
                    return Err(io::Error::other(LiteralLimitExceeded {
                        limit_bytes: maximum_literal_bytes,
                    }));
                }

                append_literal(&mut operations, contents)?;

                literal_bytes = next_literal_bytes;
            }

            Ok(())
        })?;

        if chunking.bytes != expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "candidate changed while being planned: \
                     expected {expected_bytes} bytes, read {}",
                    chunking.bytes,
                ),
            ));
        }

        let planned_bytes = referenced_bytes
            .checked_add(literal_bytes)
            .ok_or_else(|| io::Error::other("planned byte count overflowed"))?;

        if planned_bytes != expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reconstruction plan describes \
                     {planned_bytes} bytes, expected \
                     {expected_bytes}",
                ),
            ));
        }

        Ok(Self {
            target_bytes: expected_bytes,
            target_digest: hasher.finalize(),

            operations,

            candidate_chunks: chunking.chunks,
            referenced_bytes,
            literal_bytes,

            build_elapsed: chunking.elapsed,
        })
    }

    pub(crate) fn encode_wire(&self) -> io::Result<Vec<u8>> {
        let operation_count = u64::try_from(self.operations.len()).map_err(|_| {
            io::Error::other(
                "reconstruction operation count \
                     cannot be represented",
            )
        })?;

        let capacity = self.wire_size()?;

        let mut encoded = Vec::with_capacity(capacity);

        encoded.extend_from_slice(&PLAN_MAGIC);

        encoded.extend_from_slice(&self.target_bytes.to_le_bytes());

        encoded.extend_from_slice(&self.target_digest);

        encoded.extend_from_slice(&operation_count.to_le_bytes());

        for operation in &self.operations {
            match operation {
                ReconstructionOp::Basis { offset, length } => {
                    encoded.push(OP_BASIS);

                    encoded.extend_from_slice(&offset.to_le_bytes());

                    encoded.extend_from_slice(&length.to_le_bytes());
                }

                ReconstructionOp::Literal(contents) => {
                    encoded.push(OP_LITERAL);

                    let length = u64::try_from(contents.len()).map_err(|_| {
                        io::Error::other(
                            "literal operation length \
                                 cannot be represented",
                        )
                    })?;

                    encoded.extend_from_slice(&length.to_le_bytes());

                    encoded.extend_from_slice(contents);
                }
            }
        }

        debug_assert_eq!(encoded.len(), capacity,);

        Ok(encoded)
    }

    pub(crate) fn decode_wire(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() < PLAN_HEADER_BYTES {
            return Err(invalid_plan(
                "reconstruction plan header \
                 is truncated",
            ));
        }

        if encoded[..4] != PLAN_MAGIC {
            return Err(invalid_plan(
                "reconstruction plan magic \
                 is invalid",
            ));
        }

        let mut cursor = 4_usize;

        let target_bytes = read_u64(encoded, &mut cursor)?;

        let target_digest = read_digest(encoded, &mut cursor)?;

        let operation_count_u64 = read_u64(encoded, &mut cursor)?;

        let operation_count = usize::try_from(operation_count_u64).map_err(|_| {
            invalid_plan(
                "reconstruction operation count \
                     cannot be represented",
            )
        })?;

        let mut operations = Vec::with_capacity(operation_count);

        let mut referenced_bytes = 0_u64;
        let mut literal_bytes = 0_u64;

        for operation_index in 0..operation_count {
            let tag = read_u8(encoded, &mut cursor)?;

            match tag {
                OP_BASIS => {
                    let offset = read_u64(encoded, &mut cursor)?;

                    let length = read_u64(encoded, &mut cursor)?;

                    if length == 0 {
                        return Err(invalid_plan(
                            "basis operation length \
                             must not be zero",
                        ));
                    }

                    offset.checked_add(length).ok_or_else(|| {
                        invalid_plan(
                            "basis operation range \
                                 overflowed",
                        )
                    })?;

                    referenced_bytes = referenced_bytes.checked_add(length).ok_or_else(|| {
                        invalid_plan(
                            "referenced byte count \
                                     overflowed",
                        )
                    })?;

                    operations.push(ReconstructionOp::Basis { offset, length });
                }

                OP_LITERAL => {
                    let length_u64 = read_u64(encoded, &mut cursor)?;

                    if length_u64 == 0 {
                        return Err(invalid_plan(
                            "literal operation length \
                             must not be zero",
                        ));
                    }

                    let length = usize::try_from(length_u64).map_err(|_| {
                        invalid_plan(
                            "literal operation length \
                                 cannot be represented",
                        )
                    })?;

                    let contents = read_bytes(encoded, &mut cursor, length)?.to_vec();

                    literal_bytes = literal_bytes.checked_add(length_u64).ok_or_else(|| {
                        invalid_plan(
                            "literal byte count \
                                     overflowed",
                        )
                    })?;

                    operations.push(ReconstructionOp::Literal(contents));
                }

                unknown => {
                    return Err(invalid_plan(format!(
                        "reconstruction operation \
                             {operation_index} has unknown \
                             tag {unknown}",
                    )));
                }
            }
        }

        if cursor != encoded.len() {
            return Err(invalid_plan(format!(
                "reconstruction plan contains {} \
                 trailing bytes",
                encoded.len() - cursor,
            )));
        }

        let described_bytes = referenced_bytes.checked_add(literal_bytes).ok_or_else(|| {
            invalid_plan(
                "reconstruction byte count \
                         overflowed",
            )
        })?;

        if described_bytes != target_bytes {
            return Err(invalid_plan(format!(
                "reconstruction operations describe \
                 {described_bytes} bytes, but the header \
                 declares {target_bytes}",
            )));
        }

        Ok(Self {
            target_bytes,
            target_digest,

            operations,

            candidate_chunks: 0,
            referenced_bytes,
            literal_bytes,

            build_elapsed: Duration::ZERO,
        })
    }

    pub(crate) fn target_bytes(&self) -> u64 {
        self.target_bytes
    }

    pub(crate) fn referenced_bytes(&self) -> u64 {
        self.referenced_bytes
    }

    pub(crate) fn literal_bytes(&self) -> u64 {
        self.literal_bytes
    }

    pub(crate) fn build_elapsed(&self) -> Duration {
        self.build_elapsed
    }

    fn wire_size(&self) -> io::Result<usize> {
        let mut size = PLAN_HEADER_BYTES;

        for operation in &self.operations {
            let operation_bytes = match operation {
                ReconstructionOp::Basis { .. } => BASIS_RECORD_BYTES,

                ReconstructionOp::Literal(contents) => LITERAL_HEADER_BYTES
                    .checked_add(contents.len())
                    .ok_or_else(|| {
                        io::Error::other(
                            "literal wire size \
                                     overflowed",
                        )
                    })?,
            };

            size = size.checked_add(operation_bytes).ok_or_else(|| {
                io::Error::other(
                    "reconstruction plan wire size \
                         overflowed",
                )
            })?;
        }

        Ok(size)
    }

    pub(crate) fn basis_range_count(&self) -> usize {
        self.operations
            .iter()
            .filter(|operation| matches!(operation, ReconstructionOp::Basis { .. }))
            .count()
    }

    pub(crate) fn literal_range_count(&self) -> usize {
        self.operations
            .iter()
            .filter(|operation| matches!(operation, ReconstructionOp::Literal(_,)))
            .count()
    }
}

impl ReconstructionBenchReport {
    pub fn print(&self) {
        let reusable_percent = percent(self.referenced_bytes, self.candidate_bytes);

        let wire_reduction = reduction_percent(self.candidate_bytes, self.total_wire_bytes);

        println!("CDC reconstruction prototype complete",);

        println!("  Target average:         {} KiB", self.average_kib,);

        println!(
            "  Basis data:             {} bytes",
            format_bytes(self.basis_bytes),
        );

        println!(
            "  Candidate data:         {} bytes",
            format_bytes(self.candidate_bytes),
        );

        println!(
            "  Basis chunks:           {}",
            format_bytes(self.index_chunks),
        );

        println!(
            "  Candidate chunks:       {}",
            format_bytes(self.candidate_chunks),
        );

        println!();

        println!(
            "  Reconstruction ops:     {} basis ranges / {} literal ranges",
            format_bytes(self.basis_ranges),
            format_bytes(self.literal_ranges),
        );

        println!(
            "  Reused basis data:      {} bytes ({reusable_percent:.2}%)",
            format_bytes(self.referenced_bytes),
        );

        println!(
            "  Literal data:           {} bytes",
            format_bytes(self.literal_bytes),
        );

        println!(
            "  Receiver index wire:    {} bytes",
            format_bytes(self.index_wire_bytes),
        );

        println!(
            "  Sender plan wire:       {} bytes",
            format_bytes(self.plan_wire_bytes),
        );

        println!(
            "  Application wire total: {} bytes ({wire_reduction:.2}% reduction)",
            format_bytes(self.total_wire_bytes),
        );

        println!();

        println!(
            "  Final BLAKE3:           {}",
            format_digest(&self.final_digest,),
        );

        println!("  Verification:           reconstructed digest matches candidate",);

        println!();

        println!(
            "  Receiver index build:   {:.6} s",
            self.index_build_elapsed.as_secs_f64(),
        );

        println!(
            "  Receiver index encode:  {:.6} s",
            self.index_encode_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender index decode:    {:.6} s",
            self.index_decode_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender plan build:      {:.6} s",
            self.plan_build_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender plan encode:     {:.6} s",
            self.plan_encode_elapsed.as_secs_f64(),
        );

        println!(
            "  Receiver plan decode:   {:.6} s",
            self.plan_decode_elapsed.as_secs_f64(),
        );

        println!(
            "  Receiver reconstruction:{:>9.6} s",
            self.reconstruction_elapsed.as_secs_f64(),
        );

        println!(
            "  Complete pipeline:      {:.6} s",
            self.total_elapsed.as_secs_f64(),
        );

        println!(
            "  Effective logical rate: {:.2} MB/s",
            decimal_megabytes_per_second(self.candidate_bytes, self.total_elapsed,),
        );
    }
}

pub fn run(
    basis_path: &Path,
    candidate_path: &Path,
    output_path: &Path,
    average_kib: usize,
) -> io::Result<ReconstructionBenchReport> {
    let total_started = Instant::now();

    let receiver_index = BasisFileIndex::build(basis_path, average_kib)?;

    let index_encode_started = Instant::now();

    let index_wire = receiver_index.encode_wire()?;

    let index_encode_elapsed = index_encode_started.elapsed();

    let index_decode_started = Instant::now();

    let sender_index = BasisFileIndex::decode_wire(&index_wire)?;

    let index_decode_elapsed = index_decode_started.elapsed();

    let sender_plan = ReconstructionPlan::build(candidate_path, &sender_index)?;

    let plan_encode_started = Instant::now();

    let plan_wire = sender_plan.encode_wire()?;

    let plan_encode_elapsed = plan_encode_started.elapsed();

    let plan_decode_started = Instant::now();

    let receiver_plan = ReconstructionPlan::decode_wire(&plan_wire)?;

    let plan_decode_elapsed = plan_decode_started.elapsed();

    if receiver_plan.target_bytes != sender_plan.target_bytes
        || receiver_plan.target_digest != sender_plan.target_digest
        || receiver_plan.operations != sender_plan.operations
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reconstruction plan changed during \
             wire round-trip",
        ));
    }

    let reconstruction = reconstruct(
        basis_path,
        output_path,
        sender_index.file_bytes(),
        &receiver_plan,
    )?;

    let index_chunks = u64::try_from(sender_index.chunks().len())
        .map_err(|_| io::Error::other("basis chunk count cannot be represented"))?;

    let basis_ranges = u64::try_from(sender_plan.basis_range_count())
        .map_err(|_| io::Error::other("basis range count cannot be represented"))?;

    let literal_ranges = u64::try_from(sender_plan.literal_range_count())
        .map_err(|_| io::Error::other("literal range count cannot be represented"))?;

    let index_wire_bytes = u64::try_from(index_wire.len())
        .map_err(|_| io::Error::other("index wire length cannot be represented"))?;

    let plan_wire_bytes = u64::try_from(plan_wire.len())
        .map_err(|_| io::Error::other("plan wire length cannot be represented"))?;

    let total_wire_bytes = index_wire_bytes
        .checked_add(plan_wire_bytes)
        .ok_or_else(|| io::Error::other("total application wire size overflowed"))?;

    Ok(ReconstructionBenchReport {
        average_kib,

        basis_bytes: sender_index.file_bytes(),

        candidate_bytes: sender_plan.target_bytes,

        index_chunks,
        candidate_chunks: sender_plan.candidate_chunks,

        basis_ranges,
        literal_ranges,

        referenced_bytes: sender_plan.referenced_bytes,

        literal_bytes: sender_plan.literal_bytes,

        index_wire_bytes,
        plan_wire_bytes,
        total_wire_bytes,

        final_digest: reconstruction.digest,

        index_build_elapsed: receiver_index.build_elapsed(),

        index_encode_elapsed,
        index_decode_elapsed,

        plan_build_elapsed: sender_plan.build_elapsed,

        plan_encode_elapsed,
        plan_decode_elapsed,

        reconstruction_elapsed: reconstruction.elapsed,

        total_elapsed: total_started.elapsed(),
    })
}

fn append_basis(
    operations: &mut Vec<ReconstructionOp>,
    offset: u64,
    length: u64,
) -> io::Result<()> {
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "basis reference length must not be zero",
        ));
    }

    if let Some(ReconstructionOp::Basis {
        offset: previous_offset,
        length: previous_length,
    }) = operations.last_mut()
    {
        let previous_end = previous_offset
            .checked_add(*previous_length)
            .ok_or_else(|| io::Error::other("previous basis range overflowed"))?;

        if previous_end == offset {
            *previous_length = previous_length
                .checked_add(length)
                .ok_or_else(|| io::Error::other("merged basis range overflowed"))?;

            return Ok(());
        }
    }

    operations.push(ReconstructionOp::Basis { offset, length });

    Ok(())
}

fn append_literal(operations: &mut Vec<ReconstructionOp>, contents: &[u8]) -> io::Result<()> {
    if contents.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "literal contents must not be empty",
        ));
    }

    if let Some(ReconstructionOp::Literal(previous)) = operations.last_mut() {
        previous.try_reserve(contents.len()).map_err(|error| {
            io::Error::other(format!(
                "failed to reserve merged literal \
                     storage: {error}",
            ))
        })?;

        previous.extend_from_slice(contents);

        return Ok(());
    }

    let mut literal = Vec::new();

    literal.try_reserve(contents.len()).map_err(|error| {
        io::Error::other(format!(
            "failed to reserve literal storage: \
                 {error}",
        ))
    })?;

    literal.extend_from_slice(contents);

    operations.push(ReconstructionOp::Literal(literal));

    Ok(())
}

pub(crate) fn reconstruct_verified(
    basis_path: &Path,
    output_path: &Path,
    expected_basis_bytes: u64,
    plan: &ReconstructionPlan,
) -> io::Result<()> {
    reconstruct(basis_path, output_path, expected_basis_bytes, plan).map(|_| ())
}

fn reconstruct(
    basis_path: &Path,
    output_path: &Path,
    expected_basis_bytes: u64,
    plan: &ReconstructionPlan,
) -> io::Result<ReconstructionStats> {
    let started = Instant::now();

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let temporary_path = temporary_path(output_path)?;

    let mut basis = File::open(basis_path)?;

    let actual_basis_bytes = basis.metadata()?.len();

    if actual_basis_bytes != expected_basis_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "basis file size changed: expected \
                 {expected_basis_bytes}, found \
                 {actual_basis_bytes}",
            ),
        ));
    }

    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)?;

    let write_result = reconstruct_into(&mut basis, &mut output, actual_basis_bytes, plan);

    drop(output);
    drop(basis);

    let digest = match write_result {
        Ok(digest) => digest,

        Err(error) => {
            let _ = fs::remove_file(&temporary_path);

            return Err(error);
        }
    };

    if let Err(error) = windows_file_replace::replace(&temporary_path, output_path) {
        let _ = fs::remove_file(&temporary_path);

        return Err(error);
    }

    let output_bytes = fs::metadata(output_path)?.len();

    if output_bytes != plan.target_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reconstructed output has \
                 {output_bytes} bytes, expected {}",
                plan.target_bytes,
            ),
        ));
    }

    Ok(ReconstructionStats {
        digest,
        elapsed: started.elapsed(),
    })
}

fn reconstruct_into(
    basis: &mut File,
    output: &mut File,
    basis_bytes: u64,
    plan: &ReconstructionPlan,
) -> io::Result<[u8; 32]> {
    output.set_len(plan.target_bytes)?;

    output.seek(SeekFrom::Start(0))?;

    let mut buffer = vec![0_u8; RECONSTRUCTION_BUFFER_BYTES];

    let mut hasher = ContentHasher::new();

    let mut written_bytes = 0_u64;

    for operation in &plan.operations {
        match operation {
            ReconstructionOp::Basis { offset, length } => {
                let range_end = offset.checked_add(*length).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "basis reconstruction \
                                 range overflowed",
                    )
                })?;

                if range_end > basis_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "basis reconstruction range \
                             {offset}..{range_end} exceeds \
                             {basis_bytes} bytes",
                        ),
                    ));
                }

                basis.seek(SeekFrom::Start(*offset))?;

                let mut remaining = *length;

                while remaining > 0 {
                    let requested =
                        usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                            io::Error::other(
                                "basis read size cannot \
                                 be represented",
                            )
                        })?;

                    let read = read_retry_interrupted(basis, &mut buffer[..requested])?;

                    if read == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "basis file ended during \
                             reconstruction",
                        ));
                    }

                    output.write_all(&buffer[..read])?;

                    hasher.update(&buffer[..read]);

                    let read_u64 = u64::try_from(read).map_err(|_| {
                        io::Error::other(
                            "basis read length \
                                     cannot be represented",
                        )
                    })?;

                    remaining -= read_u64;

                    written_bytes = written_bytes.checked_add(read_u64).ok_or_else(|| {
                        io::Error::other(
                            "reconstructed byte \
                                     count overflowed",
                        )
                    })?;
                }
            }

            ReconstructionOp::Literal(contents) => {
                output.write_all(contents)?;

                hasher.update(contents);

                let literal_bytes = u64::try_from(contents.len()).map_err(|_| {
                    io::Error::other(
                        "literal length cannot \
                             be represented",
                    )
                })?;

                written_bytes = written_bytes.checked_add(literal_bytes).ok_or_else(|| {
                    io::Error::other(
                        "reconstructed byte \
                                 count overflowed",
                    )
                })?;
            }
        }
    }

    output.flush()?;

    if written_bytes != plan.target_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reconstruction wrote \
                 {written_bytes} bytes, expected {}",
                plan.target_bytes,
            ),
        ));
    }

    let digest = hasher.finalize();

    if digest != plan.target_digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reconstructed BLAKE3 mismatch: \
                 expected {}, found {}",
                format_digest(&plan.target_digest,),
                format_digest(&digest),
            ),
        ));
    }

    Ok(digest)
}

fn temporary_path(output_path: &Path) -> io::Result<PathBuf> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            io::Error::other(format!(
                "system clock is before Unix \
                     epoch: {error}",
            ))
        })?
        .as_nanos();

    let mut temporary = OsString::from(output_path.as_os_str());

    temporary.push(format!(".ncs-cdc-part-{}-{unique}", process::id(),));

    Ok(PathBuf::from(temporary))
}

fn read_u8(encoded: &[u8], cursor: &mut usize) -> io::Result<u8> {
    let byte = *encoded
        .get(*cursor)
        .ok_or_else(|| invalid_plan("reconstruction plan is truncated"))?;

    *cursor = cursor
        .checked_add(1)
        .ok_or_else(|| invalid_plan("reconstruction plan cursor overflowed"))?;

    Ok(byte)
}

fn read_u64(encoded: &[u8], cursor: &mut usize) -> io::Result<u64> {
    let bytes = read_bytes(encoded, cursor, 8)?;

    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);

    Ok(u64::from_le_bytes(value))
}

fn read_digest(encoded: &[u8], cursor: &mut usize) -> io::Result<[u8; 32]> {
    let bytes = read_bytes(encoded, cursor, 32)?;

    let mut digest = [0_u8; 32];
    digest.copy_from_slice(bytes);

    Ok(digest)
}

fn read_bytes<'a>(encoded: &'a [u8], cursor: &mut usize, length: usize) -> io::Result<&'a [u8]> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| invalid_plan("reconstruction plan cursor overflowed"))?;

    let bytes = encoded
        .get(*cursor..end)
        .ok_or_else(|| invalid_plan("reconstruction plan is truncated"))?;

    *cursor = end;

    Ok(bytes)
}

fn read_retry_interrupted(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            result => return result,
        }
    }
}

fn invalid_plan(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

fn reduction_percent(full_bytes: u64, reduced_bytes: u64) -> f64 {
    if full_bytes == 0 {
        0.0
    } else {
        (full_bytes as f64 - reduced_bytes as f64) / full_bytes as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ReconstructionPlan, is_literal_limit_exceeded, reconstruct, run};
    use crate::cdc_basis_index::BasisFileIndex;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn plan_wire_round_trip_preserves_operations() {
        let root = temporary_root("plan-round-trip");

        fs::create_dir_all(&root).unwrap();

        let basis_path = root.join("basis.bin");

        let candidate_path = root.join("candidate.bin");

        let basis = deterministic_bytes(1024 * 1024, 0x1234_5678_90AB_CDEF);

        let mut candidate = basis.clone();

        candidate.splice(200_123..200_123, [1_u8, 2, 3, 4, 5]);

        fs::write(&basis_path, &basis).unwrap();

        fs::write(&candidate_path, &candidate).unwrap();

        let index = BasisFileIndex::build(&basis_path, 64).unwrap();

        let plan = ReconstructionPlan::build(&candidate_path, &index).unwrap();

        let wire = plan.encode_wire().unwrap();

        let decoded = ReconstructionPlan::decode_wire(&wire).unwrap();

        assert_eq!(decoded.target_bytes, plan.target_bytes,);

        assert_eq!(decoded.target_digest, plan.target_digest,);

        assert_eq!(decoded.operations, plan.operations,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_candidate_coalesces_to_one_basis_range() {
        let root = temporary_root("exact");

        fs::create_dir_all(&root).unwrap();

        let basis_path = root.join("basis.bin");

        let candidate_path = root.join("candidate.bin");

        let output_path = root.join("output.bin");

        let basis = deterministic_bytes(2 * 1024 * 1024, 0xCAFE_BABE_DEAD_BEEF);

        fs::write(&basis_path, &basis).unwrap();

        fs::write(&candidate_path, &basis).unwrap();

        let report = run(&basis_path, &candidate_path, &output_path, 64).unwrap();

        assert_eq!(report.literal_bytes, 0,);

        assert_eq!(report.basis_ranges, 1,);

        assert_eq!(fs::read(&output_path).unwrap(), basis,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unaligned_insertion_reconstructs_exact_candidate() {
        let root = temporary_root("insertion");

        fs::create_dir_all(&root).unwrap();

        let basis_path = root.join("basis.bin");

        let candidate_path = root.join("candidate.bin");

        let output_path = root.join("output.bin");

        let basis = deterministic_bytes(2 * 1024 * 1024, 0xA5A5_5A5A_1122_3344);

        let insertion = deterministic_bytes(4097, 0x8877_6655_4433_2211);

        let insertion_offset = 512 * 1024 + 123;

        let mut candidate = Vec::with_capacity(basis.len() + insertion.len());

        candidate.extend_from_slice(&basis[..insertion_offset]);

        candidate.extend_from_slice(&insertion);

        candidate.extend_from_slice(&basis[insertion_offset..]);

        fs::write(&basis_path, &basis).unwrap();

        fs::write(&candidate_path, &candidate).unwrap();

        let report = run(&basis_path, &candidate_path, &output_path, 64).unwrap();

        assert!(report.referenced_bytes > report.candidate_bytes * 90 / 100,);

        assert!(report.literal_bytes < 512 * 1024,);

        assert_eq!(fs::read(&output_path).unwrap(), candidate,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_plan_stops_before_unbounded_literal_growth() {
        let root = temporary_root("literal-limit");

        fs::create_dir_all(&root).unwrap();

        let basis_path = root.join("basis.bin");

        let candidate_path = root.join("candidate.bin");

        let basis = deterministic_bytes(2 * 1024 * 1024, 0xABCD_EF01_2345_6789);

        let candidate = deterministic_bytes(2 * 1024 * 1024, 0x9876_5432_10FE_DCBA);

        fs::write(&basis_path, &basis).unwrap();

        fs::write(&candidate_path, &candidate).unwrap();

        let index = BasisFileIndex::build(&basis_path, 64).unwrap();

        let error =
            ReconstructionPlan::build_bounded(&candidate_path, &index, 64 * 1024).unwrap_err();

        assert!(is_literal_limit_exceeded(&error,),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn digest_mismatch_does_not_replace_destination() {
        let root = temporary_root("digest-mismatch");

        fs::create_dir_all(&root).unwrap();

        let basis_path = root.join("basis.bin");

        let candidate_path = root.join("candidate.bin");

        let output_path = root.join("output.bin");

        let basis = deterministic_bytes(1024 * 1024, 0x0102_0304_0506_0708);

        let mut candidate = basis.clone();

        candidate[100_000] ^= 0xFF;

        fs::write(&basis_path, &basis).unwrap();

        fs::write(&candidate_path, &candidate).unwrap();

        fs::write(&output_path, b"existing destination").unwrap();

        let index = BasisFileIndex::build(&basis_path, 64).unwrap();

        let mut plan = ReconstructionPlan::build(&candidate_path, &index).unwrap();

        plan.target_digest[0] ^= 0xFF;

        let result = reconstruct(&basis_path, &output_path, index.file_bytes(), &plan);

        assert!(result.is_err());

        assert_eq!(fs::read(&output_path).unwrap(), b"existing destination",);

        fs::remove_dir_all(root).unwrap();
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

    fn temporary_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-cdc-reconstruct-{name}-{}-{unique}",
            process::id(),
        ))
    }
}
