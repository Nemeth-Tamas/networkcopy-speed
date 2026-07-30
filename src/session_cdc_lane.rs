use crate::cdc_basis_index::BasisFileIndex;
use crate::cdc_lane::{CdcLaneDecision, CdcLaneStats};
use crate::cdc_reconstruction_bench::{
    ReconstructionPlan, is_literal_limit_exceeded, reconstruct_verified,
};
use crate::content_defined_dedup_bench;
use crate::manifest_scan::{FileClass, ManifestEntry};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

pub(crate) const MESSAGE_SESSION_CDC_PLAN: u8 = 0x40;

const MINIMUM_TARGET_BYTES: u64 = 1024 * 1024;
const MAXIMUM_LITERAL_BYTES: u64 = 16 * 1024 * 1024;
const MAXIMUM_PLAN_WIRE_BYTES: usize = 20 * 1024 * 1024;

const MAXIMUM_BASIS_CANDIDATES: usize = 32;
const SIMILARITY_SAMPLE_BYTES: u64 = 64 * 1024;

const SESSION_CDC_PLAN_FIXED_WIRE_BYTES: u64 = 1 + 8 + 8 + 8 + 8;

pub(crate) fn sender_try_plan(
    writer: &mut impl Write,
    source_root: &Path,
    manifest: &[ManifestEntry],
    target_file_id: usize,
    basis_file_ids: &[usize],
) -> io::Result<CdcLaneDecision> {
    let target_entry = manifest.get(target_file_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session CDC target references unknown file ID {target_file_id}"),
        )
    })?;

    if target_entry.class != FileClass::Medium
        || target_entry.file_size < MINIMUM_TARGET_BYTES
        || basis_file_ids.is_empty()
    {
        return Ok(unavailable_decision());
    }

    let target_path = source_root.join(&target_entry.relative_path);

    validate_source_file(&target_path, target_entry, "session CDC target")?;

    let mut considered_basis_files = 0_usize;

    for &basis_file_id in basis_file_ids.iter().rev() {
        if basis_file_id == target_file_id {
            continue;
        }

        let basis_entry = manifest.get(basis_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session CDC catalog references unknown basis file ID {basis_file_id}"),
            )
        })?;

        if !matches!(basis_entry.class, FileClass::Medium | FileClass::Large)
            || basis_entry.file_size < MINIMUM_TARGET_BYTES
        {
            continue;
        }

        considered_basis_files = considered_basis_files
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session CDC basis count overflowed"))?;

        if considered_basis_files > MAXIMUM_BASIS_CANDIDATES {
            break;
        }

        let basis_path = source_root.join(&basis_entry.relative_path);

        validate_source_file(&basis_path, basis_entry, "session CDC basis")?;

        if !likely_similar(
            &target_path,
            target_entry.file_size,
            &basis_path,
            basis_entry.file_size,
        )? {
            continue;
        }

        let index = BasisFileIndex::build(
            &basis_path,
            content_defined_dedup_bench::DEFAULT_AVERAGE_KIB,
        )?;

        validate_source_file(&basis_path, basis_entry, "session CDC basis")?;

        if index.file_bytes() != basis_entry.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "session CDC basis index contains {} bytes, expected {}",
                    index.file_bytes(),
                    basis_entry.file_size,
                ),
            ));
        }

        let plan =
            match ReconstructionPlan::build_bounded(&target_path, &index, MAXIMUM_LITERAL_BYTES) {
                Ok(plan) => plan,

                Err(error) if is_literal_limit_exceeded(&error) => {
                    continue;
                }

                Err(error) => return Err(error),
            };

        validate_source_file(&target_path, target_entry, "session CDC target")?;

        if plan.referenced_bytes() == 0 {
            continue;
        }

        let plan_wire = plan.encode_wire()?;

        if plan_wire.len() > MAXIMUM_PLAN_WIRE_BYTES {
            continue;
        }

        let plan_wire_bytes = u64::try_from(plan_wire.len())
            .map_err(|_| io::Error::other("session CDC plan length cannot be represented"))?;

        let total_wire_bytes = SESSION_CDC_PLAN_FIXED_WIRE_BYTES
            .checked_add(plan_wire_bytes)
            .ok_or_else(|| io::Error::other("session CDC wire size overflowed"))?;

        if total_wire_bytes >= target_entry.file_size {
            continue;
        }

        write_u8(writer, MESSAGE_SESSION_CDC_PLAN)?;

        write_file_id(writer, target_file_id)?;

        write_file_id(writer, basis_file_id)?;

        write_u64(writer, index.file_bytes())?;

        write_u64(writer, plan_wire_bytes)?;

        writer.write_all(&plan_wire)?;

        writer.flush()?;

        return Ok(completed_decision(
            target_entry.file_size,
            &plan,
            plan_wire_bytes,
        ));
    }

    Ok(unavailable_decision())
}

