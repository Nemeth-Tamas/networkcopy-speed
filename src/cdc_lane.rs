use crate::cdc_basis_index::BasisFileIndex;
use crate::cdc_reconstruction_bench::{
    ReconstructionPlan, is_literal_limit_exceeded, reconstruct_verified,
};
use crate::content_defined_dedup_bench;
use crate::manifest_scan::{FileClass, ManifestEntry};
use std::fs;
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const MESSAGE_CDC_UNAVAILABLE: u8 = 0x38;
const MESSAGE_CDC_INDEX: u8 = 0x39;
const MESSAGE_CDC_FALLBACK: u8 = 0x3A;
const MESSAGE_CDC_PLAN: u8 = 0x3B;

const MINIMUM_FILE_BYTES: u64 = 1024 * 1024;

const MAXIMUM_LITERAL_BYTES: u64 = 16 * 1024 * 1024;

const MAXIMUM_INDEX_WIRE_BYTES: usize = 4 * 1024 * 1024;

const MAXIMUM_PLAN_WIRE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CdcLaneStats {
    pub(crate) offered_files: u64,
    pub(crate) completed_files: u64,
    pub(crate) fallback_files: u64,

    pub(crate) logical_bytes: u64,
    pub(crate) reused_bytes: u64,
    pub(crate) literal_bytes: u64,

    pub(crate) index_wire_bytes: u64,
    pub(crate) plan_wire_bytes: u64,
}

impl CdcLaneStats {
    pub(crate) fn merge(&mut self, other: Self) -> io::Result<()> {
        add(
            &mut self.offered_files,
            other.offered_files,
            "CDC offer count",
        )?;

        add(
            &mut self.completed_files,
            other.completed_files,
            "CDC completion count",
        )?;

        add(
            &mut self.fallback_files,
            other.fallback_files,
            "CDC fallback count",
        )?;

        add(
            &mut self.logical_bytes,
            other.logical_bytes,
            "CDC logical bytes",
        )?;

        add(
            &mut self.reused_bytes,
            other.reused_bytes,
            "CDC reused bytes",
        )?;

        add(
            &mut self.literal_bytes,
            other.literal_bytes,
            "CDC literal bytes",
        )?;

        add(
            &mut self.index_wire_bytes,
            other.index_wire_bytes,
            "CDC index wire bytes",
        )?;

        add(
            &mut self.plan_wire_bytes,
            other.plan_wire_bytes,
            "CDC plan wire bytes",
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CdcLaneDecision {
    pub(crate) completed: bool,
    pub(crate) stats: CdcLaneStats,
}

pub(crate) fn sender_negotiate(
    reader: &mut impl Read,
    writer: &mut impl Write,
    source_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
) -> io::Result<CdcLaneDecision> {
    let message = read_u8(reader)?;

    match message {
        MESSAGE_CDC_UNAVAILABLE => {
            validate_file_id(read_file_id(reader)?, file_id, "CDC unavailable offer")?;

            Ok(CdcLaneDecision {
                completed: false,
                stats: CdcLaneStats::default(),
            })
        }

        MESSAGE_CDC_INDEX => {
            validate_file_id(read_file_id(reader)?, file_id, "CDC index offer")?;

            let index_wire = read_payload(reader, MAXIMUM_INDEX_WIRE_BYTES, "CDC index")?;

            let index_wire_bytes = u64::try_from(index_wire.len())
                .map_err(|_| io::Error::other("CDC index length cannot be represented"))?;

            let mut stats = CdcLaneStats {
                offered_files: 1,
                index_wire_bytes,
                ..CdcLaneStats::default()
            };

            let index = BasisFileIndex::decode_wire(&index_wire)?;

            if index.average_kib() != content_defined_dedup_bench::DEFAULT_AVERAGE_KIB {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "receiver offered CDC average {} KiB; \
                         this protocol slice requires {} KiB",
                        index.average_kib(),
                        content_defined_dedup_bench::DEFAULT_AVERAGE_KIB,
                    ),
                ));
            }

            let source_path = source_root.join(&entry.relative_path);

            validate_source(&source_path, entry)?;

            let plan = match ReconstructionPlan::build_bounded(
                &source_path,
                &index,
                MAXIMUM_LITERAL_BYTES,
            ) {
                Ok(plan) => plan,

                Err(error) if is_literal_limit_exceeded(&error) => {
                    return send_fallback(writer, file_id, stats);
                }

                Err(error) => return Err(error),
            };

            validate_source(&source_path, entry)?;

            let plan_wire = plan.encode_wire()?;

            if plan_wire.len() > MAXIMUM_PLAN_WIRE_BYTES {
                return send_fallback(writer, file_id, stats);
            }

            let plan_wire_bytes = u64::try_from(plan_wire.len())
                .map_err(|_| io::Error::other("CDC plan length cannot be represented"))?;

            let total_cdc_wire = index_wire_bytes
                .checked_add(plan_wire_bytes)
                .ok_or_else(|| io::Error::other("CDC combined wire length overflowed"))?;

            if total_cdc_wire >= entry.file_size {
                return send_fallback(writer, file_id, stats);
            }

            write_u8(writer, MESSAGE_CDC_PLAN)?;

            write_file_id(writer, file_id)?;

            write_u64(writer, plan_wire_bytes)?;

            writer.write_all(&plan_wire)?;

            writer.flush()?;

            stats.completed_files = 1;
            stats.logical_bytes = entry.file_size;

            stats.reused_bytes = plan.referenced_bytes();

            stats.literal_bytes = plan.literal_bytes();

            stats.plan_wire_bytes = plan_wire_bytes;

            Ok(CdcLaneDecision {
                completed: true,
                stats,
            })
        }

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "received unknown CDC offer message \
                 0x{unknown:02X}",
            ),
        )),
    }
}

