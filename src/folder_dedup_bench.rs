use crate::cdc_basis_index::BasisFileIndex;
use crate::cdc_reconstruction_bench::{ReconstructionPlan, is_literal_limit_exceeded};
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

pub const DEFAULT_MINIMUM_FILE_MIB: usize = 1;
pub const DEFAULT_MAXIMUM_LITERAL_MIB: usize = 64;

const SAMPLE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct FolderDedupReport {
    pub average_kib: usize,
    pub minimum_file_bytes: u64,
    pub maximum_literal_bytes: u64,

    pub source_files: u64,
    pub source_bytes: u64,

    pub same_path_candidates: u64,
    pub deduplicated_files: u64,

    pub skipped_small_files: u64,
    pub missing_basis_files: u64,
    pub sample_rejected_files: u64,
    pub literal_limit_fallbacks: u64,
    pub no_savings_fallbacks: u64,

    pub candidate_bytes: u64,
    pub referenced_bytes: u64,
    pub literal_bytes: u64,

    pub index_wire_bytes: u64,
    pub plan_wire_bytes: u64,
    pub fallback_wire_bytes: u64,
    pub total_wire_bytes: u64,

    pub basis_ranges: u64,
    pub literal_ranges: u64,

    pub peak_staged_payload_bytes: u64,

    pub index_elapsed: Duration,
    pub plan_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl FolderDedupReport {
    pub fn print(&self) {
        println!("Bounded folder-level CDC planning complete",);

        println!("  Target average:         {} KiB", self.average_kib,);

        println!(
            "  Minimum candidate:      {} bytes",
            format_bytes(self.minimum_file_bytes,),
        );

        println!(
            "  Literal staging cap:    {} bytes",
            format_bytes(self.maximum_literal_bytes,),
        );

        println!();

        println!(
            "  Source files:           {} / {} bytes",
            format_bytes(self.source_files),
            format_bytes(self.source_bytes),
        );

        println!(
            "  Same-path candidates:   {}",
            format_bytes(self.same_path_candidates,),
        );

        println!(
            "  Deduplicated files:     {}",
            format_bytes(self.deduplicated_files,),
        );

        println!(
            "  Skipped small:          {}",
            format_bytes(self.skipped_small_files,),
        );

        println!(
            "  Missing basis:          {}",
            format_bytes(self.missing_basis_files,),
        );

        println!(
            "  Sample rejected:        {}",
            format_bytes(self.sample_rejected_files,),
        );

        println!(
            "  Literal-cap fallback:   {}",
            format_bytes(self.literal_limit_fallbacks,),
        );

        println!(
            "  No-savings fallback:    {}",
            format_bytes(self.no_savings_fallbacks,),
        );

        println!();

        println!(
            "  Planned candidate data: {} bytes",
            format_bytes(self.candidate_bytes,),
        );

        println!(
            "  Reused basis data:      {} bytes ({:.2}%)",
            format_bytes(self.referenced_bytes,),
            percent(self.referenced_bytes, self.candidate_bytes,),
        );

        println!(
            "  Literal data:           {} bytes",
            format_bytes(self.literal_bytes,),
        );

        println!(
            "  Basis ranges:           {}",
            format_bytes(self.basis_ranges,),
        );

        println!(
            "  Literal ranges:         {}",
            format_bytes(self.literal_ranges,),
        );

        println!();

        println!(
            "  Index wire:             {} bytes",
            format_bytes(self.index_wire_bytes,),
        );

        println!(
            "  Dedup plan wire:        {} bytes",
            format_bytes(self.plan_wire_bytes,),
        );

        println!(
            "  Full-file fallback:     {} bytes",
            format_bytes(self.fallback_wire_bytes,),
        );

        println!(
            "  Total planned wire:     {} bytes",
            format_bytes(self.total_wire_bytes,),
        );

        println!(
            "  Peak staged payload:    {} bytes",
            format_bytes(self.peak_staged_payload_bytes,),
        );

        println!();

        println!(
            "  Basis indexing:         {:.6} s",
            self.index_elapsed.as_secs_f64(),
        );

        println!(
            "  Sender planning:        {:.6} s",
            self.plan_elapsed.as_secs_f64(),
        );

        println!(
            "  Complete planning:      {:.6} s",
            self.total_elapsed.as_secs_f64(),
        );

        println!(
            "  Logical planning rate:  {:.2} MB/s",
            decimal_megabytes_per_second(self.source_bytes, self.total_elapsed,),
        );
    }
}

pub fn validate_limits(minimum_file_mib: usize, maximum_literal_mib: usize) -> io::Result<()> {
    if minimum_file_mib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "minimum candidate size must be at least 1 MiB",
        ));
    }

    if maximum_literal_mib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "maximum literal staging must be at least 1 MiB",
        ));
    }

    Ok(())
}

