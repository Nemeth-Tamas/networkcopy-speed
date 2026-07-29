use crate::compression_probe;
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::manifest_scan::{self, FileClass, ManifestEntry};
use crate::tiny_pack_codec;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_DICTIONARY_KIB: usize = 64;

const MAX_DICTIONARY_KIB: usize = 1024;
const TRAINING_STRIDE: usize = 4;
const MIN_TRAINING_FILES: usize = 8;
const MAX_TRAINING_FILES: usize = 8192;
const MAX_TRAINING_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PACK_FILES: usize = 4096;

#[derive(Debug)]
pub struct ZstdDictionaryBenchReport {
    pub worker_count: usize,
    pub tiny_files: u64,
    pub tiny_bytes: u64,
    pub training_files: u64,
    pub training_bytes: u64,
    pub evaluation_files: u64,
    pub evaluation_bytes: u64,
    pub evaluation_packs: u64,
    pub requested_dictionary_bytes: u64,
    pub dictionary_bytes: u64,
    pub dictionary_training_elapsed: Duration,
    pub baseline_payload_bytes: u64,
    pub baseline_compressed_packs: u64,
    pub baseline_compression_elapsed: Duration,
    pub dictionary_payload_bytes: u64,
    pub dictionary_compressed_packs: u64,
    pub dictionary_compression_elapsed: Duration,
    pub dictionary_inclusive_bytes: u64,
}

impl ZstdDictionaryBenchReport {
    pub fn print(&self) {
        println!("Held-out shared Zstandard dictionary benchmark complete");
        println!("  Scanner workers:          {}", self.worker_count);
        println!(
            "  Tiny-file dataset:        {} files / {} bytes",
            format_bytes(self.tiny_files),
            format_bytes(self.tiny_bytes),
        );
        println!(
            "  Training subset:          {} files / {} bytes",
            format_bytes(self.training_files),
            format_bytes(self.training_bytes),
        );
        println!(
            "  Held-out evaluation:      {} files / {} bytes",
            format_bytes(self.evaluation_files),
            format_bytes(self.evaluation_bytes),
        );
        println!(
            "  Held-out packs:           {}",
            format_bytes(self.evaluation_packs),
        );
        println!(
            "  Dictionary requested:     {} bytes",
            format_bytes(self.requested_dictionary_bytes),
        );
        println!(
            "  Dictionary produced:      {} bytes",
            format_bytes(self.dictionary_bytes),
        );
        println!(
            "  Dictionary training:      {:.6} s",
            self.dictionary_training_elapsed.as_secs_f64(),
        );
        println!(
            "  Training throughput:      {:.2} MB/s",
            decimal_megabytes_per_second(self.training_bytes, self.dictionary_training_elapsed,),
        );
        println!();
        println!(
            "  Current adaptive payload: {} bytes",
            format_bytes(self.baseline_payload_bytes),
        );
        println!(
            "  Current compressed packs: {}",
            format_bytes(self.baseline_compressed_packs),
        );
        println!(
            "  Current compression time: {:.6} s",
            self.baseline_compression_elapsed.as_secs_f64(),
        );
        println!(
            "  Current throughput:       {:.2} MB/s",
            decimal_megabytes_per_second(self.evaluation_bytes, self.baseline_compression_elapsed,),
        );
        println!();
        println!(
            "  Dictionary payload:       {} bytes",
            format_bytes(self.dictionary_payload_bytes),
        );
        println!(
            "  Dictionary packs used:    {}",
            format_bytes(self.dictionary_compressed_packs),
        );
        println!(
            "  Dictionary compression:   {:.6} s",
            self.dictionary_compression_elapsed.as_secs_f64(),
        );
        println!(
            "  Dictionary throughput:    {:.2} MB/s",
            decimal_megabytes_per_second(
                self.evaluation_bytes,
                self.dictionary_compression_elapsed,
            ),
        );
        println!(
            "  Dictionary-inclusive:     {} bytes",
            format_bytes(self.dictionary_inclusive_bytes),
        );
        println!();
        println!(
            "  Current payload savings:  {:.2}%",
            savings_percent(self.evaluation_bytes, self.baseline_payload_bytes),
        );
        println!(
            "  Dictionary savings:       {:.2}%",
            savings_percent(self.evaluation_bytes, self.dictionary_inclusive_bytes),
        );
        println!(
            "  Gain versus current:      {:.2}%",
            savings_percent(self.baseline_payload_bytes, self.dictionary_inclusive_bytes,),
        );
        println!("  Integrity:                 all held-out packs verified");
    }
}

