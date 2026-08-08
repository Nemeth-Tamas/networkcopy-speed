use crate::copy_bench::{binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes};
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, Prefix};
use std::ptr::{self, NonNull};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;

const PROPERTY_STANDARD_QUERY: u32 = 0;

const UNCACHED_ALIGNMENT_BYTES: usize = 4096;

const UNCACHED_READ_CHUNK_BYTES: usize = 4 * 1024 * 1024;

const STORAGE_READ_LANE_COUNTS: [usize; 4] = [1, 2, 4, 8];

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[repr(C)]
#[derive(Default)]
struct DeviceSeekPenaltyDescriptor {
    version: u32,
    size: u32,
    incurs_seek_penalty: u8,
    padding: [u8; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageMediaKind {
    SeekPenalty,
    NoSeekPenalty,
    Unknown,
}

impl fmt::Display for StorageMediaKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeekPenalty => {
                write!(formatter, "seek-penalty / rotational-sensitive",)
            }

            Self::NoSeekPenalty => {
                write!(formatter, "no seek penalty",)
            }

            Self::Unknown => {
                write!(formatter, "unknown")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageMediaReport {
    pub drive_letter: Option<char>,
    pub kind: StorageMediaKind,
}

impl StorageMediaReport {
    pub fn incurs_seek_penalty(self) -> Option<bool> {
        match self.kind {
            StorageMediaKind::SeekPenalty => Some(true),

            StorageMediaKind::NoSeekPenalty => Some(false),

            StorageMediaKind::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StorageReadLaneReport {
    pub bytes_read: u64,
    pub lane_count: usize,
    pub elapsed: Duration,
}

impl StorageReadLaneReport {
    pub fn print(&self) {
        println!("Uncached storage source-read benchmark complete",);

        println!("  Bytes read:    {}", format_bytes(self.bytes_read),);

        println!("  Read lanes:    {}", self.lane_count,);

        println!("  Chunk size:    4 MiB",);

        println!("  Read time:     {:.6} s", self.elapsed.as_secs_f64(),);

        println!(
            "  Throughput:    {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_read, self.elapsed,),
            binary_mebibytes_per_second(self.bytes_read, self.elapsed,),
        );
    }
}

struct AlignedReadBuffer {
    pointer: NonNull<u8>,
    length: usize,
    layout: Layout,
}

impl AlignedReadBuffer {
    fn new(length: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(length, UNCACHED_ALIGNMENT_BYTES).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid aligned read-buffer layout",
            )
        })?;

        // SAFETY:
        // layout is non-zero, valid, and uses a
        // power-of-two alignment. The allocation is
        // owned by this object until Drop.
        let pointer = unsafe { alloc_zeroed(layout) };

        let pointer = NonNull::new(pointer)
            .ok_or_else(|| io::Error::other("failed to allocate aligned read buffer"))?;

        Ok(Self {
            pointer,
            length,
            layout,
        })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY:
        // pointer owns length initialized bytes for
        // the lifetime of self.
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }
}

impl Drop for AlignedReadBuffer {
    fn drop(&mut self) {
        // SAFETY:
        // pointer was allocated with exactly this
        // layout and has not already been freed.
        unsafe {
            dealloc(self.pointer.as_ptr(), self.layout);
        }
    }
}

struct VolumeHandle(HANDLE);

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY:
        // The handle was returned successfully by
        // CreateFileW and is owned by this wrapper.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn inspect_path(path: &Path) -> io::Result<StorageMediaReport> {
    let Some(drive_letter) = drive_letter_from_path(path) else {
        return Ok(StorageMediaReport {
            drive_letter: None,
            kind: StorageMediaKind::Unknown,
        });
    };

    let incurs_seek_penalty = query_drive_seek_penalty(drive_letter)?;

    Ok(StorageMediaReport {
        drive_letter: Some(char::from(drive_letter)),

        kind: if incurs_seek_penalty {
            StorageMediaKind::SeekPenalty
        } else {
            StorageMediaKind::NoSeekPenalty
        },
    })
}

pub fn benchmark_uncached_read_lanes(
    source: &Path,
    lane_count: usize,
) -> io::Result<StorageReadLaneReport> {
    validate_storage_read_lane_count(lane_count)?;

    let metadata = std::fs::metadata(source)?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "storage benchmark source is not a regular file: {}",
                source.display(),
            ),
        ));
    }

    let file_bytes = metadata.len();

    if file_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage benchmark source is empty",
        ));
    }

    let required_alignment = u64::try_from(UNCACHED_ALIGNMENT_BYTES)
        .expect("uncached alignment fits in u64")
        .checked_mul(u64::try_from(lane_count).expect("lane count fits in u64"))
        .ok_or_else(|| io::Error::other("uncached lane alignment overflowed"))?;

    if file_bytes % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "uncached benchmark file size must be divisible by {required_alignment} bytes for {lane_count} lane(s)",
            ),
        ));
    }

    let started = Instant::now();

    let lane_results = thread::scope(|scope| -> io::Result<Vec<u64>> {
        let mut handles = Vec::with_capacity(lane_count);

        for lane_index in 0..lane_count {
            let (offset, length) = storage_read_lane_range(file_bytes, lane_index, lane_count)?;

            handles.push(
                thread::Builder::new()
                    .name(format!("networkcopy-storage-read-{lane_index}",))
                    .spawn_scoped(scope, move || read_uncached_range(source, offset, length))?,
            );
        }

        let mut results = Vec::with_capacity(lane_count);

        for handle in handles {
            results.push(
                handle
                    .join()
                    .map_err(|_| io::Error::other("storage read lane panicked"))??,
            );
        }

        Ok(results)
    })?;

    let elapsed = started.elapsed();

    let bytes_read = lane_results.into_iter().try_fold(0_u64, |total, bytes| {
        total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("storage read byte count overflowed"))
    })?;

    if bytes_read != file_bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("uncached benchmark read {bytes_read} bytes, expected {file_bytes}",),
        ));
    }

    Ok(StorageReadLaneReport {
        bytes_read,
        lane_count,
        elapsed,
    })
}