pub fn run(
    source_root: &Path,
    destination_root: &Path,
    average_kib: usize,
    minimum_file_mib: usize,
    maximum_literal_mib: usize,
) -> io::Result<FolderDedupReport> {
    validate_limits(minimum_file_mib, maximum_literal_mib)?;

    validate_directory(source_root, "source root")?;

    validate_directory(destination_root, "destination root")?;

    let minimum_file_bytes = mib_to_bytes(minimum_file_mib, "minimum candidate size")?;

    let maximum_literal_bytes = mib_to_bytes(maximum_literal_mib, "maximum literal staging")?;

    let started = Instant::now();

    let relative_files = collect_regular_files(source_root)?;

    let mut report = FolderDedupReport {
        average_kib,
        minimum_file_bytes,
        maximum_literal_bytes,

        source_files: 0,
        source_bytes: 0,

        same_path_candidates: 0,
        deduplicated_files: 0,

        skipped_small_files: 0,
        missing_basis_files: 0,
        sample_rejected_files: 0,
        literal_limit_fallbacks: 0,
        no_savings_fallbacks: 0,

        candidate_bytes: 0,
        referenced_bytes: 0,
        literal_bytes: 0,

        index_wire_bytes: 0,
        plan_wire_bytes: 0,
        fallback_wire_bytes: 0,
        total_wire_bytes: 0,

        basis_ranges: 0,
        literal_ranges: 0,

        peak_staged_payload_bytes: 0,

        index_elapsed: Duration::ZERO,
        plan_elapsed: Duration::ZERO,
        total_elapsed: Duration::ZERO,
    };

    for relative_path in relative_files {
        let source_path = source_root.join(&relative_path);

        let source_bytes = regular_file_bytes(&source_path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "source file disappeared during planning: {}",
                    source_path.display(),
                ),
            )
        })?;

        add(&mut report.source_files, 1, "source file count")?;

        add(&mut report.source_bytes, source_bytes, "source byte count")?;

        if source_bytes < minimum_file_bytes {
            add(&mut report.skipped_small_files, 1, "small-file skip count")?;

            add(
                &mut report.fallback_wire_bytes,
                source_bytes,
                "fallback wire bytes",
            )?;

            continue;
        }

        let basis_path = destination_root.join(&relative_path);

        let Some(basis_bytes) = regular_file_bytes(&basis_path)? else {
            add(&mut report.missing_basis_files, 1, "missing-basis count")?;

            add(
                &mut report.fallback_wire_bytes,
                source_bytes,
                "fallback wire bytes",
            )?;

            continue;
        };

        if basis_bytes == 0
            || !likely_similar(&source_path, source_bytes, &basis_path, basis_bytes)?
        {
            add(
                &mut report.sample_rejected_files,
                1,
                "sample rejection count",
            )?;

            add(
                &mut report.fallback_wire_bytes,
                source_bytes,
                "fallback wire bytes",
            )?;

            continue;
        }

        add(
            &mut report.same_path_candidates,
            1,
            "same-path candidate count",
        )?;

        let receiver_index = BasisFileIndex::build(&basis_path, average_kib)?;

        report.index_elapsed += receiver_index.build_elapsed();

        let index_wire = receiver_index.encode_wire()?;

        let index_wire_bytes = u64::try_from(index_wire.len())
            .map_err(|_| io::Error::other("index wire length cannot be represented"))?;

        let sender_index = BasisFileIndex::decode_wire(&index_wire)?;

        drop(receiver_index);
        drop(index_wire);

        let plan = match ReconstructionPlan::build_bounded(
            &source_path,
            &sender_index,
            maximum_literal_bytes,
        ) {
            Ok(plan) => plan,

            Err(error) if is_literal_limit_exceeded(&error) => {
                add(
                    &mut report.literal_limit_fallbacks,
                    1,
                    "literal-limit fallback count",
                )?;

                add(
                    &mut report.index_wire_bytes,
                    index_wire_bytes,
                    "index wire bytes",
                )?;

                add(
                    &mut report.fallback_wire_bytes,
                    source_bytes,
                    "fallback wire bytes",
                )?;

                report.peak_staged_payload_bytes =
                    report.peak_staged_payload_bytes.max(index_wire_bytes);

                continue;
            }

            Err(error) => return Err(error),
        };

        report.plan_elapsed += plan.build_elapsed();

        let plan_wire = plan.encode_wire()?;

        let plan_wire_bytes = u64::try_from(plan_wire.len())
            .map_err(|_| io::Error::other("plan wire length cannot be represented"))?;

        let candidate_wire_bytes = index_wire_bytes
            .checked_add(plan_wire_bytes)
            .ok_or_else(|| io::Error::other("candidate wire size overflowed"))?;

        report.peak_staged_payload_bytes = report
            .peak_staged_payload_bytes
            .max(index_wire_bytes)
            .max(plan_wire_bytes);

        if candidate_wire_bytes >= source_bytes {
            add(
                &mut report.no_savings_fallbacks,
                1,
                "no-savings fallback count",
            )?;

            add(
                &mut report.index_wire_bytes,
                index_wire_bytes,
                "index wire bytes",
            )?;

            add(
                &mut report.fallback_wire_bytes,
                source_bytes,
                "fallback wire bytes",
            )?;

            continue;
        }

        add(&mut report.deduplicated_files, 1, "deduplicated file count")?;

        add(
            &mut report.candidate_bytes,
            plan.target_bytes(),
            "candidate bytes",
        )?;

        add(
            &mut report.referenced_bytes,
            plan.referenced_bytes(),
            "referenced bytes",
        )?;

        add(
            &mut report.literal_bytes,
            plan.literal_bytes(),
            "literal bytes",
        )?;

        add(
            &mut report.index_wire_bytes,
            index_wire_bytes,
            "index wire bytes",
        )?;

        add(
            &mut report.plan_wire_bytes,
            plan_wire_bytes,
            "plan wire bytes",
        )?;

        add(
            &mut report.basis_ranges,
            u64::try_from(plan.basis_range_count())
                .map_err(|_| io::Error::other("basis range count cannot be represented"))?,
            "basis range count",
        )?;

        add(
            &mut report.literal_ranges,
            u64::try_from(plan.literal_range_count())
                .map_err(|_| io::Error::other("literal range count cannot be represented"))?,
            "literal range count",
        )?;

        drop(plan_wire);
        drop(plan);
        drop(sender_index);
    }

    report.total_wire_bytes = report
        .index_wire_bytes
        .checked_add(report.plan_wire_bytes)
        .and_then(|bytes| bytes.checked_add(report.fallback_wire_bytes))
        .ok_or_else(|| io::Error::other("total folder wire size overflowed"))?;

    report.total_elapsed = started.elapsed();

    Ok(report)
}