#[derive(Debug)]
struct DatasetSplit {
    training: Vec<ManifestEntry>,
    evaluation: Vec<ManifestEntry>,
    training_bytes: u64,
    evaluation_bytes: u64,
}

#[derive(Debug, Default)]
struct EvaluationMetrics {
    packs: u64,
    raw_bytes: u64,
    baseline_payload_bytes: u64,
    baseline_compressed_packs: u64,
    baseline_compression_elapsed: Duration,
    dictionary_payload_bytes: u64,
    dictionary_compressed_packs: u64,
    dictionary_compression_elapsed: Duration,
}

pub fn validate_dictionary_kib(dictionary_kib: usize) -> io::Result<()> {
    if !(1..=MAX_DICTIONARY_KIB).contains(&dictionary_kib) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("dictionary size must be between 1 and {MAX_DICTIONARY_KIB} KiB"),
        ));
    }

    Ok(())
}

pub fn run(
    root: &Path,
    worker_count: usize,
    dictionary_kib: usize,
    level: i32,
) -> io::Result<ZstdDictionaryBenchReport> {
    validate_dictionary_kib(dictionary_kib)?;
    compression_probe::validate_level(level)?;

    let root = root.canonicalize()?;
    let scan = manifest_scan::run(&root, worker_count)?;

    let tiny_entries: Vec<ManifestEntry> = scan
        .manifest
        .into_iter()
        .filter(|entry| entry.class == FileClass::Tiny)
        .collect();

    let tiny_files = u64::try_from(tiny_entries.len())
        .map_err(|_| io::Error::other("tiny-file count cannot be represented"))?;

    let tiny_bytes = sum_entry_bytes(&tiny_entries)?;

    if tiny_entries.len() < MIN_TRAINING_FILES + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "dictionary benchmark requires at least {} tiny files",
                MIN_TRAINING_FILES + 1,
            ),
        ));
    }

    let split = split_dataset(tiny_entries)?;

    if split.training.len() < MIN_TRAINING_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "dictionary training produced only {} usable files; at least \
                 {MIN_TRAINING_FILES} are required",
                split.training.len(),
            ),
        ));
    }

    if split.evaluation.is_empty() || split.evaluation_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "held-out dictionary evaluation requires non-empty tiny-file data",
        ));
    }

    let requested_dictionary_bytes = dictionary_kib
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("dictionary byte count overflowed"))?;

    let minimum_training_bytes = requested_dictionary_bytes
        .checked_mul(4)
        .ok_or_else(|| io::Error::other("minimum training byte count overflowed"))?;

    let minimum_training_bytes = u64::try_from(minimum_training_bytes)
        .map_err(|_| io::Error::other("minimum training size cannot be represented"))?;

    if split.training_bytes < minimum_training_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "dictionary training has {} bytes, but a {dictionary_kib} KiB \
                 dictionary requires at least {} bytes for this benchmark",
                split.training_bytes, minimum_training_bytes,
            ),
        ));
    }

    let training_samples = read_training_samples(&root, &split.training)?;

    let training_started = Instant::now();

    let dictionary = zstd::dict::from_samples(&training_samples, requested_dictionary_bytes)
        .map_err(|error| {
            io::Error::other(format!(
                "failed to train shared Zstandard dictionary: {error}"
            ))
        })?;

    let dictionary_training_elapsed = training_started.elapsed();

    if dictionary.is_empty() {
        return Err(io::Error::other(
            "Zstandard dictionary trainer produced an empty dictionary",
        ));
    }

    let evaluation = evaluate_held_out_packs(&root, &split.evaluation, level, &dictionary)?;

    if evaluation.raw_bytes != split.evaluation_bytes {
        return Err(io::Error::other(format!(
            "held-out evaluation processed {} bytes, expected {}",
            evaluation.raw_bytes, split.evaluation_bytes,
        )));
    }

    let dictionary_bytes = u64::try_from(dictionary.len())
        .map_err(|_| io::Error::other("dictionary length cannot be represented"))?;

    let dictionary_overhead = if evaluation.dictionary_compressed_packs == 0 {
        0
    } else {
        dictionary_bytes
    };

    let dictionary_inclusive_bytes = evaluation
        .dictionary_payload_bytes
        .checked_add(dictionary_overhead)
        .ok_or_else(|| io::Error::other("dictionary-inclusive size overflowed"))?;

    Ok(ZstdDictionaryBenchReport {
        worker_count,
        tiny_files,
        tiny_bytes,
        training_files: u64::try_from(split.training.len())
            .map_err(|_| io::Error::other("training file count cannot be represented"))?,
        training_bytes: split.training_bytes,
        evaluation_files: u64::try_from(split.evaluation.len())
            .map_err(|_| io::Error::other("evaluation file count cannot be represented"))?,
        evaluation_bytes: split.evaluation_bytes,
        evaluation_packs: evaluation.packs,
        requested_dictionary_bytes: u64::try_from(requested_dictionary_bytes)
            .map_err(|_| io::Error::other("requested dictionary size cannot be represented"))?,
        dictionary_bytes,
        dictionary_training_elapsed,
        baseline_payload_bytes: evaluation.baseline_payload_bytes,
        baseline_compressed_packs: evaluation.baseline_compressed_packs,
        baseline_compression_elapsed: evaluation.baseline_compression_elapsed,
        dictionary_payload_bytes: evaluation.dictionary_payload_bytes,
        dictionary_compressed_packs: evaluation.dictionary_compressed_packs,
        dictionary_compression_elapsed: evaluation.dictionary_compression_elapsed,
        dictionary_inclusive_bytes,
    })
}

