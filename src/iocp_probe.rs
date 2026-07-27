use std::io;
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};

const PROBE_BYTES: u32 = 0x1234_5678;
const PROBE_KEY: usize = 0x4E43_5350;
const INFINITE_TIMEOUT: u32 = u32::MAX;

#[derive(Debug)]
pub struct ProbeReport {
    pub bytes_transferred: u32,
    pub completion_key: usize,
    pub elapsed: Duration,
}

impl ProbeReport {
    pub fn print(&self) {
        println!("Native IOCP probe complete");
        println!("  Completion bytes: 0x{:08X}", self.bytes_transferred);
        println!("  Completion key:   0x{:08X}", self.completion_key);
        println!("  Round-trip time:  {:.6} s", self.elapsed.as_secs_f64());
    }
}

#[derive(Debug)]
struct CompletionPort {
    handle: HANDLE,
}

impl CompletionPort {
    fn new() -> io::Result<Self> {
        // SAFETY:
        // INVALID_HANDLE_VALUE with a null existing port creates a new,
        // initially unassociated I/O completion port.
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 0) };

        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { handle })
    }

    fn post(&self, bytes_transferred: u32, completion_key: usize) -> io::Result<()> {
        // SAFETY:
        // The completion port handle remains valid for this call.
        // A null OVERLAPPED pointer is permitted for manually posted packets.
        let succeeded = unsafe {
            PostQueuedCompletionStatus(
                self.handle,
                bytes_transferred,
                completion_key,
                ptr::null_mut(),
            )
        };

        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn wait(&self) -> io::Result<CompletionPacket> {
        let mut bytes_transferred = 0_u32;
        let mut completion_key = 0_usize;
        let mut overlapped: *mut OVERLAPPED = ptr::null_mut();

        // SAFETY:
        // All output pointers refer to live writable variables.
        // The completion port handle remains valid for the duration of the call.
        let succeeded = unsafe {
            GetQueuedCompletionStatus(
                self.handle,
                &mut bytes_transferred,
                &mut completion_key,
                &mut overlapped,
                INFINITE_TIMEOUT,
            )
        };

        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(CompletionPacket {
            bytes_transferred,
            completion_key,
            overlapped,
        })
    }
}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        // SAFETY:
        // This object exclusively owns the completion-port handle.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[derive(Debug)]
struct CompletionPacket {
    bytes_transferred: u32,
    completion_key: usize,
    overlapped: *mut OVERLAPPED,
}

pub fn run() -> io::Result<ProbeReport> {
    let completion_port = CompletionPort::new()?;
    let started = Instant::now();

    completion_port.post(PROBE_BYTES, PROBE_KEY)?;
    let packet = completion_port.wait()?;

    if !packet.overlapped.is_null() {
        return Err(io::Error::other(
            "manually posted completion unexpectedly contained an OVERLAPPED pointer",
        ));
    }

    if packet.bytes_transferred != PROBE_BYTES {
        return Err(io::Error::other(format!(
            "completion byte value differs: expected 0x{PROBE_BYTES:08X}, received 0x{:08X}",
            packet.bytes_transferred
        )));
    }

    if packet.completion_key != PROBE_KEY {
        return Err(io::Error::other(format!(
            "completion key differs: expected 0x{PROBE_KEY:08X}, received 0x{:08X}",
            packet.completion_key
        )));
    }

    Ok(ProbeReport {
        bytes_transferred: packet.bytes_transferred,
        completion_key: packet.completion_key,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::{PROBE_BYTES, PROBE_KEY, run};

    #[test]
    fn completion_packet_round_trips_through_iocp() {
        let report = run().unwrap();

        assert_eq!(report.bytes_transferred, PROBE_BYTES);
        assert_eq!(report.completion_key, PROBE_KEY);
    }
}
