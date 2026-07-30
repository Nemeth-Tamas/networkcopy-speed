use std::collections::{BTreeSet, VecDeque};
use std::io;

pub const AVERAGE_CHUNK_BYTES: u64 = 64 * 1024;

pub const DEFAULT_GENERATION_TARGET_BYTES: u64 = 512 * 1024 * 1024;

pub const DEFAULT_MAX_CATALOG_ENTRIES: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogCandidate {
    pub file_id: usize,

    pub logical_bytes: u64,
}

impl CatalogCandidate {
    pub fn new(file_id: usize, logical_bytes: u64) -> io::Result<Self> {
        if logical_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC candidate must not be empty",
            ));
        }

        Ok(Self {
            file_id,
            logical_bytes,
        })
    }

    pub fn estimated_entries(self) -> io::Result<u64> {
        self.logical_bytes
            .checked_add(AVERAGE_CHUNK_BYTES - 1)
            .map(|bytes| bytes / AVERAGE_CHUNK_BYTES)
            .ok_or_else(|| io::Error::other("session CDC chunk estimate overflowed"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogLimits {
    pub generation_target_bytes: u64,

    pub max_catalog_entries: u64,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            generation_target_bytes: DEFAULT_GENERATION_TARGET_BYTES,

            max_catalog_entries: DEFAULT_MAX_CATALOG_ENTRIES,
        }
    }
}

impl CatalogLimits {
    pub fn validate(self) -> io::Result<()> {
        if self.generation_target_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC generation target must not be zero",
            ));
        }

        if self.max_catalog_entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC catalog limit must not be zero",
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogGeneration {
    pub index: usize,

    pub basis_file_ids: Vec<usize>,

    pub transfer_files: Vec<CatalogCandidate>,

    pub published_file_ids: Vec<usize>,

    pub uncataloged_file_ids: Vec<usize>,

    pub evicted_file_ids: Vec<usize>,

    pub logical_bytes: u64,

    pub catalog_entries_before: u64,

    pub catalog_entries_after: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogPlan {
    pub generations: Vec<CatalogGeneration>,

    pub candidate_files: u64,

    pub candidate_bytes: u64,

    pub peak_catalog_entries: u64,
}

pub fn plan(candidates: &[CatalogCandidate], limits: CatalogLimits) -> io::Result<CatalogPlan> {
    limits.validate()?;

    let mut candidates = candidates.to_vec();

    candidates.sort_unstable_by_key(|candidate| candidate.file_id);

    validate_candidates(&candidates)?;

    let batches = build_generation_batches(candidates, limits)?;

    let mut catalog = VecDeque::<(usize, u64)>::new();

    let mut catalog_entries = 0_u64;

    let mut peak_catalog_entries = 0_u64;

    let mut generations = Vec::with_capacity(batches.len());

    let mut candidate_files = 0_u64;

    let mut candidate_bytes = 0_u64;

    for (generation_index, transfer_files) in batches.into_iter().enumerate() {
        let basis_file_ids = catalog.iter().map(|(file_id, _)| *file_id).collect();

        let catalog_entries_before = catalog_entries;

        let logical_bytes = sum_candidate_bytes(&transfer_files)?;

        candidate_files = candidate_files
            .checked_add(u64::try_from(transfer_files.len()).map_err(|_| {
                io::Error::other("session CDC candidate count cannot be represented")
            })?)
            .ok_or_else(|| io::Error::other("session CDC candidate count overflowed"))?;

        candidate_bytes = candidate_bytes
            .checked_add(logical_bytes)
            .ok_or_else(|| io::Error::other("session CDC candidate byte count overflowed"))?;

        let mut published_file_ids = Vec::new();

        let mut uncataloged_file_ids = Vec::new();

        let mut evicted_file_ids = Vec::new();

        for candidate in &transfer_files {
            let estimated_entries = candidate.estimated_entries()?;

            if estimated_entries > limits.max_catalog_entries {
                uncataloged_file_ids.push(candidate.file_id);

                continue;
            }

            while catalog_entries
                .checked_add(estimated_entries)
                .is_none_or(|entries| entries > limits.max_catalog_entries)
            {
                let Some((evicted_file_id, evicted_entries)) = catalog.pop_front() else {
                    return Err(io::Error::other(
                        "session CDC catalog could not satisfy its entry limit",
                    ));
                };

                catalog_entries =
                    catalog_entries
                        .checked_sub(evicted_entries)
                        .ok_or_else(|| {
                            io::Error::other("session CDC catalog entry count underflowed")
                        })?;

                evicted_file_ids.push(evicted_file_id);
            }

            catalog.push_back((candidate.file_id, estimated_entries));

            catalog_entries = catalog_entries
                .checked_add(estimated_entries)
                .ok_or_else(|| io::Error::other("session CDC catalog entry count overflowed"))?;

            published_file_ids.push(candidate.file_id);
        }

        peak_catalog_entries = peak_catalog_entries.max(catalog_entries);

        generations.push(CatalogGeneration {
            index: generation_index,

            basis_file_ids,

            transfer_files,

            published_file_ids,

            uncataloged_file_ids,

            evicted_file_ids,

            logical_bytes,

            catalog_entries_before,

            catalog_entries_after: catalog_entries,
        });
    }

    let plan = CatalogPlan {
        generations,

        candidate_files,

        candidate_bytes,

        peak_catalog_entries,
    };

    validate_plan(&plan, limits)?;

    Ok(plan)
}

pub fn validate_plan(plan: &CatalogPlan, limits: CatalogLimits) -> io::Result<()> {
    limits.validate()?;

    let mut catalog = VecDeque::<(usize, u64)>::new();

    let mut catalog_entries = 0_u64;

    let mut peak_catalog_entries = 0_u64;

    let mut seen_file_ids = BTreeSet::new();

    let mut candidate_files = 0_u64;

    let mut candidate_bytes = 0_u64;

    for (expected_index, generation) in plan.generations.iter().enumerate() {
        if generation.index != expected_index {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation index is not contiguous",
            ));
        }

        if generation.transfer_files.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation is empty",
            ));
        }

        let expected_basis: Vec<usize> = catalog.iter().map(|(file_id, _)| *file_id).collect();

        if generation.basis_file_ids != expected_basis {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation basis does not match the committed catalog",
            ));
        }

        if generation.catalog_entries_before != catalog_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation reports an incorrect starting catalog size",
            ));
        }

        let logical_bytes = sum_candidate_bytes(&generation.transfer_files)?;

        if logical_bytes != generation.logical_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation logical byte count is incorrect",
            ));
        }

        candidate_files = candidate_files
            .checked_add(u64::try_from(generation.transfer_files.len()).map_err(|_| {
                io::Error::other("session CDC generation file count cannot be represented")
            })?)
            .ok_or_else(|| io::Error::other("session CDC validation file count overflowed"))?;

        candidate_bytes = candidate_bytes
            .checked_add(logical_bytes)
            .ok_or_else(|| io::Error::other("session CDC validation byte count overflowed"))?;

        let mut expected_published = Vec::new();

        let mut expected_uncataloged = Vec::new();

        let mut expected_evicted = Vec::new();

        for candidate in &generation.transfer_files {
            if !seen_file_ids.insert(candidate.file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session CDC plan transfers the same file more than once",
                ));
            }

            if generation.basis_file_ids.contains(&candidate.file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session CDC generation references one of its own transfer files",
                ));
            }

            let estimated_entries = candidate.estimated_entries()?;

            if estimated_entries > limits.max_catalog_entries {
                expected_uncataloged.push(candidate.file_id);

                continue;
            }

            while catalog_entries
                .checked_add(estimated_entries)
                .is_none_or(|entries| entries > limits.max_catalog_entries)
            {
                let Some((evicted_file_id, evicted_entries)) = catalog.pop_front() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "session CDC validation could not satisfy the catalog limit",
                    ));
                };

                catalog_entries =
                    catalog_entries
                        .checked_sub(evicted_entries)
                        .ok_or_else(|| {
                            io::Error::other("session CDC validation catalog underflowed")
                        })?;

                expected_evicted.push(evicted_file_id);
            }

            catalog.push_back((candidate.file_id, estimated_entries));

            catalog_entries = catalog_entries
                .checked_add(estimated_entries)
                .ok_or_else(|| io::Error::other("session CDC validation catalog overflowed"))?;

            expected_published.push(candidate.file_id);
        }

        if generation.published_file_ids != expected_published {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation published-file set is incorrect",
            ));
        }

        if generation.uncataloged_file_ids != expected_uncataloged {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation uncataloged-file set is incorrect",
            ));
        }

        if generation.evicted_file_ids != expected_evicted {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation eviction order is incorrect",
            ));
        }

        if generation.catalog_entries_after != catalog_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC generation reports an incorrect final catalog size",
            ));
        }

        if catalog_entries > limits.max_catalog_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session CDC catalog exceeds its configured limit",
            ));
        }

        peak_catalog_entries = peak_catalog_entries.max(catalog_entries);
    }

    if plan.candidate_files != candidate_files {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan candidate count is incorrect",
        ));
    }

    if plan.candidate_bytes != candidate_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan candidate byte count is incorrect",
        ));
    }

    if plan.peak_catalog_entries != peak_catalog_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan peak catalog size is incorrect",
        ));
    }

    Ok(())
}