fn split_dataset(entries: Vec<ManifestEntry>) -> io::Result<DatasetSplit> {
    let mut training = Vec::new();
    let mut evaluation = Vec::new();
    let mut training_bytes = 0_u64;
    let mut evaluation_bytes = 0_u64;

    for (index, entry) in entries.into_iter().enumerate() {
        let next_training_bytes = training_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("training byte count overflowed"))?;

        let use_for_training = index % TRAINING_STRIDE == 0
            && entry.file_size > 0
            && training.len() < MAX_TRAINING_FILES
            && next_training_bytes <= MAX_TRAINING_BYTES;

        if use_for_training {
            training_bytes = next_training_bytes;
            training.push(entry);
        } else {
            evaluation_bytes = evaluation_bytes
                .checked_add(entry.file_size)
                .ok_or_else(|| io::Error::other("evaluation byte count overflowed"))?;

            evaluation.push(entry);
        }
    }

    Ok(DatasetSplit {
        training,
        evaluation,
        training_bytes,
        evaluation_bytes,
    })
}

fn read_training_samples(root: &Path, entries: &[ManifestEntry]) -> io::Result<Vec<Vec<u8>>> {
    entries
        .iter()
        .map(|entry| read_entry(root, entry))
        .collect()
}

fn evaluate_held_out_packs(
    root: &Path,
    entries: &[ManifestEntry],
    level: i32,
    dictionary: &[u8],
) -> io::Result<EvaluationMetrics> {
    let mut baseline_compressor = zstd::bulk::Compressor::new(level).map_err(|error| {
        io::Error::other(format!(
            "failed to create baseline Zstandard compressor: {error}"
        ))
    })?;

    let mut dictionary_compressor = zstd::bulk::Compressor::with_dictionary(level, dictionary)
        .map_err(|error| {
            io::Error::other(format!(
                "failed to create dictionary Zstandard compressor: {error}"
            ))
        })?;

    let mut baseline_decompressor = zstd::bulk::Decompressor::new().map_err(|error| {
        io::Error::other(format!(
            "failed to create baseline Zstandard decompressor: {error}"
        ))
    })?;

    let mut dictionary_decompressor = zstd::bulk::Decompressor::with_dictionary(dictionary)
        .map_err(|error| {
            io::Error::other(format!(
                "failed to create dictionary Zstandard decompressor: {error}"
            ))
        })?;

    let mut metrics = EvaluationMetrics::default();
    let mut pack = Vec::with_capacity(tiny_pack_codec::MAX_TINY_PACK_BYTES);
    let mut pack_files = 0_usize;

    for entry in entries {
        let contents = read_entry(root, entry)?;

        let exceeds_byte_target = !pack.is_empty()
            && pack
                .len()
                .checked_add(contents.len())
                .is_none_or(|bytes| bytes > tiny_pack_codec::MAX_TINY_PACK_BYTES);

        let exceeds_file_limit = pack_files >= MAX_PACK_FILES;

        if pack_files > 0 && (exceeds_byte_target || exceeds_file_limit) {
            evaluate_pack(
                &pack,
                &mut baseline_compressor,
                &mut dictionary_compressor,
                &mut baseline_decompressor,
                &mut dictionary_decompressor,
                &mut metrics,
            )?;

            pack.clear();
            pack_files = 0;
        }

        pack.extend_from_slice(&contents);
        pack_files = pack_files
            .checked_add(1)
            .ok_or_else(|| io::Error::other("evaluation pack file count overflowed"))?;
    }

    if pack_files > 0 {
        evaluate_pack(
            &pack,
            &mut baseline_compressor,
            &mut dictionary_compressor,
            &mut baseline_decompressor,
            &mut dictionary_decompressor,
            &mut metrics,
        )?;
    }

    Ok(metrics)
}

