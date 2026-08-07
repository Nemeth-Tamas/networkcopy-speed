use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::transfer_profile::TransferProfiler;
use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;
const SAMPLE_BYTES: usize = MIB;
const MAX_SAMPLE_COUNT: usize = 3;
const MIN_SAVINGS_PERCENT: u64 = 10;

pub const DEFAULT_LEVEL: i32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionDecision {
    Compress,
    SendRaw,
}

impl CompressionDecision {
    fn description(self) -> &'static str {
        match self {
            Self::Compress => "compress",
            Self::SendRaw => "send raw",
        }
    }
}

#[derive(Debug)]
pub struct CompressionProbeReport {
    pub file_bytes: u64,
    pub sample_count: usize,
    pub sampled_bytes: u64,
    pub compressed_bytes: u64,
    pub estimated_wire_bytes: u64,
    pub level: i32,
    pub ratio_percent: f64,
    pub savings_percent: f64,
    pub decision: CompressionDecision,
    pub compression_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl CompressionProbeReport {
    pub fn print(&self) {
        println!("Adaptive Zstandard probe complete");
        println!(
            "  File size:             {} bytes",
            format_bytes(self.file_bytes)
        );
        println!("  Compression level:     {}", self.level);
        println!("  Samples:               {}", self.sample_count);
        println!(
            "  Sampled data:          {} bytes",
            format_bytes(self.sampled_bytes)
        );
        println!(
            "  Compressed samples:    {} bytes",
            format_bytes(self.compressed_bytes)
        );
        println!("  Sample ratio:          {:.2}%", self.ratio_percent);
        println!("  Estimated savings:     {:.2}%", self.savings_percent);
        println!(
            "  Estimated wire size:   {} bytes",
            format_bytes(self.estimated_wire_bytes)
        );
        println!(
            "  Compression time:      {:.6} s",
            self.compression_elapsed.as_secs_f64()
        );
        println!(
            "  Sample throughput:     {:.2} MB/s",
            decimal_megabytes_per_second(self.sampled_bytes, self.compression_elapsed,)
        );
        println!("  Decision:              {}", self.decision.description());
        println!(
            "  Total probe time:      {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
    }
}

pub fn validate_level(level: i32) -> io::Result<()> {
    if !zstd::compression_level_range().contains(&level) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Zstandard compression level {level} is outside the supported range"),
        ));
    }

    Ok(())
}

pub fn run(source: &Path, level: i32) -> io::Result<CompressionProbeReport> {
    validate_level(level)?;

    let total_started = Instant::now();
    let file = File::open(source)?;
    let metadata = file.metadata()?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let file_bytes = metadata.len();
    let ranges = sample_ranges(file_bytes);

    let compression_started = Instant::now();

    let (sampled_bytes, compressed_bytes) = measure_ranges(&file, 0, &ranges, level)?;
    let compression_elapsed = compression_started.elapsed();

    let ratio_percent = compression_ratio_percent(sampled_bytes, compressed_bytes);

    let savings_percent = 100.0 - ratio_percent;

    let decision = choose_decision(sampled_bytes, compressed_bytes);

    let estimated_wire_bytes =
        estimate_wire_bytes(file_bytes, sampled_bytes, compressed_bytes, decision)?;

    Ok(CompressionProbeReport {
        file_bytes,
        sample_count: ranges.len(),
        sampled_bytes,
        compressed_bytes,
        estimated_wire_bytes,
        level,
        ratio_percent,
        savings_percent,
        decision,
        compression_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

pub(crate) fn decide_file_range(
    file: &File,
    offset: u64,
    length: u64,
    level: i32,
) -> io::Result<CompressionDecision> {
    decide_file_range_profiled(
        file,
        offset,
        length,
        level,
        None,
    )
}

pub(crate) fn decide_file_range_profiled(
    file: &File,
    offset: u64,
    length: u64,
    level: i32,
    profiler: Option<&TransferProfiler>,
) -> io::Result<CompressionDecision> {
    validate_level(level)?;

    let end = offset.checked_add(length).ok_or_else(
        || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "compression range overflowed",
            )
        },
    )?;

    let file_length = file.metadata()?.len();

    if end > file_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "compression range {offset}..{end} exceeds file length {file_length}"
            ),
        ));
    }

    let ranges = sample_ranges(length);

    let original_position = {
        let mut cursor = file;

        cursor.stream_position()?
    };

    let measurement_started = Instant::now();

    let measurement =
        measure_ranges(
            file,
            offset,
            &ranges,
            level,
        );

    let measurement_elapsed =
        measurement_started.elapsed();

    {
        let mut cursor = file;

        cursor.seek(SeekFrom::Start(
            original_position,
        ))?;
    }

    let (sampled_bytes, compressed_bytes) =
        measurement?;

    if let Some(profiler) = profiler {
        profiler.record_sender_compression(
            measurement_elapsed,
            sampled_bytes,
        );
    }

    Ok(choose_decision(
        sampled_bytes,
        compressed_bytes,
    ))
}