pub(crate) fn receiver_negotiate(
    reader: &mut impl Read,
    writer: &mut impl Write,
    destination_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
    enabled: bool,
) -> io::Result<CdcLaneDecision> {
    if !enabled || entry.class != FileClass::Medium || entry.file_size < MINIMUM_FILE_BYTES {
        send_unavailable(writer, file_id)?;

        return Ok(CdcLaneDecision {
            completed: false,
            stats: CdcLaneStats::default(),
        });
    }

    let basis_path = destination_root.join(&entry.relative_path);

    if !eligible_basis(&basis_path)? {
        send_unavailable(writer, file_id)?;

        return Ok(CdcLaneDecision {
            completed: false,
            stats: CdcLaneStats::default(),
        });
    }

    let index = match BasisFileIndex::build(
        &basis_path,
        content_defined_dedup_bench::DEFAULT_AVERAGE_KIB,
    ) {
        Ok(index) => index,

        Err(_) => {
            send_unavailable(writer, file_id)?;

            return Ok(CdcLaneDecision {
                completed: false,
                stats: CdcLaneStats::default(),
            });
        }
    };

    let expected_basis_bytes = index.file_bytes();

    let index_wire = index.encode_wire()?;

    if index_wire.len() > MAXIMUM_INDEX_WIRE_BYTES {
        send_unavailable(writer, file_id)?;

        return Ok(CdcLaneDecision {
            completed: false,
            stats: CdcLaneStats::default(),
        });
    }

    let index_wire_bytes = u64::try_from(index_wire.len())
        .map_err(|_| io::Error::other("CDC index length cannot be represented"))?;

    write_u8(writer, MESSAGE_CDC_INDEX)?;

    write_file_id(writer, file_id)?;

    write_u64(writer, index_wire_bytes)?;

    writer.write_all(&index_wire)?;

    writer.flush()?;

    drop(index);
    drop(index_wire);

    let response = read_u8(reader)?;

    validate_file_id(read_file_id(reader)?, file_id, "CDC sender response")?;

    let mut stats = CdcLaneStats {
        offered_files: 1,
        index_wire_bytes,
        ..CdcLaneStats::default()
    };

    match response {
        MESSAGE_CDC_FALLBACK => {
            stats.fallback_files = 1;

            Ok(CdcLaneDecision {
                completed: false,
                stats,
            })
        }

        MESSAGE_CDC_PLAN => {
            let plan_wire =
                read_payload(reader, MAXIMUM_PLAN_WIRE_BYTES, "CDC reconstruction plan")?;

            let plan_wire_bytes = u64::try_from(plan_wire.len())
                .map_err(|_| io::Error::other("CDC plan length cannot be represented"))?;

            let plan = ReconstructionPlan::decode_wire(&plan_wire)?;

            if plan.target_bytes() != entry.file_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "CDC plan targets {} bytes, \
                         but manifest file {file_id} \
                         requires {}",
                        plan.target_bytes(),
                        entry.file_size,
                    ),
                ));
            }

            if plan.literal_bytes() > MAXIMUM_LITERAL_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "CDC plan exceeds the negotiated \
                     literal staging limit",
                ));
            }

            let total_cdc_wire = index_wire_bytes
                .checked_add(plan_wire_bytes)
                .ok_or_else(|| io::Error::other("CDC combined wire length overflowed"))?;

            if total_cdc_wire >= entry.file_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sender selected CDC even though \
                     ordinary transfer would be smaller",
                ));
            }

            reconstruct_verified(&basis_path, &basis_path, expected_basis_bytes, &plan)?;

            stats.completed_files = 1;
            stats.logical_bytes = entry.file_size;

            stats.reused_bytes = plan.referenced_bytes();

            stats.literal_bytes = plan.literal_bytes();

            stats.plan_wire_bytes = plan_wire_bytes;

            Ok(CdcLaneDecision {
                completed: true,
                stats,
            })
        }

        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "received unknown CDC sender response \
                 0x{unknown:02X}",
            ),
        )),
    }
}