pub(crate) fn receiver_apply_plan(
    reader: &mut impl Read,
    destination_root: &Path,
    manifest: &[ManifestEntry],
    expected_target_file_id: usize,
    allowed_basis_file_ids: &[usize],
) -> io::Result<CdcLaneDecision> {
    let target_entry = manifest.get(expected_target_file_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC receiver expected unknown target file ID {expected_target_file_id}"
            ),
        )
    })?;

    if target_entry.class != FileClass::Medium || target_entry.file_size < MINIMUM_TARGET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan targeted an ineligible file",
        ));
    }

    let announced_target_file_id = read_file_id(reader)?;

    if announced_target_file_id != expected_target_file_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC plan targeted file {announced_target_file_id}, expected {expected_target_file_id}",
            ),
        ));
    }

    let basis_file_id = read_file_id(reader)?;

    if basis_file_id == expected_target_file_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan attempted to reference its target as its own basis",
        ));
    }

    if !allowed_basis_file_ids.contains(&basis_file_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session CDC plan referenced unpublished basis file ID {basis_file_id}",),
        ));
    }

    let basis_entry = manifest.get(basis_file_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session CDC plan referenced unknown basis file ID {basis_file_id}"),
        )
    })?;

    if !matches!(basis_entry.class, FileClass::Medium | FileClass::Large) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan referenced an ineligible basis file",
        ));
    }

    let expected_basis_bytes = read_u64(reader)?;

    if expected_basis_bytes != basis_entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC plan declared {expected_basis_bytes} basis bytes, but manifest file {basis_file_id} contains {}",
                basis_entry.file_size,
            ),
        ));
    }

    let plan_wire = read_payload(
        reader,
        MAXIMUM_PLAN_WIRE_BYTES,
        "session CDC reconstruction plan",
    )?;

    let plan_wire_bytes = u64::try_from(plan_wire.len())
        .map_err(|_| io::Error::other("session CDC plan length cannot be represented"))?;

    let plan = ReconstructionPlan::decode_wire(&plan_wire)?;

    if plan.target_bytes() != target_entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC plan targets {} bytes, but manifest file {expected_target_file_id} requires {}",
                plan.target_bytes(),
                target_entry.file_size,
            ),
        ));
    }

    if plan.referenced_bytes() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan does not reference any basis data",
        ));
    }

    if plan.literal_bytes() > MAXIMUM_LITERAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan exceeds the literal staging limit",
        ));
    }

    let total_wire_bytes = SESSION_CDC_PLAN_FIXED_WIRE_BYTES
        .checked_add(plan_wire_bytes)
        .ok_or_else(|| io::Error::other("session CDC wire size overflowed"))?;

    if total_wire_bytes >= target_entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC plan is not smaller than an ordinary whole-file transfer",
        ));
    }

    let basis_path = destination_root.join(&basis_entry.relative_path);

    validate_committed_basis(&basis_path, expected_basis_bytes, basis_file_id)?;

    let target_path = destination_root.join(&target_entry.relative_path);

    reconstruct_verified(&basis_path, &target_path, expected_basis_bytes, &plan)?;

    Ok(completed_decision(
        target_entry.file_size,
        &plan,
        plan_wire_bytes,
    ))
}

fn completed_decision(
    logical_bytes: u64,
    plan: &ReconstructionPlan,
    plan_wire_bytes: u64,
) -> CdcLaneDecision {
    CdcLaneDecision {
        completed: true,

        stats: CdcLaneStats {
            offered_files: 1,
            completed_files: 1,
            logical_bytes,
            reused_bytes: plan.referenced_bytes(),
            literal_bytes: plan.literal_bytes(),
            plan_wire_bytes,
            ..CdcLaneStats::default()
        },
    }
}

fn unavailable_decision() -> CdcLaneDecision {
    CdcLaneDecision {
        completed: false,
        stats: CdcLaneStats::default(),
    }
}

