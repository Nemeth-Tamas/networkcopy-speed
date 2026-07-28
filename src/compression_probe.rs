use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;
const SAMPLE_BYTES: usize = MIB;
const MAX_SAMPLE_COUNT: usize = 3;
const MIN_SAVINGS_PERCENT: f64 = 10.0;

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
    let mut sampled_bytes = 0_u64;
    let mut compressed_bytes = 0_u64;

    for (offset, length) in &ranges {
        let mut sample = vec![0_u8; *length];

        read_exact_at(&file, &mut sample, *offset)?;

        let compressed = zstd::bulk::compress(&sample, level).map_err(|error| {
            io::Error::other(format!("Zstandard sample compression failed: {error}"))
        })?;

        sampled_bytes = sampled_bytes
            .checked_add(*length as u64)
            .ok_or_else(|| io::Error::other("sampled byte count overflowed"))?;

        compressed_bytes = compressed_bytes
            .checked_add(compressed.len() as u64)
            .ok_or_else(|| io::Error::other("compressed sample byte count overflowed"))?;
    }

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
    if sampled_bytes == 0 {
        return CompressionDecision::SendRaw;
    }

    let savings_percent = 100.0 - compression_ratio_percent(sampled_bytes, compressed_bytes);

    if savings_percent >= MIN_SAVINGS_PERCENT {
        CompressionDecision::Compress
    } else {
        CompressionDecision::SendRaw
    }
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
