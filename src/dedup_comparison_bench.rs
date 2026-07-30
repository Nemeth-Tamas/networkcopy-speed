use crate::content_defined_dedup_bench;
use crate::copy_bench::{decimal_megabytes_per_second, format_bytes};
use crate::fixed_block_dedup_bench;
use std::io;
use std::path::Path;
use std::time::Duration;

pub const SIZE_KIB_MATRIX: [usize; 4] = [4, 16, 64, 256];

#[derive(Debug)]
pub struct DedupComparisonRow {
    pub size_kib: usize,

    pub fixed_reusable_bytes: u64,
    pub cdc_reusable_bytes: u64,

    pub fixed_literal_bytes: u64,
    pub cdc_literal_bytes: u64,

    pub fixed_index_bytes: u64,
    pub cdc_index_bytes: u64,

    pub fixed_elapsed: Duration,
    pub cdc_elapsed: Duration,
}

#[derive(Debug)]
pub struct DedupComparisonReport {
    pub basis_bytes: u64,
    pub candidate_bytes: u64,
    pub rows: Vec<DedupComparisonRow>,
}

impl DedupComparisonReport {
    pub fn print(&self) {
        let analyzed_bytes = self.basis_bytes.saturating_add(self.candidate_bytes);

        println!("Fixed-block versus content-defined dedup matrix complete",);

        println!(
            "  Basis data:       {} bytes",
            format_bytes(self.basis_bytes),
        );

        println!(
            "  Candidate data:   {} bytes",
            format_bytes(self.candidate_bytes),
        );

        println!();

        println!("Wire efficiency");

        println!(
            "  KiB   Fixed reuse    CDC reuse   Fixed literal     CDC literal   CDC reduction",
        );

        for row in &self.rows {
            println!(
                "  {:>3}      {:>6.2}%      {:>6.2}%  {:>14}  {:>14}         {:>6.2}%",
                row.size_kib,
                percent(row.fixed_reusable_bytes, self.candidate_bytes,),
                percent(row.cdc_reusable_bytes, self.candidate_bytes,),
                format_bytes(row.fixed_literal_bytes),
                format_bytes(row.cdc_literal_bytes),
                literal_reduction_percent(row.fixed_literal_bytes, row.cdc_literal_bytes,),
            );
        }

        println!();
        println!("Analysis cost");

        println!("  KiB     Fixed index       CDC index    Fixed MB/s      CDC MB/s",);

        for row in &self.rows {
            println!(
                "  {:>3}  {:>14}  {:>14}  {:>12.2}  {:>12.2}",
                row.size_kib,
                format_bytes(row.fixed_index_bytes),
                format_bytes(row.cdc_index_bytes),
                decimal_megabytes_per_second(analyzed_bytes, row.fixed_elapsed,),
                decimal_megabytes_per_second(analyzed_bytes, row.cdc_elapsed,),
            );
        }

        println!();

        println!("  Analysis bytes include one basis indexing pass and one candidate scan.",);

        println!("  Literal values exclude future chunk-reference protocol metadata.",);
    }
}

pub fn run(basis_path: &Path, candidate_path: &Path) -> io::Result<DedupComparisonReport> {
    let mut rows = Vec::with_capacity(SIZE_KIB_MATRIX.len());

    let mut basis_bytes = 0_u64;
    let mut candidate_bytes = 0_u64;

    for (matrix_index, size_kib) in SIZE_KIB_MATRIX.into_iter().enumerate() {
        let fixed = fixed_block_dedup_bench::run(basis_path, candidate_path, size_kib)?;

        let cdc = content_defined_dedup_bench::run(basis_path, candidate_path, size_kib)?;

        if fixed.basis_bytes != cdc.basis_bytes || fixed.candidate_bytes != cdc.candidate_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fixed and content-defined reports disagree \
                     about file sizes at {size_kib} KiB",
                ),
            ));
        }

        if matrix_index == 0 {
            basis_bytes = fixed.basis_bytes;
            candidate_bytes = fixed.candidate_bytes;
        } else if fixed.basis_bytes != basis_bytes || fixed.candidate_bytes != candidate_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deduplication input files changed during \
                 the comparison matrix",
            ));
        }

        let fixed_reusable_bytes = fixed
            .positional_bytes
            .checked_add(fixed.relocated_bytes)
            .ok_or_else(|| io::Error::other("fixed reusable byte count overflowed"))?;

        let cdc_reusable_bytes = cdc
            .positional_bytes
            .checked_add(cdc.relocated_bytes)
            .ok_or_else(|| io::Error::other("CDC reusable byte count overflowed"))?;

        rows.push(DedupComparisonRow {
            size_kib,

            fixed_reusable_bytes,
            cdc_reusable_bytes,

            fixed_literal_bytes: fixed.literal_bytes,
            cdc_literal_bytes: cdc.literal_bytes,

            fixed_index_bytes: fixed.index_payload_bytes,

            cdc_index_bytes: cdc.index_payload_bytes,

            fixed_elapsed: fixed.basis_elapsed + fixed.candidate_elapsed,

            cdc_elapsed: cdc.basis_elapsed + cdc.candidate_elapsed,
        });
    }

    Ok(DedupComparisonReport {
        basis_bytes,
        candidate_bytes,
        rows,
    })
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

fn literal_reduction_percent(fixed_literal_bytes: u64, cdc_literal_bytes: u64) -> f64 {
    if fixed_literal_bytes == 0 {
        return 0.0;
    }

    (fixed_literal_bytes as f64 - cdc_literal_bytes as f64) / fixed_literal_bytes as f64 * 100.0
}

#[cfg(test)]
mod tests {
    use super::{SIZE_KIB_MATRIX, literal_reduction_percent};

    #[test]
    fn comparison_matrix_uses_expected_sizes() {
        assert_eq!(SIZE_KIB_MATRIX, [4, 16, 64, 256],);
    }

    #[test]
    fn literal_reduction_handles_improvement_and_zero() {
        assert_eq!(literal_reduction_percent(0, 0), 0.0,);

        assert!((literal_reduction_percent(1_000_000, 100_000,) - 90.0).abs() < f64::EPSILON,);
    }
}
