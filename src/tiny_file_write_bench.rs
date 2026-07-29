use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::manifest_scan::{self, FileClass};
use crate::tiny_file_materialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_WORKERS: usize = 64;
const DEFAULT_WORKER_CAP: usize = 16;
const MAX_PRELOAD_BYTES: u64 = 512 * 1024 * 1024;
const RECOMMENDATION_TOLERANCE_PERCENT: u128 = 5;

#[derive(Debug)]
pub struct TinyFileWriteBenchReport {
    pub scan_workers: usize,
    pub tiny_files: u64,
    pub tiny_bytes: u64,
    pub preload_elapsed: Duration,
    pub runs: Vec<TinyFileWriteRun>,
    pub recommended_workers: usize,
}

impl TinyFileWriteBenchReport {
    pub fn print(&self) {
        println!("Tiny-file filesystem worker calibration complete");
        println!("  Scanner workers:     {}", self.scan_workers);
        println!("  Tiny files:          {}", format_bytes(self.tiny_files),);
        println!(
            "  Tiny data:           {} bytes",
            format_bytes(self.tiny_bytes),
        );
        println!(
            "  Preload time:        {:.6} s",
            self.preload_elapsed.as_secs_f64(),
        );
        println!();

        let baseline = self.runs.first().map(|run| run.elapsed);

        for run in &self.runs {
            println!("  Workers:             {}", run.worker_count);
            println!("    Materialization:   {:.6} s", run.elapsed.as_secs_f64(),);
            println!(
                "    Files/s:           {:.0}",
                files_per_second(self.tiny_files, run.elapsed),
            );
            println!(
                "    Payload rate:      {:.2} MB/s",
                decimal_megabytes_per_second(self.tiny_bytes, run.elapsed),
            );

            if let Some(baseline) = baseline {
                println!(
                    "    Speedup vs 1:      {:.2}x",
                    speedup(baseline, run.elapsed),
                );
            }

            println!();
        }

        println!("  Recommended workers: {}", self.recommended_workers,);
        println!(
            "  Selection policy:    smallest count within {}% of the fastest run",
            RECOMMENDATION_TOLERANCE_PERCENT,
        );
        println!("  Integrity:           atomic materialization completed");
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TinyFileWriteRun {
    pub worker_count: usize,
    pub elapsed: Duration,
}

#[derive(Debug)]
struct TinyFileJob {
    file_id: usize,
    relative_path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct WorkerResult {
    files: u64,
    bytes: u64,
}

pub fn default_max_workers() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get().min(DEFAULT_WORKER_CAP))
        .unwrap_or(4)
}

pub fn validate_max_workers(max_workers: usize) -> io::Result<()> {
    if !(1..=MAX_WORKERS).contains(&max_workers) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("maximum worker count must be between 1 and {MAX_WORKERS}"),
        ));
    }

    Ok(())
}

pub fn run(
    source_root: &Path,
    output_root: &Path,
    max_workers: usize,
    scan_workers: usize,
) -> io::Result<TinyFileWriteBenchReport> {
    validate_max_workers(max_workers)?;
    manifest_scan::validate_worker_count(scan_workers)?;

    let source_root = source_root.canonicalize()?;
    let scan = manifest_scan::run(&source_root, scan_workers)?;

    prepare_empty_output_root(output_root)?;

    let output_root = output_root.canonicalize()?;

    if output_root.starts_with(&source_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "benchmark output directory must not be inside the source directory",
        ));
    }

    let preload_started = Instant::now();

    let mut jobs = Vec::new();
    let mut tiny_bytes = 0_u64;

    for (file_id, entry) in scan.manifest.into_iter().enumerate() {
        if entry.class != FileClass::Tiny {
            continue;
        }

        tiny_bytes = tiny_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("tiny-file byte count overflowed"))?;

        if tiny_bytes > MAX_PRELOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "tiny-file benchmark contains more than {} preload bytes",
                    MAX_PRELOAD_BYTES,
                ),
            ));
        }

        let path = source_root.join(&entry.relative_path);
        let contents = fs::read(&path)?;

        let expected_size = usize::try_from(entry.file_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tiny-file size cannot be represented: {}", path.display()),
            )
        })?;

        if contents.len() != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "tiny file changed while being preloaded: {}",
                    path.display(),
                ),
            ));
        }

        jobs.push(TinyFileJob {
            file_id,
            relative_path: entry.relative_path,
            contents,
        });
    }

    if jobs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source directory does not contain any tiny files",
        ));
    }

    let preload_elapsed = preload_started.elapsed();

    let tiny_files = u64::try_from(jobs.len())
        .map_err(|_| io::Error::other("tiny-file count cannot be represented"))?;

    let mut runs = Vec::new();

    for worker_count in candidate_worker_counts(max_workers) {
        runs.push(run_worker_count(
            &output_root,
            &jobs,
            worker_count,
            tiny_files,
            tiny_bytes,
        )?);
    }

    let recommended_workers = recommend_worker_count(&runs)?;

    Ok(TinyFileWriteBenchReport {
        scan_workers,
        tiny_files,
        tiny_bytes,
        preload_elapsed,
        runs,
        recommended_workers,
    })
}

