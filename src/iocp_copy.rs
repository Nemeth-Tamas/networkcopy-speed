use crate::copy_bench::{
    binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes, reject_same_file,
};
use crate::iocp_probe::CompletionPort;
use crate::pipeline_bench::validate_config;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::mem;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};

const SOURCE_COMPLETION_KEY: usize = 0x4E43_5352;
const DESTINATION_COMPLETION_KEY: usize = 0x4E43_5357;
const MIB: usize = 1024 * 1024;

pub const DEFAULT_CHUNK_MIB: usize = 8;
pub const DEFAULT_OPERATION_COUNT: usize = 8;

#[derive(Debug)]
pub struct IocpCopyReport {
    pub bytes_copied: u64,
    pub chunk_bytes: usize,
    pub operation_count: usize,
    pub pool_bytes: usize,
    pub read_submissions: u64,
    pub write_submissions: u64,
    pub immediate_read_submissions: u64,
    pub immediate_write_submissions: u64,
    pub setup_elapsed: Duration,
    pub io_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl IocpCopyReport {
    pub fn print(&self) {
        println!("Native IOCP copy complete");
        println!("  Bytes copied:       {}", format_bytes(self.bytes_copied));
        println!("  Chunk size:         {} MiB", self.chunk_bytes / MIB);
        println!("  Operations:         {}", self.operation_count);
        println!("  Buffer pool:        {} MiB", self.pool_bytes / MIB);
        println!("  Read submissions:   {}", self.read_submissions);
        println!("  Write submissions:  {}", self.write_submissions);
        println!("  Immediate reads:    {}", self.immediate_read_submissions);
        println!("  Immediate writes:   {}", self.immediate_write_submissions);
        println!(
            "  Setup time:         {:.3} s",
            self.setup_elapsed.as_secs_f64()
        );
        println!(
            "  Transfer time:      {:.3} s",
            self.io_elapsed.as_secs_f64()
        );
        println!(
            "  Total time:         {:.3} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Throughput:         {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_copied, self.io_elapsed),
            binary_mebibytes_per_second(self.bytes_copied, self.io_elapsed)
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationStage {
    Idle,
    Reading,
    Writing,
}

// This matches the documented offset form of the native OVERLAPPED structure.
// Keeping our own named representation avoids depending on generated anonymous
// union field names while retaining the exact Windows ABI layout.
#[repr(C)]
#[derive(Debug)]
struct OffsetOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: HANDLE,
}

impl OffsetOverlapped {
    fn new(offset: u64) -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: offset as u32,
            offset_high: (offset >> 32) as u32,
            event: ptr::null_mut(),
        }
    }

    fn as_overlapped_pointer(&mut self) -> *mut OVERLAPPED {
        ptr::from_mut(self).cast()
    }
}

#[repr(C)]
#[derive(Debug)]
struct IoOperation {
    // This must remain the first field. Windows returns a pointer to this
    // structure, allowing us to recover the owning operation allocation.
    overlapped: OffsetOverlapped,
    buffer: Vec<u8>,
    stage: OperationStage,
    file_offset: u64,
    requested_bytes: u32,
}

impl IoOperation {
    fn new(chunk_bytes: usize) -> Self {
        Self {
            overlapped: OffsetOverlapped::new(0),
            buffer: vec![0_u8; chunk_bytes],
            stage: OperationStage::Idle,
            file_offset: 0,
            requested_bytes: 0,
        }
    }

    fn prepare(&mut self, stage: OperationStage, file_offset: u64, requested_bytes: u32) {
        self.overlapped = OffsetOverlapped::new(file_offset);
        self.stage = stage;
        self.file_offset = file_offset;
        self.requested_bytes = requested_bytes;
    }

    fn overlapped_pointer(&mut self) -> *mut OVERLAPPED {
        self.overlapped.as_overlapped_pointer()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Submission {
    Immediate,
    Pending,
}

pub fn run(
    source: &Path,
    destination: &Path,
    chunk_mib: usize,
    operation_count: usize,
) -> io::Result<IocpCopyReport> {
    let total_started = Instant::now();
    let (chunk_bytes, pool_bytes) = validate_config(chunk_mib, operation_count)?;

    validate_overlapped_layout()?;
    reject_same_file(source, destination)?;

    let source_file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(source)?;

    let source_metadata = source_file.metadata()?;

    if !source_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", source.display()),
        ));
    }