fn validate_storage_read_lane_count(lane_count: usize) -> io::Result<()> {
    if !STORAGE_READ_LANE_COUNTS.contains(&lane_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage read lanes must be 1, 2, 4, or 8",
        ));
    }

    Ok(())
}

fn storage_read_lane_range(
    file_bytes: u64,
    lane_index: usize,
    lane_count: usize,
) -> io::Result<(u64, u64)> {
    validate_storage_read_lane_count(lane_count)?;

    if lane_index >= lane_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage read lane index is outside the lane count",
        ));
    }

    let lane_count = u64::try_from(lane_count)
        .map_err(|_| io::Error::other("storage lane count cannot be represented"))?;

    if file_bytes % lane_count != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage benchmark file does not divide evenly across lanes",
        ));
    }

    let length = file_bytes / lane_count;

    let offset = length
        .checked_mul(
            u64::try_from(lane_index)
                .map_err(|_| io::Error::other("storage lane index cannot be represented"))?,
        )
        .ok_or_else(|| io::Error::other("storage lane offset overflowed"))?;

    Ok((offset, length))
}

fn read_uncached_range(source: &Path, offset: u64, length: u64) -> io::Result<u64> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(source)?;

    file.seek(SeekFrom::Start(offset))?;

    let mut buffer = AlignedReadBuffer::new(UNCACHED_READ_CHUNK_BYTES)?;

    let mut transferred = 0_u64;

    while transferred < length {
        let remaining = length - transferred;

        let requested = usize::try_from(remaining.min(UNCACHED_READ_CHUNK_BYTES as u64))
            .map_err(|_| io::Error::other("storage read request cannot be represented"))?;

        if requested % UNCACHED_ALIGNMENT_BYTES != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "uncached storage read request is not 4 KiB aligned",
            ));
        }

        let destination = &mut buffer.as_mut_slice()[..requested];

        let read = match file.read(destination) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue;
            }

            result => result?,
        };

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "uncached storage benchmark reached EOF early",
            ));
        }

        transferred = transferred
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| io::Error::other("storage read length cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("storage read count overflowed"))?;
    }

    Ok(transferred)
}

