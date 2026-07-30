use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

const JOURNAL_MAGIC: [u8; 4] = *b"NCR1";
const JOURNAL_VERSION_V1: u16 = 1;
const JOURNAL_VERSION: u16 = 2;

const MAX_COMPLETED_STRIPES: u32 = 10_000_000;
const MAX_COMPLETED_FILES: u32 = 1_000_000;

pub(crate) const JOURNAL_FILE_NAME: &str = ".networkcopy-resume.bin";

const TEMPORARY_JOURNAL_FILE_NAME: &str = ".networkcopy-resume.tmp";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ResumeStripe {
    pub(crate) file_id: u64,
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

impl ResumeStripe {
    pub(crate) fn new(file_id: usize, offset: u64, length: u64) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resume stripe length must not be zero",
            ));
        }

        Ok(Self {
            file_id: u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resume file ID cannot be represented",
                )
            })?,

            offset,
            length,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ResumeJournal {
    manifest_fingerprint: u64,
    data_stream_count: u32,
    completed_stripes: BTreeSet<ResumeStripe>,
    completed_file_ids: BTreeSet<usize>,
}

impl ResumeJournal {
    pub(crate) fn new(manifest_fingerprint: u64, data_stream_count: usize) -> io::Result<Self> {
        if data_stream_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resume journal requires at least one data stream",
            ));
        }

        Ok(Self {
            manifest_fingerprint,

            data_stream_count: u32::try_from(data_stream_count).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resume stream count cannot be represented",
                )
            })?,

            completed_stripes: BTreeSet::new(),
            completed_file_ids: BTreeSet::new(),
        })
    }

    pub(crate) fn load_existing(
        destination_root: &Path,
        expected_manifest_fingerprint: u64,
        expected_data_stream_count: usize,
    ) -> io::Result<Self> {
        let expected_data_stream_count =
            u32::try_from(expected_data_stream_count).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected resume stream count cannot be represented",
                )
            })?;

        let journal_path = destination_root.join(JOURNAL_FILE_NAME);

        let journal = Self::read_path(&journal_path)?;

        if journal.manifest_fingerprint != expected_manifest_fingerprint {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resume journal manifest fingerprint is \
                     0x{:016X}, expected 0x{expected_manifest_fingerprint:016X}",
                    journal.manifest_fingerprint
                ),
            ));
        }

        if journal.data_stream_count != expected_data_stream_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resume journal stores {} data streams, expected \
                     {expected_data_stream_count}",
                    journal.data_stream_count
                ),
            ));
        }

        Ok(journal)
    }

    pub(crate) fn mark_completed(&mut self, stripe: ResumeStripe) -> bool {
        self.completed_stripes.insert(stripe)
    }

    pub(crate) fn completed_stripes(&self) -> impl Iterator<Item = ResumeStripe> + '_ {
        self.completed_stripes.iter().copied()
    }

    pub(crate) fn mark_file_completed(&mut self, file_id: usize) -> bool {
        self.completed_file_ids.insert(file_id)
    }

    pub(crate) fn completed_file_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.completed_file_ids.iter().copied()
    }

    pub(crate) fn remove_completed_files(&mut self, file_ids: &BTreeSet<usize>) -> bool {
        let previous_stripe_count = self.completed_stripes.len();
        let previous_file_count = self.completed_file_ids.len();

        self.completed_stripes.retain(|stripe| {
            usize::try_from(stripe.file_id).map_or(true, |file_id| !file_ids.contains(&file_id))
        });

        self.completed_file_ids
            .retain(|file_id| !file_ids.contains(file_id));

        self.completed_stripes.len() != previous_stripe_count
            || self.completed_file_ids.len() != previous_file_count
    }

    pub(crate) fn save_atomic(&self, destination_root: &Path) -> io::Result<()> {
        let destination_root = destination_root.canonicalize()?;

        let journal_path = destination_root.join(JOURNAL_FILE_NAME);

        let temporary_path = destination_root.join(TEMPORARY_JOURNAL_FILE_NAME);

        match fs::remove_file(&temporary_path) {
            Ok(()) => {}

            Err(error) if error.kind() == io::ErrorKind::NotFound => {}

            Err(error) => return Err(error),
        }

        let save_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)?;

            self.write_to(&mut file)?;

            file.flush()?;
            file.sync_all()?;
            drop(file);

            let reloaded = Self::read_path(&temporary_path)?;

            if reloaded != *self {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resume journal verification failed before publication",
                ));
            }

            replace_file(&temporary_path, &journal_path)
        })();

        if save_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }

        save_result
    }

    pub(crate) fn remove(destination_root: &Path) -> io::Result<()> {
        let journal_path = destination_root.join(JOURNAL_FILE_NAME);

        match fs::remove_file(journal_path) {
            Ok(()) => Ok(()),

            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),

            Err(error) => Err(error),
        }
    }

    fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(&JOURNAL_MAGIC)?;
        write_u16(writer, JOURNAL_VERSION)?;
        write_u16(writer, 0)?;

        write_u64(writer, self.manifest_fingerprint)?;

        write_u32(writer, self.data_stream_count)?;

        write_u32(
            writer,
            u32::try_from(self.completed_stripes.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resume journal contains too many completed stripes",
                )
            })?,
        )?;

        write_u32(
            writer,
            u32::try_from(self.completed_file_ids.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resume journal contains too many completed files",
                )
            })?,
        )?;

        for stripe in &self.completed_stripes {
            write_u64(writer, stripe.file_id)?;
            write_u64(writer, stripe.offset)?;
            write_u64(writer, stripe.length)?;
        }

        for &file_id in &self.completed_file_ids {
            write_u64(
                writer,
                u64::try_from(file_id).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "completed resume file ID cannot be represented",
                    )
                })?,
            )?;
        }

        Ok(())
    }

    fn read_path(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;

        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic)?;

        if magic != JOURNAL_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resume journal used an invalid magic value",
            ));
        }

        let version = read_u16(&mut file)?;

        if !matches!(version, JOURNAL_VERSION_V1 | JOURNAL_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported resume journal version {version}"),
            ));
        }

        let reserved = read_u16(&mut file)?;

        if reserved != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resume journal reserved field was not zero",
            ));
        }

        let manifest_fingerprint = read_u64(&mut file)?;

        let data_stream_count = read_u32(&mut file)?;

        if data_stream_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resume journal stored zero data streams",
            ));
        }

        let stripe_count = read_u32(&mut file)?;

        let completed_file_count = if version >= JOURNAL_VERSION {
            read_u32(&mut file)?
        } else {
            0
        };

        if stripe_count > MAX_COMPLETED_STRIPES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resume journal contains {stripe_count} stripes, exceeding the supported limit"
                ),
            ));
        }

        if completed_file_count > MAX_COMPLETED_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resume journal contains {completed_file_count} completed files, exceeding the supported limit"
                ),
            ));
        }

        let mut completed_stripes = BTreeSet::new();

        for _ in 0..stripe_count {
            let stripe = ResumeStripe {
                file_id: read_u64(&mut file)?,

                offset: read_u64(&mut file)?,

                length: read_u64(&mut file)?,
            };

            if stripe.length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resume journal contains a zero-length stripe",
                ));
            }

            if !completed_stripes.insert(stripe) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resume journal contains a duplicate stripe",
                ));
            }
        }

        let mut completed_file_ids = BTreeSet::new();

        for _ in 0..completed_file_count {
            let encoded_file_id = read_u64(&mut file)?;

            let file_id = usize::try_from(encoded_file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "completed resume file ID cannot be represented",
                )
            })?;

            if !completed_file_ids.insert(file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resume journal contains a duplicate completed file ID",
                ));
            }
        }

        let mut trailing = [0_u8; 1];

        if file.read(&mut trailing)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resume journal contains trailing data",
            ));
        }

        Ok(Self {
            manifest_fingerprint,
            data_stream_count,
            completed_stripes,
            completed_file_ids,
        })
    }
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    let source = wide_null(source);
    let destination = wide_null(destination);

    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn write_u16(writer: &mut impl Write, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut value = [0_u8; 2];
    reader.read_exact(&mut value)?;
    Ok(u16::from_be_bytes(value))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0_u8; 4];
    reader.read_exact(&mut value)?;
    Ok(u32::from_be_bytes(value))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::{
        JOURNAL_FILE_NAME, JOURNAL_MAGIC, JOURNAL_VERSION_V1, ResumeJournal, ResumeStripe,
        TEMPORARY_JOURNAL_FILE_NAME, write_u16, write_u32, write_u64,
    };
    use std::collections::BTreeSet;
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn journal_round_trips_and_replaces_atomically() {
        let root = temporary_root("round-trip");

        fs::create_dir_all(&root).unwrap();

        let mut journal = ResumeJournal::new(0x1234_5678_9ABC_DEF0, 4).unwrap();

        journal.mark_completed(ResumeStripe::new(7, 0, 1024).unwrap());
        journal.mark_file_completed(3);

        journal.save_atomic(&root).unwrap();

        journal.mark_completed(ResumeStripe::new(7, 1024, 2048).unwrap());
        journal.mark_file_completed(9);

        journal.save_atomic(&root).unwrap();

        let loaded = ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF0, 4).unwrap();

        assert_eq!(loaded, journal);

        assert_eq!(loaded.completed_file_ids().collect::<Vec<_>>(), vec![3, 9],);

        assert!(!root.join(TEMPORARY_JOURNAL_FILE_NAME,).exists());

        ResumeJournal::remove(&root).unwrap();

        assert!(!root.join(JOURNAL_FILE_NAME).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_reads_version_one_without_completed_files() {
        let root = temporary_root("version-one");

        fs::create_dir_all(&root).unwrap();

        let path = root.join(JOURNAL_FILE_NAME);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();

        file.write_all(&JOURNAL_MAGIC).unwrap();
        write_u16(&mut file, JOURNAL_VERSION_V1).unwrap();
        write_u16(&mut file, 0).unwrap();
        write_u64(&mut file, 0x1234_5678_9ABC_DEF0).unwrap();
        write_u32(&mut file, 4).unwrap();
        write_u32(&mut file, 1).unwrap();

        write_u64(&mut file, 7).unwrap();
        write_u64(&mut file, 1024).unwrap();
        write_u64(&mut file, 2048).unwrap();

        file.flush().unwrap();
        file.sync_all().unwrap();
        drop(file);

        let loaded = ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF0, 4).unwrap();

        assert_eq!(
            loaded.completed_stripes().collect::<Vec<_>>(),
            vec![ResumeStripe::new(7, 1024, 2048).unwrap()],
        );

        assert_eq!(loaded.completed_file_ids().count(), 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_rejects_mismatched_identity() {
        let root = temporary_root("identity");

        fs::create_dir_all(&root).unwrap();

        let journal = ResumeJournal::new(0x1234_5678_9ABC_DEF0, 4).unwrap();

        journal.save_atomic(&root).unwrap();

        let fingerprint_error =
            ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF1, 4).unwrap_err();

        assert_eq!(fingerprint_error.kind(), io::ErrorKind::InvalidData);

        assert!(
            fingerprint_error
                .to_string()
                .contains("manifest fingerprint",)
        );

        let stream_error =
            ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF0, 3).unwrap_err();

        assert_eq!(stream_error.kind(), io::ErrorKind::InvalidData);

        assert!(stream_error.to_string().contains("data streams"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_completed_files_clears_stripes_and_file_ids() {
        let mut journal = ResumeJournal::new(42, 2).unwrap();

        journal.mark_completed(ResumeStripe::new(3, 0, 1024).unwrap());
        journal.mark_completed(ResumeStripe::new(4, 0, 2048).unwrap());

        journal.mark_file_completed(3);
        journal.mark_file_completed(4);

        let reset_file_ids = BTreeSet::from([3]);

        assert!(journal.remove_completed_files(&reset_file_ids));

        assert_eq!(
            journal.completed_stripes().collect::<Vec<_>>(),
            vec![ResumeStripe::new(4, 0, 2048).unwrap()],
        );

        assert_eq!(journal.completed_file_ids().collect::<Vec<_>>(), vec![4],);
    }

    #[test]
    fn journal_rejects_trailing_corruption() {
        let root = temporary_root("corrupt");

        fs::create_dir_all(&root).unwrap();

        let mut journal = ResumeJournal::new(42, 2).unwrap();

        journal.mark_completed(ResumeStripe::new(3, 4096, 4096).unwrap());

        journal.save_atomic(&root).unwrap();

        let path = root.join(JOURNAL_FILE_NAME);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();

        file.write_all(&[0xA5]).unwrap();
        file.sync_all().unwrap();

        assert!(ResumeJournal::read_path(&path,).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!(
            "networkcopy-resume-{label}-{}-{unique}",
            process::id()
        ))
    }
}