fn collect_regular_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    let mut directories = vec![PathBuf::new()];

    while let Some(relative_directory) = directories.pop() {
        let absolute_directory = root.join(&relative_directory);

        for entry in fs::read_dir(&absolute_directory)? {
            let entry = entry?;

            let relative_path = relative_directory.join(entry.file_name());

            let metadata = fs::symlink_metadata(entry.path())?;

            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }

            if metadata.is_dir() {
                directories.push(relative_path);
            } else if metadata.is_file() {
                files.push(relative_path);
            }
        }
    }

    files.sort();

    Ok(files)
}

fn likely_similar(
    source_path: &Path,
    source_bytes: u64,
    basis_path: &Path,
    basis_bytes: u64,
) -> io::Result<bool> {
    let common_bytes = source_bytes.min(basis_bytes);

    if common_bytes == 0 {
        return Ok(false);
    }

    let sample_bytes = common_bytes.min(SAMPLE_BYTES as u64);

    if samples_match(source_path, 0, basis_path, 0, sample_bytes)? {
        return Ok(true);
    }

    samples_match(
        source_path,
        source_bytes - sample_bytes,
        basis_path,
        basis_bytes - sample_bytes,
        sample_bytes,
    )
}

fn samples_match(
    source_path: &Path,
    source_offset: u64,
    basis_path: &Path,
    basis_offset: u64,
    sample_bytes: u64,
) -> io::Result<bool> {
    let sample_bytes = usize::try_from(sample_bytes)
        .map_err(|_| io::Error::other("sample length cannot be represented"))?;

    let mut source = File::open(source_path)?;

    let mut basis = File::open(basis_path)?;

    source.seek(SeekFrom::Start(source_offset))?;

    basis.seek(SeekFrom::Start(basis_offset))?;

    let mut source_sample = vec![0_u8; sample_bytes];

    let mut basis_sample = vec![0_u8; sample_bytes];

    source.read_exact(&mut source_sample)?;

    basis.read_exact(&mut basis_sample)?;

    Ok(source_sample == basis_sample)
}