fn validate_source_file(path: &Path, entry: &ManifestEntry, description: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;

    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} is not a regular file: {}", path.display()),
        ));
    }

    if metadata.len() != entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} size changed after scanning: expected {}, found {}: {}",
                entry.file_size,
                metadata.len(),
                path.display(),
            ),
        ));
    }

    let last_write_time = metadata.last_write_time();

    if last_write_time != entry.last_write_time {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} last-write time changed after scanning: expected {}, found {last_write_time}: {}",
                entry.last_write_time,
                path.display(),
            ),
        ));
    }

    Ok(())
}

fn validate_committed_basis(
    path: &Path,
    expected_bytes: u64,
    basis_file_id: usize,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "session CDC committed basis file {basis_file_id} is unavailable: {}: {error}",
                path.display(),
            ),
        )
    })?;

    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC committed basis file {basis_file_id} is not a regular file: {}",
                path.display(),
            ),
        ));
    }

    if metadata.len() != expected_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session CDC committed basis file {basis_file_id} contains {} bytes, expected {expected_bytes}: {}",
                metadata.len(),
                path.display(),
            ),
        ));
    }

    Ok(())
}

fn likely_similar(
    target_path: &Path,
    target_bytes: u64,
    basis_path: &Path,
    basis_bytes: u64,
) -> io::Result<bool> {
    if target_bytes == 0 || basis_bytes == 0 {
        return Ok(false);
    }

    let smaller = target_bytes.min(basis_bytes);
    let larger = target_bytes.max(basis_bytes);

    let allowed_difference = (smaller / 4).max(SIMILARITY_SAMPLE_BYTES);

    if larger - smaller > allowed_difference {
        return Ok(false);
    }

    let sample_bytes = smaller.min(SIMILARITY_SAMPLE_BYTES);

    let target_prefix = read_sample(target_path, 0, sample_bytes)?;

    let basis_prefix = read_sample(basis_path, 0, sample_bytes)?;

    if target_prefix == basis_prefix {
        return Ok(true);
    }

    let target_suffix = read_sample(target_path, target_bytes - sample_bytes, sample_bytes)?;

    let basis_suffix = read_sample(basis_path, basis_bytes - sample_bytes, sample_bytes)?;

    Ok(target_suffix == basis_suffix)
}

fn read_sample(path: &Path, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    let length = usize::try_from(length)
        .map_err(|_| io::Error::other("session CDC sample length cannot be represented"))?;

    let mut file = File::open(path)?;

    file.seek(SeekFrom::Start(offset))?;

    let mut sample = vec![0_u8; length];

    file.read_exact(&mut sample)?;

    Ok(sample)
}

fn read_payload(
    reader: &mut impl Read,
    maximum_bytes: usize,
    description: &str,
) -> io::Result<Vec<u8>> {
    let wire_bytes = usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} length cannot be represented"),
        )
    })?;

    if wire_bytes == 0 || wire_bytes > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} contains {wire_bytes} bytes; supported range is 1..={maximum_bytes}",
            ),
        ));
    }

    let mut payload = Vec::new();

    payload.try_reserve_exact(wire_bytes).map_err(|error| {
        io::Error::other(format!("failed to reserve {description} storage: {error}",))
    })?;

    payload.resize(wire_bytes, 0);

    reader.read_exact(&mut payload)?;

    Ok(payload)
}

fn write_file_id(writer: &mut impl Write, file_id: usize) -> io::Result<()> {
    write_u64(
        writer,
        u64::try_from(file_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session CDC file ID cannot be represented",
            )
        })?,
    )
}