fn drive_letter_from_path(path: &Path) -> Option<u8> {
    let component = path.components().next()?;

    let Component::Prefix(prefix) = component else {
        return None;
    };

    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,

        _ => return None,
    };

    Some(letter.to_ascii_uppercase())
}

fn query_drive_seek_penalty(drive_letter: u8) -> io::Result<bool> {
    let device_path = format!(r"\\.\{}:", char::from(drive_letter),);

    let wide_path: Vec<u16> = OsStr::new(&device_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY:
    // wide_path is NUL-terminated and remains alive
    // for the duration of the call. We request no
    // read/write data access, only a device handle
    // suitable for a storage-property query.
    let raw_handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if raw_handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!(
                "failed to open storage volume {device_path}: {}",
                io::Error::last_os_error(),
            ),
        ));
    }

    let handle = VolumeHandle(raw_handle);

    let query = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_SEEK_PENALTY_PROPERTY,

        query_type: PROPERTY_STANDARD_QUERY,

        additional_parameters: [0],
    };

    let mut descriptor = DeviceSeekPenaltyDescriptor::default();

    let mut bytes_returned = 0_u32;

    let input_bytes = u32::try_from(mem::size_of::<StoragePropertyQuery>())
        .map_err(|_| io::Error::other("storage property query size cannot be represented"))?;

    let output_bytes = u32::try_from(mem::size_of::<DeviceSeekPenaltyDescriptor>())
        .map_err(|_| io::Error::other("seek-penalty descriptor size cannot be represented"))?;

    // SAFETY:
    // handle is a valid open volume handle.
    // query and descriptor are repr(C) buffers that
    // remain alive and correctly sized for the call.
    let succeeded = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            ptr::from_ref(&query).cast(),
            input_bytes,
            ptr::from_mut(&mut descriptor).cast(),
            output_bytes,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if succeeded == 0 {
        let error = io::Error::last_os_error();

        return Err(io::Error::new(
            error.kind(),
            format!("storage seek-penalty query failed for {device_path}: {error}",),
        ));
    }

    if bytes_returned < output_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "storage seek-penalty query returned {bytes_returned} bytes, expected at least {output_bytes}",
            ),
        ));
    }

    Ok(descriptor.incurs_seek_penalty != 0)
}

#[cfg(test)]
mod tests {
    use super::{drive_letter_from_path, storage_read_lane_range};
    use std::path::Path;

    #[test]
    fn drive_letter_paths_are_recognized() {
        assert_eq!(
            drive_letter_from_path(Path::new(r"C:\Windows"),),
            Some(b'C'),
        );

        assert_eq!(drive_letter_from_path(Path::new(r"f:\data"),), Some(b'F'),);

        assert_eq!(
            drive_letter_from_path(Path::new(r"\\?\D:\benchmark",),),
            Some(b'D'),
        );
    }

    #[test]
    fn unsupported_paths_have_no_drive_letter() {
        assert_eq!(drive_letter_from_path(Path::new(r".\relative"),), None,);

        assert_eq!(
            drive_letter_from_path(Path::new(r"\\server\share\folder",),),
            None,
        );
    }

    #[test]
    fn storage_read_lanes_cover_file_exactly() {
        let file_bytes = 2 * 1024 * 1024 * 1024_u64;

        assert_eq!(
            storage_read_lane_range(file_bytes, 0, 4,).unwrap(),
            (0, 512 * 1024 * 1024,),
        );

        assert_eq!(
            storage_read_lane_range(file_bytes, 1, 4,).unwrap(),
            (512 * 1024 * 1024, 512 * 1024 * 1024,),
        );

        assert_eq!(
            storage_read_lane_range(file_bytes, 3, 4,).unwrap(),
            (1536 * 1024 * 1024, 512 * 1024 * 1024,),
        );
    }

    #[test]
    fn storage_read_lanes_reject_bad_counts() {
        assert!(storage_read_lane_range(1024 * 1024, 0, 3,).is_err(),);

        assert!(storage_read_lane_range(1024 * 1024, 8, 8,).is_err(),);
    }
}