fn evaluate_pack(
    raw: &[u8],
    baseline_compressor: &mut zstd::bulk::Compressor<'_>,
    dictionary_compressor: &mut zstd::bulk::Compressor<'_>,
    baseline_decompressor: &mut zstd::bulk::Decompressor<'_>,
    dictionary_decompressor: &mut zstd::bulk::Decompressor<'_>,
    metrics: &mut EvaluationMetrics,
) -> io::Result<()> {
    let baseline_started = Instant::now();

    let baseline_compressed = baseline_compressor.compress(raw).map_err(|error| {
        io::Error::other(format!(
            "baseline held-out pack compression failed: {error}"
        ))
    })?;

    metrics.baseline_compression_elapsed += baseline_started.elapsed();

    let dictionary_started = Instant::now();

    let dictionary_compressed = dictionary_compressor.compress(raw).map_err(|error| {
        io::Error::other(format!(
            "dictionary held-out pack compression failed: {error}"
        ))
    })?;

    metrics.dictionary_compression_elapsed += dictionary_started.elapsed();

    verify_pack(raw, &baseline_compressed, baseline_decompressor, "baseline")?;

    verify_pack(
        raw,
        &dictionary_compressed,
        dictionary_decompressor,
        "dictionary",
    )?;

    let (baseline_payload, baseline_used_compression) =
        adaptive_payload_size(raw.len(), baseline_compressed.len())?;

    let (dictionary_payload, dictionary_used_compression) =
        adaptive_payload_size(raw.len(), dictionary_compressed.len())?;

    metrics.packs = metrics
        .packs
        .checked_add(1)
        .ok_or_else(|| io::Error::other("evaluation pack count overflowed"))?;

    metrics.raw_bytes = metrics
        .raw_bytes
        .checked_add(
            u64::try_from(raw.len())
                .map_err(|_| io::Error::other("pack length cannot be represented"))?,
        )
        .ok_or_else(|| io::Error::other("evaluation raw byte count overflowed"))?;

    metrics.baseline_payload_bytes = metrics
        .baseline_payload_bytes
        .checked_add(baseline_payload)
        .ok_or_else(|| io::Error::other("baseline payload byte count overflowed"))?;

    metrics.dictionary_payload_bytes = metrics
        .dictionary_payload_bytes
        .checked_add(dictionary_payload)
        .ok_or_else(|| io::Error::other("dictionary payload byte count overflowed"))?;

    if baseline_used_compression {
        metrics.baseline_compressed_packs = metrics
            .baseline_compressed_packs
            .checked_add(1)
            .ok_or_else(|| io::Error::other("baseline compressed-pack count overflowed"))?;
    }

    if dictionary_used_compression {
        metrics.dictionary_compressed_packs = metrics
            .dictionary_compressed_packs
            .checked_add(1)
            .ok_or_else(|| io::Error::other("dictionary compressed-pack count overflowed"))?;
    }

    Ok(())
}