fn measure_ranges(
    file: &File,
    base_offset: u64,
    ranges: &[(u64, usize)],
    level: i32,
) -> io::Result<(u64, u64)> {
    let mut compressor = zstd::bulk::Compressor::new(level).map_err(|error| {
        io::Error::other(format!("failed to create Zstandard compressor: {error}"))
    })?;

    let mut sampled_bytes = 0_u64;
    let mut compressed_bytes = 0_u64;

    for (relative_offset, length) in ranges {
        let absolute_offset = base_offset
            .checked_add(*relative_offset)
            .ok_or_else(|| io::Error::other("compression sample offset overflowed"))?;

        let mut sample = vec![0_u8; *length];

        read_exact_at(file, &mut sample, absolute_offset)?;

        let compressed = compressor.compress(&sample).map_err(|error| {
            io::Error::other(format!("Zstandard sample compression failed: {error}"))
        })?;

        sampled_bytes = sampled_bytes
            .checked_add(
                u64::try_from(*length)
                    .map_err(|_| io::Error::other("sample length cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("sampled byte count overflowed"))?;

        compressed_bytes =
            compressed_bytes
                .checked_add(u64::try_from(compressed.len()).map_err(|_| {
                    io::Error::other("compressed sample length cannot be represented")
                })?)
                .ok_or_else(|| io::Error::other("compressed sample byte count overflowed"))?;
    }

    Ok((sampled_bytes, compressed_bytes))
}

fn sample_ranges(file_bytes: u64) -> Vec<(u64, usize)> {
    if file_bytes == 0 {
        return Vec::new();
    }

    let maximum_full_sample = (SAMPLE_BYTES * MAX_SAMPLE_COUNT) as u64;

    if file_bytes <= maximum_full_sample {
        return vec![(0, file_bytes as usize)];
    }

    let sample_bytes = SAMPLE_BYTES as u64;
    let last_offset = file_bytes - sample_bytes;

    vec![
        (0, SAMPLE_BYTES),
        (last_offset / 2, SAMPLE_BYTES),
        (last_offset, SAMPLE_BYTES),
    ]
}

fn choose_decision(sampled_bytes: u64, compressed_bytes: u64) -> CompressionDecision {
    if should_compress_sizes(sampled_bytes, compressed_bytes) {
        CompressionDecision::Compress
    } else {
        CompressionDecision::SendRaw
    }
}

pub(crate) fn should_compress_sizes(raw_bytes: u64, compressed_bytes: u64) -> bool {
    if raw_bytes == 0 {
        return false;
    }

    let compressed_percent = u128::from(compressed_bytes) * 100;

    let required_percent = u128::from(raw_bytes) * u128::from(100 - MIN_SAVINGS_PERCENT);

    compressed_percent <= required_percent
}

fn compression_ratio_percent(sampled_bytes: u64, compressed_bytes: u64) -> f64 {
    if sampled_bytes == 0 {
        return 100.0;
    }

    compressed_bytes as f64 / sampled_bytes as f64 * 100.0
}

fn estimate_wire_bytes(
    file_bytes: u64,
    sampled_bytes: u64,
    compressed_bytes: u64,
    decision: CompressionDecision,
) -> io::Result<u64> {
    if decision == CompressionDecision::SendRaw || sampled_bytes == 0 {
        return Ok(file_bytes);
    }

    let numerator = u128::from(file_bytes)
        .checked_mul(u128::from(compressed_bytes))
        .ok_or_else(|| io::Error::other("estimated wire-size calculation overflowed"))?;

    let estimate = numerator.div_ceil(u128::from(sampled_bytes));

    u64::try_from(estimate)
        .map_err(|_| io::Error::other("estimated wire size cannot be represented"))
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match file.seek_read(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "compression sample ended unexpectedly",
                ));
            }

            Ok(read) => {
                buffer = &mut buffer[read..];

                offset = offset
                    .checked_add(read as u64)
                    .ok_or_else(|| io::Error::other("sample offset overflowed"))?;
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}

            Err(error) => return Err(error),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CompressionDecision, MAX_SAMPLE_COUNT, SAMPLE_BYTES, choose_decision, sample_ranges,
    };

    #[test]
    fn large_files_sample_start_middle_and_end() {
        let file_bytes = 100 * 1024 * 1024_u64;

        let ranges = sample_ranges(file_bytes);

        assert_eq!(ranges.len(), MAX_SAMPLE_COUNT);
        assert_eq!(ranges[0], (0, SAMPLE_BYTES));

        assert_eq!(ranges[2], (file_bytes - SAMPLE_BYTES as u64, SAMPLE_BYTES,));

        assert!(ranges[0].0 < ranges[1].0);

        assert!(ranges[1].0 < ranges[2].0);
    }

    #[test]
    fn small_files_are_sampled_once() {
        let file_bytes = 512 * 1024_u64;

        assert_eq!(sample_ranges(file_bytes), vec![(0, file_bytes as usize)]);

        assert!(sample_ranges(0).is_empty());
    }

    #[test]
    fn compression_requires_ten_percent_savings() {
        assert_eq!(choose_decision(1_000, 899), CompressionDecision::Compress);

        assert_eq!(choose_decision(1_000, 900), CompressionDecision::Compress);

        assert_eq!(choose_decision(1_000, 901), CompressionDecision::SendRaw);

        assert_eq!(choose_decision(0, 0), CompressionDecision::SendRaw);
    }

    #[test]
    fn zstandard_sample_round_trips() {
        let source = vec![0x5A_u8; 1024 * 1024];

        let compressed = zstd::bulk::compress(&source, 1).unwrap();

        let decompressed = zstd::bulk::decompress(&compressed, source.len()).unwrap();

        assert_eq!(decompressed, source);
        assert!(compressed.len() < source.len());
    }
}