fn prepare_empty_output_root(output_root: &Path) -> io::Result<()> {
    if output_root.try_exists()? {
        let metadata = fs::metadata(output_root)?;

        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "benchmark output is not a directory: {}",
                    output_root.display(),
                ),
            ));
        }

        if fs::read_dir(output_root)?.next().transpose()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "benchmark output directory must be empty: {}",
                    output_root.display(),
                ),
            ));
        }

        return Ok(());
    }

    fs::create_dir_all(output_root)
}

fn candidate_worker_counts(max_workers: usize) -> Vec<usize> {
    let mut candidates = vec![1];
    let mut worker_count = 2_usize;

    while worker_count < max_workers {
        candidates.push(worker_count);

        let Some(next) = worker_count.checked_mul(2) else {
            break;
        };

        worker_count = next;
    }

    if candidates.last().copied() != Some(max_workers) {
        candidates.push(max_workers);
    }

    candidates
}

fn run_worker_count(
    output_root: &Path,
    jobs: &[TinyFileJob],
    worker_count: usize,
    expected_files: u64,
    expected_bytes: u64,
) -> io::Result<TinyFileWriteRun> {
    let run_root = output_root.join(format!("workers-{worker_count:02}"));

    if run_root.try_exists()? {
        fs::remove_dir_all(&run_root)?;
    }

    fs::create_dir_all(&run_root)?;

    precreate_parent_directories(&run_root, jobs)?;

    let started = Instant::now();

    let materialization = materialize_with_workers(&run_root, jobs, worker_count);

    let elapsed = started.elapsed();

    let cleanup = fs::remove_dir_all(&run_root);

    let result = materialization?;

    cleanup?;

    if result.files != expected_files || result.bytes != expected_bytes {
        return Err(io::Error::other(format!(
            "worker run materialized {} files / {} bytes, expected \
             {expected_files} files / {expected_bytes} bytes",
            result.files, result.bytes,
        )));
    }

    Ok(TinyFileWriteRun {
        worker_count,
        elapsed,
    })
}

fn precreate_parent_directories(run_root: &Path, jobs: &[TinyFileJob]) -> io::Result<()> {
    for job in jobs {
        let Some(parent) = job.relative_path.parent() else {
            continue;
        };

        if parent.as_os_str().is_empty() {
            continue;
        }

        fs::create_dir_all(run_root.join(parent))?;
    }

    Ok(())
}