fn validate_candidates(candidates: &[CatalogCandidate]) -> io::Result<()> {
    let mut file_ids = BTreeSet::new();

    for candidate in candidates {
        if candidate.logical_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC candidate must not be empty",
            ));
        }

        if !file_ids.insert(candidate.file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC candidate list contains a duplicate file ID",
            ));
        }

        candidate.estimated_entries()?;
    }

    Ok(())
}

fn build_generation_batches(
    candidates: Vec<CatalogCandidate>,
    limits: CatalogLimits,
) -> io::Result<Vec<Vec<CatalogCandidate>>> {
    let mut generations = Vec::new();

    let mut current = Vec::new();

    let mut current_bytes = 0_u64;

    for candidate in candidates {
        let combined_bytes = current_bytes
            .checked_add(candidate.logical_bytes)
            .ok_or_else(|| io::Error::other("session CDC generation byte count overflowed"))?;

        if !current.is_empty() && combined_bytes > limits.generation_target_bytes {
            generations.push(std::mem::take(&mut current));

            current_bytes = 0;
        }

        current_bytes = current_bytes
            .checked_add(candidate.logical_bytes)
            .ok_or_else(|| io::Error::other("session CDC generation byte count overflowed"))?;

        current.push(candidate);
    }

    if !current.is_empty() {
        generations.push(current);
    }

    Ok(generations)
}