    let source_len = source_metadata.len();

    let mut destination_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(destination)?;

    destination_file.set_len(source_len)?;

    let source_handle = source_file.as_raw_handle() as HANDLE;
    let destination_handle = destination_file.as_raw_handle() as HANDLE;

    let completion_port = CompletionPort::new()?;

    completion_port.associate(source_handle, SOURCE_COMPLETION_KEY)?;

    completion_port.associate(destination_handle, DESTINATION_COMPLETION_KEY)?;

    // Moving a Box does not move its heap allocation. We allocate every
    // operation before submitting I/O, so each OVERLAPPED address remains
    // stable until every completion has been drained.
    let mut operations: Vec<Box<IoOperation>> = (0..operation_count)
        .map(|_| Box::new(IoOperation::new(chunk_bytes)))
        .collect();

    let operation_addresses: HashSet<usize> = operations
        .iter_mut()
        .map(|operation| operation.overlapped_pointer() as usize)
        .collect();

    let setup_elapsed = total_started.elapsed();
    let io_started = Instant::now();

    let mut next_offset = 0_u64;
    let mut outstanding = 0_usize;
    let mut bytes_read = 0_u64;
    let mut bytes_written = 0_u64;

    let mut read_submissions = 0_u64;
    let mut write_submissions = 0_u64;
    let mut immediate_read_submissions = 0_u64;
    let mut immediate_write_submissions = 0_u64;

    let mut first_error: Option<io::Error> = None;
    let mut cancellation_requested = false;

    for operation in &mut operations {
        if next_offset >= source_len {
            break;
        }

        let requested_bytes = request_size(source_len, next_offset, chunk_bytes)?;

        match submit_read(source_handle, operation, next_offset, requested_bytes) {
            Ok(submission) => {
                outstanding += 1;
                read_submissions += 1;

                if submission == Submission::Immediate {
                    immediate_read_submissions += 1;
                }

                next_offset += u64::from(requested_bytes);
            }

            Err(error) => {
                record_failure(
                    &mut first_error,
                    &mut cancellation_requested,
                    source_handle,
                    destination_handle,
                    error,
                );

                break;
            }
        }
    }

    while outstanding > 0 {
        let packet = match completion_port.wait_io() {
            Ok(packet) => packet,

            Err(error) => {
                // We cannot prove that Windows has stopped using the
                // operation allocations if the completion port itself
                // becomes unusable. Preserve them rather than risk freeing
                // memory that the kernel may still reference.
                cancel_all(source_handle, destination_handle);
                mem::forget(operations);
                return Err(error);
            }
        };

        outstanding -= 1;

        if first_error.is_some() {
            // A failure has already triggered cancellation. Drain every
            // remaining completion packet without submitting new work.
            continue;
        }

        if let Some(error_code) = packet.error_code {
            record_failure(
                &mut first_error,
                &mut cancellation_requested,
                source_handle,
                destination_handle,
                io::Error::from_raw_os_error(error_code),
            );

            continue;
        }

        let operation_address = packet.overlapped as usize;

        if !operation_addresses.contains(&operation_address) {
            record_failure(
                &mut first_error,
                &mut cancellation_requested,
                source_handle,
                destination_handle,
                io::Error::other("IOCP returned an unknown OVERLAPPED pointer"),
            );

            continue;
        }

        // SAFETY:
        // Every accepted pointer was created from the first field of one of
        // the still-live boxed IoOperation allocations. The completed
        // operation is no longer in use by Windows and may now be reused.
        let operation = unsafe { &mut *(packet.overlapped.cast::<IoOperation>()) };

        match operation.stage {
            OperationStage::Reading => {
                if packet.completion_key != SOURCE_COMPLETION_KEY {
                    record_failure(
                        &mut first_error,
                        &mut cancellation_requested,
                        source_handle,
                        destination_handle,
                        io::Error::other("read operation completed with the wrong key"),
                    );

                    continue;
                }

                if packet.bytes_transferred != operation.requested_bytes {
                    record_failure(
                        &mut first_error,
                        &mut cancellation_requested,
                        source_handle,
                        destination_handle,
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "read at offset {} requested {} bytes but completed with {}",
                                operation.file_offset,
                                operation.requested_bytes,
                                packet.bytes_transferred
                            ),
                        ),
                    );

                    continue;
                }

                bytes_read = bytes_read
                    .checked_add(u64::from(packet.bytes_transferred))
                    .ok_or_else(|| io::Error::other("read byte count overflowed"))?;

                match submit_write(destination_handle, operation, packet.bytes_transferred) {
                    Ok(submission) => {
                        outstanding += 1;
                        write_submissions += 1;

                        if submission == Submission::Immediate {
                            immediate_write_submissions += 1;
                        }
                    }

                    Err(error) => {
                        record_failure(
                            &mut first_error,
                            &mut cancellation_requested,
                            source_handle,
                            destination_handle,
                            error,
                        );
                    }
                }
            }