fn regular_file_bytes(path: &Path) -> io::Result<Option<u64>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,

        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }

        Err(error) => return Err(error),
    };

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Ok(None);
    }

    Ok(Some(metadata.len()))
}

fn validate_directory(path: &Path, description: &str) -> io::Result<()> {
    let metadata = fs::metadata(path)?;

    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} is not a directory: {}", path.display(),),
        ));
    }

    Ok(())
}

fn mib_to_bytes(value: usize, description: &str) -> io::Result<u64> {
    let bytes = value
        .checked_mul(1024 * 1024)
        .ok_or_else(|| io::Error::other(format!("{description} overflowed",)))?;

    u64::try_from(bytes)
        .map_err(|_| io::Error::other(format!("{description} cannot be represented",)))
}

fn add(total: &mut u64, value: u64, description: &str) -> io::Result<()> {
    *total = total
        .checked_add(value)
        .ok_or_else(|| io::Error::other(format!("{description} overflowed",)))?;

    Ok(())
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
    use super::run;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn folder_planner_deduplicates_one_file_at_a_time() {
        let root = temporary_root("dedup");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let basis = deterministic_bytes(2 * 1024 * 1024, 0x1234_5678_9ABC_DEF0);

        let insertion = deterministic_bytes(4097, 0x0FED_CBA9_8765_4321);

        let offset = 1024 * 1024 + 123;

        let mut candidate = Vec::with_capacity(basis.len() + insertion.len());

        candidate.extend_from_slice(&basis[..offset]);

        candidate.extend_from_slice(&insertion);

        candidate.extend_from_slice(&basis[offset..]);

        fs::write(destination_root.join("file.bin"), &basis).unwrap();

        fs::write(source_root.join("file.bin"), &candidate).unwrap();

        let report = run(&source_root, &destination_root, 64, 1, 64).unwrap();

        assert_eq!(report.deduplicated_files, 1,);

        assert!(report.referenced_bytes > report.candidate_bytes * 90 / 100,);

        assert!(report.peak_staged_payload_bytes <= 64 * 1024 * 1024,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_planner_falls_back_when_literal_cap_is_reached() {
        let root = temporary_root("literal-cap");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let basis = deterministic_bytes(4 * 1024 * 1024, 0xAAAA_BBBB_CCCC_DDDD);

        let mut candidate = basis.clone();

        let replacement = deterministic_bytes(3 * 1024 * 1024, 0x1111_2222_3333_4444);

        candidate[512 * 1024..512 * 1024 + replacement.len()].copy_from_slice(&replacement);

        fs::write(destination_root.join("file.bin"), &basis).unwrap();

        fs::write(source_root.join("file.bin"), &candidate).unwrap();

        let report = run(&source_root, &destination_root, 64, 1, 1).unwrap();

        assert_eq!(report.literal_limit_fallbacks, 1,);

        assert_eq!(report.deduplicated_files, 0,);

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
            "networkcopy-folder-dedup-{name}-{}-{unique}",
            process::id(),
        ))
    }
}