fn sum_candidate_bytes(candidates: &[CatalogCandidate]) -> io::Result<u64> {
    candidates.iter().try_fold(0_u64, |total, candidate| {
        total
            .checked_add(candidate.logical_bytes)
            .ok_or_else(|| io::Error::other("session CDC logical byte count overflowed"))
    })
}

#[cfg(test)]
mod tests {
    use super::{AVERAGE_CHUNK_BYTES, CatalogCandidate, CatalogLimits, plan, validate_plan};

    fn candidate(file_id: usize, logical_bytes: u64) -> CatalogCandidate {
        CatalogCandidate::new(file_id, logical_bytes).unwrap()
    }

    #[test]
    fn generations_use_only_prior_committed_files_as_bases() {
        let four_mib = 4 * 1024 * 1024;

        let limits = CatalogLimits {
            generation_target_bytes: 8 * 1024 * 1024,

            max_catalog_entries: 1_000,
        };

        let plan = plan(
            &[
                candidate(0, four_mib),
                candidate(1, four_mib),
                candidate(2, four_mib),
            ],
            limits,
        )
        .unwrap();

        assert_eq!(plan.generations.len(), 2,);

        assert_eq!(plan.generations[0].basis_file_ids, Vec::<usize>::new(),);

        assert_eq!(
            plan.generations[0]
                .transfer_files
                .iter()
                .map(|candidate| { candidate.file_id })
                .collect::<Vec<_>>(),
            vec![0, 1],
        );

        assert_eq!(plan.generations[1].basis_file_ids, vec![0, 1],);

        assert_eq!(
            plan.generations[1].transfer_files,
            vec![candidate(2, four_mib,),],
        );

        validate_plan(&plan, limits).unwrap();
    }