            OperationStage::Writing => {
                if packet.completion_key != DESTINATION_COMPLETION_KEY {
                    record_failure(
                        &mut first_error,
                        &mut cancellation_requested,
                        source_handle,
                        destination_handle,
                        io::Error::other("write operation completed with the wrong key"),
                    );

                    continue;
                }

                if packet.bytes_transferred != operation.requested_bytes {
                    record_failure(
                        &mut first_error,
                        &mut cancellation_requested,
                        source_handle,
                        destination_handle,
                        io::Error::new(
                            io::ErrorKind::WriteZero,
                            format!(
                                "write at offset {} requested {} bytes but completed with {}",
                                operation.file_offset,
                                operation.requested_bytes,
                                packet.bytes_transferred
                            ),
                        ),
                    );

                    continue;
                }

                bytes_written = bytes_written
                    .checked_add(u64::from(packet.bytes_transferred))
                    .ok_or_else(|| io::Error::other("written byte count overflowed"))?;

                operation.stage = OperationStage::Idle;

                if next_offset < source_len {
                    let requested_bytes = request_size(source_len, next_offset, chunk_bytes)?;

                    match submit_read(source_handle, operation, next_offset, requested_bytes) {
                        Ok(submission) => {
                            outstanding += 1;
                            read_submissions += 1;

                            if submission == Submission::Immediate {
                                immediate_read_submissions += 1;
                            }

                            next_offset += u64::from(requested_bytes);
                        }

                        Err(error) => {
                            record_failure(
                                &mut first_error,
                                &mut cancellation_requested,
                                source_handle,
                                destination_handle,
                                error,
                            );
                        }
                    }
                }
            }

            OperationStage::Idle => {
                record_failure(
                    &mut first_error,
                    &mut cancellation_requested,
                    source_handle,
                    destination_handle,
                    io::Error::other("idle operation produced an IOCP completion"),
                );
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    destination_file.flush()?;

    let io_elapsed = io_started.elapsed();
    let total_elapsed = total_started.elapsed();

    if bytes_read != source_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "source length changed during transfer: expected {source_len} bytes, read \
                 {bytes_read} bytes"
            ),
        ));
    }

    if bytes_written != source_len {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "destination is incomplete: expected {source_len} bytes, wrote \
                 {bytes_written} bytes"
            ),
        ));
    }

    Ok(IocpCopyReport {
        bytes_copied: bytes_written,
        chunk_bytes,
        operation_count,
        pool_bytes,
        read_submissions,
        write_submissions,
        immediate_read_submissions,
        immediate_write_submissions,
        setup_elapsed,
        io_elapsed,
        total_elapsed,
    })
}

fn submit_read(
    source_handle: HANDLE,
    operation: &mut IoOperation,
    offset: u64,
    requested_bytes: u32,
) -> io::Result<Submission> {
    operation.prepare(OperationStage::Reading, offset, requested_bytes);

    let buffer_pointer = operation.buffer.as_mut_ptr();
    let overlapped_pointer = operation.overlapped_pointer();

    // SAFETY:
    // The source handle is valid and opened for overlapped I/O.
    // The operation owns a writable buffer of at least requested_bytes.
    // Its boxed allocation remains stable until completion is dequeued.
    let submitted = unsafe {
        ReadFile(
            source_handle,
            buffer_pointer,
            requested_bytes,
            ptr::null_mut(),
            overlapped_pointer,
        )
    };

    parse_submission_result(submitted)
}