fn materialize_with_workers(
    destination_root: &Path,
    jobs: &[TinyFileJob],
    worker_count: usize,
) -> io::Result<WorkerResult> {
    let next_job = AtomicUsize::new(0);

    thread::scope(|scope| -> io::Result<WorkerResult> {
        let mut handles = Vec::with_capacity(worker_count);

        for worker_index in 0..worker_count {
            let next_job = &next_job;

            handles.push(
                thread::Builder::new()
                    .name(format!("networkcopy-tiny-write-bench-{worker_index}"))
                    .spawn_scoped(scope, move || {
                        let mut result = WorkerResult::default();

                        loop {
                            let job_index = next_job.fetch_add(1, Ordering::Relaxed);

                            let Some(job) = jobs.get(job_index) else {
                                break;
                            };

                            tiny_file_materialize::write_verified(
                                destination_root,
                                job.file_id,
                                &job.relative_path,
                                &job.contents,
                            )?;

                            result.files = result
                                .files
                                .checked_add(1)
                                .ok_or_else(|| io::Error::other("worker file count overflowed"))?;

                            result.bytes = result
                                .bytes
                                .checked_add(u64::try_from(job.contents.len()).map_err(|_| {
                                    io::Error::other("job byte count cannot be represented")
                                })?)
                                .ok_or_else(|| io::Error::other("worker byte count overflowed"))?;
                        }

                        Ok(result)
                    })?,
            );
        }

        let mut combined = WorkerResult::default();
        let mut first_error = None;

        for handle in handles {
            match handle.join() {
                Ok(Ok(result)) => {
                    combined.files = combined
                        .files
                        .checked_add(result.files)
                        .ok_or_else(|| io::Error::other("combined worker file count overflowed"))?;

                    combined.bytes = combined
                        .bytes
                        .checked_add(result.bytes)
                        .ok_or_else(|| io::Error::other("combined worker byte count overflowed"))?;
                }

                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }

                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(io::Error::other("tiny-file benchmark worker panicked"));
                    }
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }

        Ok(combined)
    })
}

fn recommend_worker_count(runs: &[TinyFileWriteRun]) -> io::Result<usize> {
    let best_elapsed = runs
        .iter()
        .map(|run| run.elapsed)
        .min()
        .ok_or_else(|| io::Error::other("worker benchmark produced no runs"))?;

    let best_nanos = best_elapsed.as_nanos();

    let threshold_nanos = best_nanos
        .checked_mul(100 + RECOMMENDATION_TOLERANCE_PERCENT)
        .ok_or_else(|| io::Error::other("worker recommendation overflowed"))?
        / 100;

    runs.iter()
        .filter(|run| run.elapsed.as_nanos() <= threshold_nanos)
        .map(|run| run.worker_count)
        .min()
        .ok_or_else(|| io::Error::other("worker benchmark produced no recommendation"))
}

fn files_per_second(files: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();

    if seconds == 0.0 {
        0.0
    } else {
        files as f64 / seconds
    }
}

fn speedup(baseline: Duration, candidate: Duration) -> f64 {
    let candidate_seconds = candidate.as_secs_f64();

    if candidate_seconds == 0.0 {
        0.0
    } else {
        baseline.as_secs_f64() / candidate_seconds
    }
}

#[cfg(test)]
mod tests {
    use super::{TinyFileWriteRun, candidate_worker_counts, recommend_worker_count};
    use std::time::Duration;

    #[test]
    fn candidate_counts_use_powers_of_two_and_exact_maximum() {
        assert_eq!(candidate_worker_counts(1), vec![1]);
        assert_eq!(candidate_worker_counts(8), vec![1, 2, 4, 8]);
        assert_eq!(candidate_worker_counts(12), vec![1, 2, 4, 8, 12]);
        assert_eq!(candidate_worker_counts(16), vec![1, 2, 4, 8, 16],);
    }

    #[test]
    fn recommendation_prefers_lower_count_within_five_percent() {
        let runs = vec![
            TinyFileWriteRun {
                worker_count: 1,
                elapsed: Duration::from_millis(200),
            },
            TinyFileWriteRun {
                worker_count: 2,
                elapsed: Duration::from_millis(105),
            },
            TinyFileWriteRun {
                worker_count: 4,
                elapsed: Duration::from_millis(100),
            },
            TinyFileWriteRun {
                worker_count: 8,
                elapsed: Duration::from_millis(101),
            },
        ];

        assert_eq!(recommend_worker_count(&runs).unwrap(), 2);
    }
}