    #[test]
    fn catalog_evicts_oldest_files_first() {
        let limits = CatalogLimits {
            generation_target_bytes: 1,

            max_catalog_entries: 2,
        };

        let plan = plan(
            &[
                candidate(0, AVERAGE_CHUNK_BYTES),
                candidate(1, AVERAGE_CHUNK_BYTES),
                candidate(2, AVERAGE_CHUNK_BYTES),
            ],
            limits,
        )
        .unwrap();

        assert_eq!(plan.generations.len(), 3,);

        assert_eq!(plan.generations[2].basis_file_ids, vec![0, 1],);

        assert_eq!(plan.generations[2].evicted_file_ids, vec![0],);

        assert_eq!(plan.generations[2].published_file_ids, vec![2],);

        assert_eq!(plan.generations[2].catalog_entries_after, 2,);

        assert_eq!(plan.peak_catalog_entries, 2,);
    }

    #[test]
    fn oversized_file_transfers_but_is_not_cataloged() {
        let limits = CatalogLimits {
            generation_target_bytes: 1024 * 1024,

            max_catalog_entries: 2,
        };

        let plan = plan(&[candidate(7, 3 * AVERAGE_CHUNK_BYTES)], limits).unwrap();

        assert_eq!(plan.generations.len(), 1,);

        assert_eq!(plan.generations[0].published_file_ids, Vec::<usize>::new(),);

        assert_eq!(plan.generations[0].uncataloged_file_ids, vec![7],);

        assert_eq!(plan.generations[0].catalog_entries_after, 0,);

        assert_eq!(plan.candidate_files, 1,);
    }

    #[test]
    fn duplicate_file_ids_are_rejected() {
        let error = plan(
            &[
                candidate(4, AVERAGE_CHUNK_BYTES),
                candidate(4, AVERAGE_CHUNK_BYTES),
            ],
            CatalogLimits::default(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput,);

        assert!(error.to_string().contains("duplicate file ID",),);
    }

    #[test]
    fn zero_limits_are_rejected() {
        let generation_error = plan(
            &[],
            CatalogLimits {
                generation_target_bytes: 0,

                max_catalog_entries: 1,
            },
        )
        .unwrap_err();

        assert_eq!(generation_error.kind(), std::io::ErrorKind::InvalidInput,);

        let catalog_error = plan(
            &[],
            CatalogLimits {
                generation_target_bytes: 1,

                max_catalog_entries: 0,
            },
        )
        .unwrap_err();

        assert_eq!(catalog_error.kind(), std::io::ErrorKind::InvalidInput,);
    }

    #[test]
    fn empty_candidate_set_produces_empty_plan() {
        let limits = CatalogLimits::default();

        let plan = plan(&[], limits).unwrap();

        assert!(plan.generations.is_empty(),);

        assert_eq!(plan.candidate_files, 0,);

        assert_eq!(plan.candidate_bytes, 0,);

        assert_eq!(plan.peak_catalog_entries, 0,);

        validate_plan(&plan, limits).unwrap();
    }
}