fn submit_write(
    destination_handle: HANDLE,
    operation: &mut IoOperation,
    requested_bytes: u32,
) -> io::Result<Submission> {
    operation.prepare(
        OperationStage::Writing,
        operation.file_offset,
        requested_bytes,
    );

    let buffer_pointer = operation.buffer.as_ptr();
    let overlapped_pointer = operation.overlapped_pointer();

    // SAFETY:
    // The destination handle is valid and opened for overlapped I/O.
    // The operation buffer contains requested_bytes initialized bytes from
    // the completed read. The allocation remains stable until completion.
    let submitted = unsafe {
        WriteFile(
            destination_handle,
            buffer_pointer,
            requested_bytes,
            ptr::null_mut(),
            overlapped_pointer,
        )
    };

    parse_submission_result(submitted)
}

fn parse_submission_result(submitted: i32) -> io::Result<Submission> {
    if submitted != 0 {
        return Ok(Submission::Immediate);
    }

    let error = io::Error::last_os_error();

    if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
        return Ok(Submission::Pending);
    }

    Err(error)
}

fn request_size(file_len: u64, offset: u64, chunk_bytes: usize) -> io::Result<u32> {
    let remaining = file_len.checked_sub(offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file offset exceeds the source length",
        )
    })?;

    let requested = remaining.min(chunk_bytes as u64);

    u32::try_from(requested).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk exceeds the native ReadFile byte-count limit",
        )
    })
}

fn record_failure(
    first_error: &mut Option<io::Error>,
    cancellation_requested: &mut bool,
    source_handle: HANDLE,
    destination_handle: HANDLE,
    error: io::Error,
) {
    if first_error.is_none() {
        *first_error = Some(error);
    }

    if !*cancellation_requested {
        cancel_all(source_handle, destination_handle);
        *cancellation_requested = true;
    }
}

fn cancel_all(source_handle: HANDLE, destination_handle: HANDLE) {
    // SAFETY:
    // Both handles remain valid here. A null OVERLAPPED pointer requests
    // cancellation of every operation issued against the handle.
    unsafe {
        let _ = CancelIoEx(source_handle, ptr::null());
        let _ = CancelIoEx(destination_handle, ptr::null());
    }
}

fn validate_overlapped_layout() -> io::Result<()> {
    if mem::size_of::<OffsetOverlapped>() != mem::size_of::<OVERLAPPED>() {
        return Err(io::Error::other(format!(
            "OVERLAPPED size mismatch: native wrapper is {} bytes, Windows binding is {} bytes",
            mem::size_of::<OffsetOverlapped>(),
            mem::size_of::<OVERLAPPED>()
        )));
    }

    if mem::align_of::<OffsetOverlapped>() != mem::align_of::<OVERLAPPED>() {
        return Err(io::Error::other(format!(
            "OVERLAPPED alignment mismatch: native wrapper alignment is {}, Windows binding \
             alignment is {}",
            mem::align_of::<OffsetOverlapped>(),
            mem::align_of::<OVERLAPPED>()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_CHUNK_MIB, DEFAULT_OPERATION_COUNT, OffsetOverlapped, run,
        validate_overlapped_layout,
    };
    use std::env;
    use std::fs;
    use std::mem;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::System::IO::OVERLAPPED;

    #[test]
    fn native_overlapped_layout_matches_windows() {
        validate_overlapped_layout().unwrap();

        assert_eq!(
            mem::size_of::<OffsetOverlapped>(),
            mem::size_of::<OVERLAPPED>()
        );

        assert_eq!(
            mem::align_of::<OffsetOverlapped>(),
            mem::align_of::<OVERLAPPED>()
        );
    }

    #[test]
    fn copies_file_through_native_iocp() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let source = env::temp_dir().join(format!(
            "networkcopy-iocp-source-{}-{unique}.bin",
            process::id()
        ));

        let destination = env::temp_dir().join(format!(
            "networkcopy-iocp-destination-{}-{unique}.bin",
            process::id()
        ));

        let mut contents = vec![0_u8; 2 * 1024 * 1024 + 137];

        for (index, byte) in contents.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }

        fs::write(&source, &contents).unwrap();

        let copy_result = run(
            &source,
            &destination,
            DEFAULT_CHUNK_MIB,
            DEFAULT_OPERATION_COUNT,
        );

        let copied_contents = fs::read(&destination);

        let source_cleanup = fs::remove_file(&source);
        let destination_cleanup = fs::remove_file(&destination);

        let report = copy_result.unwrap();
        let copied_contents = copied_contents.unwrap();

        source_cleanup.unwrap();
        destination_cleanup.unwrap();

        assert_eq!(report.bytes_copied, contents.len() as u64);
        assert_eq!(copied_contents, contents);
    }
}