fn verify_pack(
    expected: &[u8],
    compressed: &[u8],
    decompressor: &mut zstd::bulk::Decompressor<'_>,
    description: &str,
) -> io::Result<()> {
    let mut decoded = vec![0_u8; expected.len()];

    let decoded_bytes = decompressor
        .decompress_to_buffer(compressed, &mut decoded)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} held-out pack decompression failed: {error}"),
            )
        })?;

    if decoded_bytes != expected.len() || decoded != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} held-out pack failed round-trip verification"),
        ));
    }

    Ok(())
}

fn adaptive_payload_size(raw_bytes: usize, compressed_bytes: usize) -> io::Result<(u64, bool)> {
    let raw_bytes = u64::try_from(raw_bytes)
        .map_err(|_| io::Error::other("raw pack length cannot be represented"))?;

    let compressed_bytes = u64::try_from(compressed_bytes)
        .map_err(|_| io::Error::other("compressed pack length cannot be represented"))?;

    if compression_probe::should_compress_sizes(raw_bytes, compressed_bytes) {
        Ok((compressed_bytes, true))
    } else {
        Ok((raw_bytes, false))
    }
}

fn read_entry(root: &Path, entry: &ManifestEntry) -> io::Result<Vec<u8>> {
    let path = root.join(&entry.relative_path);
    let contents = fs::read(&path)?;

    let expected_bytes = usize::try_from(entry.file_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tiny-file size cannot be represented: {}", path.display()),
        )
    })?;

    if contents.len() != expected_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tiny file changed during dictionary benchmark: {}",
                path.display(),
            ),
        ));
    }

    Ok(contents)
}

fn sum_entry_bytes(entries: &[ManifestEntry]) -> io::Result<u64> {
    entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("tiny-file byte count overflowed"))
    })
}

fn savings_percent(original_bytes: u64, candidate_bytes: u64) -> f64 {
    if original_bytes == 0 {
        return 0.0;
    }

    100.0 - candidate_bytes as f64 / original_bytes as f64 * 100.0
}

#[cfg(test)]
mod tests {
    use super::{DatasetSplit, adaptive_payload_size, savings_percent, split_dataset};
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::path::PathBuf;

    fn tiny_entry(index: usize, bytes: u64) -> ManifestEntry {
        ManifestEntry {
            relative_path: PathBuf::from(format!("file-{index:04}.bin")),
            file_size: bytes,
            last_write_time: 0,
            file_attributes: 0,
            class: FileClass::Tiny,
        }
    }

    #[test]
    fn split_uses_every_fourth_file_for_training() {
        let entries = (0..12).map(|index| tiny_entry(index, 100)).collect();

        let DatasetSplit {
            training,
            evaluation,
            training_bytes,
            evaluation_bytes,
        } = split_dataset(entries).unwrap();

        assert_eq!(training.len(), 3);
        assert_eq!(evaluation.len(), 9);
        assert_eq!(training_bytes, 300);
        assert_eq!(evaluation_bytes, 900);
    }

    #[test]
    fn adaptive_payload_uses_existing_ten_percent_threshold() {
        assert_eq!(adaptive_payload_size(1_000, 900).unwrap(), (900, true));
        assert_eq!(adaptive_payload_size(1_000, 901).unwrap(), (1_000, false));
    }

    #[test]
    fn savings_can_report_regression() {
        assert_eq!(savings_percent(1_000, 750), 25.0);
        assert_eq!(savings_percent(1_000, 1_250), -25.0);
        assert_eq!(savings_percent(0, 0), 0.0);
    }
}
