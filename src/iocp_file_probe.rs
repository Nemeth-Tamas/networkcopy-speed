use crate::copy_bench::{buffer_bytes_from_mib, format_bytes};
use crate::iocp_probe::CompletionPort;
use std::fs::OpenOptions;
use std::io;
use std::mem;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, ReadFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

const SOURCE_COMPLETION_KEY: usize = 0x4E43_5352;
const PREVIEW_BYTES: usize = 16;

pub const DEFAULT_READ_MIB: usize = 8;

#[derive(Debug)]
pub struct FileReadReport {
    pub requested_bytes: usize,
    pub bytes_transferred: u32,
    pub completion_key: usize,
    pub completed_synchronously: bool,
    pub elapsed: Duration,
    pub preview: Vec<u8>,
}

impl FileReadReport {
    pub fn print(&self) {
        println!("Native overlapped file-read probe complete");
        println!(
            "  Requested:      {}",
            format_bytes(self.requested_bytes as u64)
        );
        println!(
            "  Transferred:    {}",
            format_bytes(u64::from(self.bytes_transferred))
        );
        println!("  Completion key: 0x{:08X}", self.completion_key);
        println!(
            "  Submission:     {}",
            if self.completed_synchronously {
                "completed immediately"
            } else {
                "returned ERROR_IO_PENDING"
            }
        );
        println!("  Completion time: {:.6} s", self.elapsed.as_secs_f64());
        println!("  First bytes:     {}", format_preview(&self.preview));
    }
}

#[repr(C)]
struct ReadOperation {
    overlapped: OVERLAPPED,
    buffer: Vec<u8>,
}

impl ReadOperation {
    fn new(buffer_bytes: usize) -> Self {
        // SAFETY:
        // An all-zero OVERLAPPED structure represents a valid initial
        // operation at file offset zero with no event handle.
        let overlapped = unsafe { mem::zeroed() };

        Self {
            overlapped,
            buffer: vec![0_u8; buffer_bytes],
        }
    }
}

pub fn run(source: &Path, read_mib: usize) -> io::Result<FileReadReport> {
    let requested_bytes = buffer_bytes_from_mib(read_mib)?;

    let source_file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(source)?;

    let metadata = source_file.metadata()?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let source_len = metadata.len();

    if source_len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source file is empty",
        ));
    }

    let read_bytes = if source_len < requested_bytes as u64 {
        source_len as usize
    } else {
        requested_bytes
    };

    let read_bytes_u32 = u32::try_from(read_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "overlapped probe buffer exceeds the ReadFile byte-count limit",
        )
    })?;

    let completion_port = CompletionPort::new()?;
    let file_handle = source_file.as_raw_handle() as HANDLE;

    completion_port.associate(file_handle, SOURCE_COMPLETION_KEY)?;

    // The allocation containing OVERLAPPED must remain at the same address
    // until Windows has completed the operation.
    let mut operation = Box::pin(ReadOperation::new(read_bytes));

    let (buffer_pointer, overlapped_pointer) = {
        // SAFETY:
        // The operation is pinned for the entire outstanding-I/O lifetime.
        // Neither the Vec allocation nor OVERLAPPED structure will move or
        // be destroyed before the completion packet is received.
        let operation = unsafe { operation.as_mut().get_unchecked_mut() };

        (
            operation.buffer.as_mut_ptr(),
            &mut operation.overlapped as *mut OVERLAPPED,
        )
    };

    let started = Instant::now();

    // SAFETY:
    // The file handle is valid and associated with the completion port.
    // The buffer contains read_bytes writable bytes.
    // The pinned OVERLAPPED and buffer remain alive until completion.
    let submitted = unsafe {
        ReadFile(
            file_handle,
            buffer_pointer,
            read_bytes_u32,
            ptr::null_mut(),
            overlapped_pointer,
        )
    };

    let completed_synchronously = submitted != 0;

    if submitted == 0 {
        let error = io::Error::last_os_error();

        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
    }

    let packet = completion_port.wait()?;
    let elapsed = started.elapsed();

    if packet.completion_key != SOURCE_COMPLETION_KEY {
        return Err(io::Error::other(format!(
            "unexpected completion key: expected 0x{SOURCE_COMPLETION_KEY:08X}, \
             received 0x{:08X}",
            packet.completion_key
        )));
    }

    if packet.overlapped != overlapped_pointer {
        return Err(io::Error::other(
            "completion packet returned a different OVERLAPPED pointer",
        ));
    }

    if packet.bytes_transferred > read_bytes_u32 {
        return Err(io::Error::other(format!(
            "completion reported {} bytes for a {} byte buffer",
            packet.bytes_transferred, read_bytes_u32
        )));
    }

    if packet.bytes_transferred == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "overlapped read completed without transferring data",
        ));
    }

    let transferred = packet.bytes_transferred as usize;
    let preview_length = transferred.min(PREVIEW_BYTES);

    let operation = operation.as_ref().get_ref();
    let preview = operation.buffer[..preview_length].to_vec();

    Ok(FileReadReport {
        requested_bytes: read_bytes,
        bytes_transferred: packet.bytes_transferred,
        completion_key: packet.completion_key,
        completed_synchronously,
        elapsed,
        preview,
    })
}

fn format_preview(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 3);

    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            result.push(' ');
        }

        use std::fmt::Write as _;
        write!(result, "{byte:02X}").expect("writing to a String cannot fail");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{PREVIEW_BYTES, SOURCE_COMPLETION_KEY, run};
    use std::env;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn overlapped_read_completes_through_iocp() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let path = env::temp_dir().join(format!(
            "networkcopy-iocp-read-{}-{unique}.bin",
            process::id()
        ));

        let contents = b"NetworkCopy Speed Edition overlapped read test payload";

        fs::write(&path, contents).unwrap();

        let result = run(&path, 1);
        let cleanup_result = fs::remove_file(&path);

        let report = result.unwrap();
        cleanup_result.unwrap();

        assert_eq!(report.bytes_transferred as usize, contents.len());
        assert_eq!(report.completion_key, SOURCE_COMPLETION_KEY);
        assert_eq!(report.preview, contents[..PREVIEW_BYTES].to_vec());
    }
}