fn send_unavailable(writer: &mut impl Write, file_id: usize) -> io::Result<()> {
    write_u8(writer, MESSAGE_CDC_UNAVAILABLE)?;

    write_file_id(writer, file_id)?;

    writer.flush()
}

fn send_fallback(
    writer: &mut impl Write,
    file_id: usize,
    mut stats: CdcLaneStats,
) -> io::Result<CdcLaneDecision> {
    write_u8(writer, MESSAGE_CDC_FALLBACK)?;

    write_file_id(writer, file_id)?;

    writer.flush()?;

    stats.fallback_files = 1;

    Ok(CdcLaneDecision {
        completed: false,
        stats,
    })
}

fn eligible_basis(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,

        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(false);
        }

        Err(error) => return Err(error),
    };

    Ok(metadata.is_file()
        && metadata.len() > 0
        && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

fn validate_source(path: &Path, entry: &ManifestEntry) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;

    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CDC source is not a regular file: {}", path.display(),),
        ));
    }

    if metadata.len() != entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "CDC source size changed after scanning: \
                 expected {}, found {}: {}",
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
                "CDC source last-write time changed after scanning: \
                 expected {}, found {last_write_time}: {}",
                entry.last_write_time,
                path.display(),
            ),
        ));
    }

    Ok(())
}

fn read_payload(
    reader: &mut impl Read,
    maximum_bytes: usize,
    description: &str,
) -> io::Result<Vec<u8>> {
    let wire_bytes = usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} length cannot be represented",),
        )
    })?;

    if wire_bytes == 0 || wire_bytes > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} contains {wire_bytes} bytes; \
                 supported range is 1..={maximum_bytes}",
            ),
        ));
    }

    let mut payload = Vec::new();

    payload.try_reserve_exact(wire_bytes).map_err(|error| {
        io::Error::other(format!(
            "failed to reserve {description} storage: \
                 {error}",
        ))
    })?;

    payload.resize(wire_bytes, 0);

    reader.read_exact(&mut payload)?;

    Ok(payload)
}

fn validate_file_id(announced: usize, expected: usize, description: &str) -> io::Result<()> {
    if announced == expected {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{description} referenced file {announced}, \
             expected {expected}",
        ),
    ))
}

fn write_file_id(writer: &mut impl Write, file_id: usize) -> io::Result<()> {
    write_u64(
        writer,
        u64::try_from(file_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CDC file ID cannot be represented",
            )
        })?,
    )
}

fn read_file_id(reader: &mut impl Read) -> io::Result<usize> {
    usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "CDC file ID cannot be represented",
        )
    })
}