fn read_file_id(reader: &mut impl Read) -> io::Result<usize> {
    usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "session CDC file ID cannot be represented",
        )
    })
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];

    reader.read_exact(&mut value)?;

    Ok(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::{MESSAGE_SESSION_CDC_PLAN, receiver_apply_plan, sender_try_plan};
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::env;
    use std::fs;
    use std::io::Cursor;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn session_lane_reconstructs_related_medium_file_from_committed_basis() {
        let root = temporary_root("reconstruct");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let basis = deterministic_bytes(2 * 1024 * 1024, 0x1234_5678_90AB_CDEF);

        let insertion = deterministic_bytes(4097, 0xCAFE_BABE_DEAD_BEEF);

        let insertion_offset = 1024 * 1024 + 123;

        let mut target = Vec::with_capacity(basis.len() + insertion.len());

        target.extend_from_slice(&basis[..insertion_offset]);

        target.extend_from_slice(&insertion);

        target.extend_from_slice(&basis[insertion_offset..]);

        fs::write(source_root.join("basis.bin"), &basis).unwrap();

        fs::write(source_root.join("target.bin"), &target).unwrap();

        fs::write(destination_root.join("basis.bin"), &basis).unwrap();

        let manifest = vec![
            entry(&source_root, "basis.bin"),
            entry(&source_root, "target.bin"),
        ];

        let mut wire = Vec::new();

        let sender_decision = sender_try_plan(&mut wire, &source_root, &manifest, 1, &[0]).unwrap();

        assert!(sender_decision.completed);

        assert_eq!(wire.first().copied(), Some(MESSAGE_SESSION_CDC_PLAN),);

        assert_eq!(sender_decision.stats.index_wire_bytes, 0);

        assert!(sender_decision.stats.reused_bytes > sender_decision.stats.literal_bytes,);

        let mut reader = Cursor::new(&wire[1..]);

        let receiver_decision =
            receiver_apply_plan(&mut reader, &destination_root, &manifest, 1, &[0]).unwrap();

        assert_eq!(receiver_decision, sender_decision);

        assert_eq!(
            fs::read(destination_root.join("target.bin")).unwrap(),
            target,
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_medium_file_emits_no_session_plan() {
        let root = temporary_root("unrelated");

        let source_root = root.join("source");

        fs::create_dir_all(&source_root).unwrap();

        fs::write(
            source_root.join("basis.bin"),
            deterministic_bytes(2 * 1024 * 1024, 0x1111_2222_3333_4444),
        )
        .unwrap();

        fs::write(
            source_root.join("target.bin"),
            deterministic_bytes(2 * 1024 * 1024, 0xAAAA_BBBB_CCCC_DDDD),
        )
        .unwrap();

        let manifest = vec![
            entry(&source_root, "basis.bin"),
            entry(&source_root, "target.bin"),
        ];

        let mut wire = Vec::new();

        let decision = sender_try_plan(&mut wire, &source_root, &manifest, 1, &[0]).unwrap();

        assert!(!decision.completed);

        assert_eq!(decision.stats, Default::default());

        assert!(wire.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receiver_rejects_unpublished_session_basis() {
        let root = temporary_root("unpublished");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let basis = deterministic_bytes(2 * 1024 * 1024, 0x1020_3040_5060_7080);

        let insertion = deterministic_bytes(4097, 0x8877_6655_4433_2211);

        let insertion_offset = 1024 * 1024 + 321;

        let mut target = Vec::with_capacity(basis.len() + insertion.len());

        target.extend_from_slice(&basis[..insertion_offset]);

        target.extend_from_slice(&insertion);

        target.extend_from_slice(&basis[insertion_offset..]);

        fs::write(source_root.join("basis.bin"), &basis).unwrap();

        fs::write(source_root.join("target.bin"), &target).unwrap();

        fs::write(destination_root.join("basis.bin"), &basis).unwrap();

        let manifest = vec![
            entry(&source_root, "basis.bin"),
            entry(&source_root, "target.bin"),
        ];

        let mut wire = Vec::new();

        let decision = sender_try_plan(&mut wire, &source_root, &manifest, 1, &[0]).unwrap();

        assert!(decision.completed);

        let mut reader = Cursor::new(&wire[1..]);

        let error =
            receiver_apply_plan(&mut reader, &destination_root, &manifest, 1, &[]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("unpublished basis file ID 0"),);

        assert!(!destination_root.join("target.bin").try_exists().unwrap(),);

        fs::remove_dir_all(root).unwrap();
    }

    fn entry(root: &Path, relative_path: &str) -> ManifestEntry {
        let relative_path = PathBuf::from(relative_path);

        let metadata = fs::metadata(root.join(&relative_path)).unwrap();

        ManifestEntry {
            relative_path,
            file_size: metadata.len(),
            last_write_time: metadata.last_write_time(),
            file_attributes: metadata.file_attributes(),
            class: FileClass::Medium,
        }
    }

    fn temporary_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-session-cdc-{label}-{}-{unique}",
            process::id(),
        ))
    }

    fn deterministic_bytes(length: usize, mut state: u64) -> Vec<u8> {
        let mut bytes = vec![0_u8; length];

        for byte in &mut bytes {
            state ^= state << 13;

            state ^= state >> 7;

            state ^= state << 17;

            *byte = (state >> 24) as u8;
        }

        bytes
    }
}