fn add(target: &mut u64, value: u64, description: &str) -> io::Result<()> {
    *target = target
        .checked_add(value)
        .ok_or_else(|| io::Error::other(format!("{description} overflowed",)))?;

    Ok(())
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut value = [0_u8; 1];

    reader.read_exact(&mut value)?;

    Ok(value[0])
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];

    reader.read_exact(&mut value)?;

    Ok(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::{receiver_negotiate, sender_negotiate};
    use crate::manifest_scan::{FileClass, ManifestEntry};
    use std::env;
    use std::fs;
    use std::io::{BufReader, BufWriter};
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn lane_reconstructs_changed_medium_file() {
        let root = temporary_root("reconstruct");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let basis = deterministic_bytes(2 * 1024 * 1024, 0x1234_5678_90AB_CDEF);

        let insertion = deterministic_bytes(4097, 0xCAFE_BABE_DEAD_BEEF);

        let insertion_offset = 1024 * 1024 + 123;

        let mut candidate = Vec::with_capacity(basis.len() + insertion.len());

        candidate.extend_from_slice(&basis[..insertion_offset]);

        candidate.extend_from_slice(&insertion);

        candidate.extend_from_slice(&basis[insertion_offset..]);

        let relative_path = PathBuf::from("file.bin");

        let source_path = source_root.join(&relative_path);

        let destination_path = destination_root.join(&relative_path);

        fs::write(&source_path, &candidate).unwrap();

        fs::write(&destination_path, &basis).unwrap();

        let sender_entry = entry(&source_path, relative_path.clone());

        let receiver_entry = entry(&source_path, relative_path);

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let receiver_root = destination_root.clone();

        let receiver = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();

            let reader_stream = stream.try_clone().unwrap();

            let mut reader = BufReader::new(reader_stream);

            let mut writer = BufWriter::new(stream);

            receiver_negotiate(
                &mut reader,
                &mut writer,
                &receiver_root,
                0,
                &receiver_entry,
                true,
            )
            .unwrap()
        });

        let stream = TcpStream::connect(address).unwrap();

        let reader_stream = stream.try_clone().unwrap();

        let mut reader = BufReader::new(reader_stream);

        let mut writer = BufWriter::new(stream);

        let sender_decision =
            sender_negotiate(&mut reader, &mut writer, &source_root, 0, &sender_entry).unwrap();

        let receiver_decision = receiver.join().unwrap();

        assert!(sender_decision.completed);
        assert!(receiver_decision.completed);

        assert_eq!(sender_decision.stats, receiver_decision.stats,);

        assert_eq!(fs::read(destination_path).unwrap(), candidate,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_basis_uses_ordinary_fallback() {
        let root = temporary_root("missing");

        let source_root = root.join("source");

        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();

        fs::create_dir_all(&destination_root).unwrap();

        let relative_path = PathBuf::from("file.bin");

        let source_path = source_root.join(&relative_path);

        fs::write(
            &source_path,
            deterministic_bytes(2 * 1024 * 1024, 0xA5A5_5A5A_1122_3344),
        )
        .unwrap();

        let sender_entry = entry(&source_path, relative_path.clone());

        let receiver_entry = entry(&source_path, relative_path);

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let receiver = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();

            let reader_stream = stream.try_clone().unwrap();

            let mut reader = BufReader::new(reader_stream);

            let mut writer = BufWriter::new(stream);

            receiver_negotiate(
                &mut reader,
                &mut writer,
                &destination_root,
                0,
                &receiver_entry,
                true,
            )
            .unwrap()
        });

        let stream = TcpStream::connect(address).unwrap();

        let reader_stream = stream.try_clone().unwrap();

        let mut reader = BufReader::new(reader_stream);

        let mut writer = BufWriter::new(stream);

        let sender_decision =
            sender_negotiate(&mut reader, &mut writer, &source_root, 0, &sender_entry).unwrap();

        let receiver_decision = receiver.join().unwrap();

        assert!(!sender_decision.completed,);

        assert!(!receiver_decision.completed,);

        fs::remove_dir_all(root).unwrap();
    }

    fn entry(source_path: &Path, relative_path: PathBuf) -> ManifestEntry {
        let metadata = fs::metadata(source_path).unwrap();

        ManifestEntry {
            relative_path,
            file_size: metadata.len(),
            last_write_time: metadata.last_write_time(),
            file_attributes: metadata.file_attributes(),
            class: FileClass::Medium,
        }
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
            "networkcopy-cdc-lane-{name}-{}-{unique}",
            process::id(),
        ))
    }
}
