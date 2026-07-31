use crate::calibrated_transfer;
use crate::console_progress::ProgressCounter;
use crate::multistream_copy::DestinationMode;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

const PREPARE_REQUEST_VERSION: u16 = 1;
const PREPARE_REQUEST_HEADER_BYTES: usize = 8;

const JOB_STATUS_VERSION: u16 = 2;
const JOB_STATUS_HEADER_BYTES: usize = 20;

const CANCEL_REQUEST_VERSION: u16 = 1;
const CANCEL_REQUEST_BYTES: usize = 12;

const UPDATE_EXISTING_FLAG: u8 = 0x01;
const KNOWN_JOB_FLAGS: u8 = UPDATE_EXISTING_FLAG;

const MAX_DESTINATION_PATH_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementJobPhase {
    Idle = 0,
    ReceiverPrepared = 1,
}

impl ManagementJobPhase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",

            Self::ReceiverPrepared => "receiver waiting for sender",
        }
    }
}

impl TryFrom<u8> for ManagementJobPhase {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Idle),

            1 => Ok(Self::ReceiverPrepared),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management job phase {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedReceiveJob {
    pub job_id: u64,

    pub transfer_port: u16,

    pub destination_root: String,

    pub update_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementJobStatus {
    pub phase: ManagementJobPhase,

    pub job_id: Option<u64>,

    pub transfer_port: Option<u16>,

    pub destination_root: Option<String>,

    pub update_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrepareReceiveRequest {
    pub(crate) destination_root: String,

    pub(crate) update_existing: bool,
}

#[derive(Clone, Debug)]
struct ActiveReceiveJob {
    job: PreparedReceiveJob,

    progress: ProgressCounter,
}

#[derive(Debug, Default)]
struct JobRegistryInner {
    active_receive: Option<ActiveReceiveJob>,
}

#[derive(Debug)]
pub(crate) struct ManagementJobRegistry {
    next_job_id: AtomicU64,

    inner: Mutex<JobRegistryInner>,
}

impl ManagementJobRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_job_id: AtomicU64::new(1),

            inner: Mutex::new(JobRegistryInner::default()),
        }
    }

    pub(crate) fn is_busy(&self) -> io::Result<bool> {
        Ok(self.lock_inner()?.active_receive.is_some())
    }

    pub(crate) fn prepare_receive_on(
        self: &Arc<Self>,
        destination_root: &str,
        update_existing: bool,
        bind_address: SocketAddr,
    ) -> io::Result<PreparedReceiveJob> {
        validate_destination_path(destination_root)?;

        if let Some(active) = &self.lock_inner()?.active_receive {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "management agent already has active job {}",
                    active.job.job_id,
                ),
            ));
        }

        let destination_path = Path::new(destination_root);

        fs::create_dir_all(destination_path)?;

        let metadata = fs::metadata(destination_path)?;

        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver destination does not identify a directory",
            ));
        }

        let listener = TcpListener::bind(bind_address)?;

        let transfer_port = listener.local_addr()?.port();

        if transfer_port == 0 {
            return Err(io::Error::other(
                "managed receiver obtained transfer port zero",
            ));
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);

        if job_id == 0 {
            return Err(io::Error::other(
                "management job ID counter wrapped to zero",
            ));
        }

        let job = PreparedReceiveJob {
            job_id,

            transfer_port,

            destination_root: destination_root.to_owned(),

            update_existing,
        };

        let progress = ProgressCounter::new("Waiting for managed sender", 0);

        {
            let mut inner = self.lock_inner()?;

            if let Some(active) = &inner.active_receive {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "management agent already has active job {}",
                        active.job.job_id,
                    ),
                ));
            }

            inner.active_receive = Some(ActiveReceiveJob {
                job: job.clone(),

                progress: progress.clone(),
            });
        }

        let worker_registry = Arc::clone(self);

        let worker_progress = progress;

        let worker_destination = PathBuf::from(destination_root);

        let destination_mode = if update_existing {
            DestinationMode::UpdateVerified
        } else {
            DestinationMode::Fresh
        };

        let spawn_result = thread::Builder::new()
            .name(format!("networkcopy-managed-receiver-{job_id}"))
            .spawn(move || {
                let result = calibrated_transfer::receive_once_with_progress_and_mode(
                    listener,
                    &worker_destination,
                    worker_progress,
                    destination_mode,
                );

                if let Err(error) = worker_registry.finish_receive(job_id) {
                    eprintln!("failed to finalize managed receiver job {job_id}: {error}");
                }

                match result {
                    Ok(report) => {
                        println!("Managed receiver job {job_id} complete");

                        println!("  Files received: {}", report.transfer.files_received,);

                        println!("  Bytes received: {}", report.transfer.bytes_received,);
                    }

                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        println!("Managed receiver job {job_id} cancelled");
                    }

                    Err(error) => {
                        eprintln!("Managed receiver job {job_id} failed: {error}");
                    }
                }
            });

        if let Err(error) = spawn_result {
            let _ = self.finish_receive(job_id);

            return Err(error);
        }

        Ok(job)
    }

    pub(crate) fn status(&self) -> io::Result<ManagementJobStatus> {
        let inner = self.lock_inner()?;

        match &inner.active_receive {
            Some(active) => {
                let job = &active.job;

                Ok(ManagementJobStatus {
                    phase: ManagementJobPhase::ReceiverPrepared,

                    job_id: Some(job.job_id),

                    transfer_port: Some(job.transfer_port),

                    destination_root: Some(job.destination_root.clone()),

                    update_existing: job.update_existing,
                })
            }

            None => Ok(ManagementJobStatus {
                phase: ManagementJobPhase::Idle,

                job_id: None,

                transfer_port: None,

                destination_root: None,

                update_existing: false,
            }),
        }
    }

    pub(crate) fn cancel(&self, job_id: u64) -> io::Result<PreparedReceiveJob> {
        if job_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "management job ID must not be zero",
            ));
        }

        let active = {
            let mut inner = self.lock_inner()?;

            let Some(active) = &inner.active_receive else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "management agent has no active job",
                ));
            };

            if active.job.job_id != job_id {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "management agent has active job {}, not requested job {job_id}",
                        active.job.job_id,
                    ),
                ));
            }

            inner.active_receive.take().ok_or_else(|| {
                io::Error::other("active management job disappeared during cancellation")
            })?
        };

        active.progress.cancel();

        Ok(active.job)
    }

    fn finish_receive(&self, job_id: u64) -> io::Result<()> {
        let mut inner = self.lock_inner()?;

        let matches_job = inner
            .active_receive
            .as_ref()
            .is_some_and(|active| active.job.job_id == job_id);

        if matches_job {
            inner.active_receive.take();
        }

        Ok(())
    }

    fn lock_inner(&self) -> io::Result<MutexGuard<'_, JobRegistryInner>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("management job registry lock was poisoned"))
    }
}

pub(crate) fn encode_prepare_request(
    destination_root: &str,
    update_existing: bool,
) -> io::Result<Vec<u8>> {
    validate_destination_path(destination_root)?;

    let path_length = u32::try_from(destination_root.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver destination length cannot be represented",
        )
    })?;

    let total_length = PREPARE_REQUEST_HEADER_BYTES
        .checked_add(destination_root.len())
        .ok_or_else(|| io::Error::other("prepare-receive request length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&PREPARE_REQUEST_VERSION.to_le_bytes());

    payload.push(encode_job_flags(update_existing));

    payload.push(0);

    payload.extend_from_slice(&path_length.to_le_bytes());

    payload.extend_from_slice(destination_root.as_bytes());

    Ok(payload)
}

pub(crate) fn decode_prepare_request(payload: &[u8]) -> io::Result<PrepareReceiveRequest> {
    if payload.len() < PREPARE_REQUEST_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "prepare-receive request has {} bytes, expected at least {PREPARE_REQUEST_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "prepare-receive version was malformed",
        )
    })?);

    if version != PREPARE_REQUEST_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported prepare-receive request version {version}"),
        ));
    }

    let update_existing = decode_job_flags(payload[2])?;

    if payload[3] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepare-receive reserved byte was not zero",
        ));
    }

    let path_length = usize::try_from(u32::from_le_bytes(payload[4..8].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepare-receive path length was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "prepare-receive path length cannot be represented",
        )
    })?;

    let expected_length = PREPARE_REQUEST_HEADER_BYTES
        .checked_add(path_length)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepare-receive request length overflowed",
            )
        })?;

    if payload.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "prepare-receive request has {} bytes, expected {expected_length}",
                payload.len(),
            ),
        ));
    }

    let destination_root = decode_destination_path(&payload[PREPARE_REQUEST_HEADER_BYTES..])?;

    Ok(PrepareReceiveRequest {
        destination_root,
        update_existing,
    })
}

pub(crate) fn encode_prepared_response(job: &PreparedReceiveJob) -> io::Result<Vec<u8>> {
    encode_status(&ManagementJobStatus {
        phase: ManagementJobPhase::ReceiverPrepared,

        job_id: Some(job.job_id),

        transfer_port: Some(job.transfer_port),

        destination_root: Some(job.destination_root.clone()),

        update_existing: job.update_existing,
    })
}

pub(crate) fn decode_prepared_response(payload: &[u8]) -> io::Result<PreparedReceiveJob> {
    let status = decode_status(payload)?;

    if status.phase != ManagementJobPhase::ReceiverPrepared {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepare-receive response did not describe a prepared receiver",
        ));
    }

    Ok(PreparedReceiveJob {
        job_id: status.job_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared receiver response omitted its job ID",
            )
        })?,

        transfer_port: status.transfer_port.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared receiver response omitted its transfer port",
            )
        })?,

        destination_root: status.destination_root.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared receiver response omitted its destination",
            )
        })?,

        update_existing: status.update_existing,
    })
}

pub(crate) fn encode_status(status: &ManagementJobStatus) -> io::Result<Vec<u8>> {
    let (job_id, transfer_port, destination_root) = match status.phase {
        ManagementJobPhase::Idle => {
            if status.job_id.is_some()
                || status.transfer_port.is_some()
                || status.destination_root.is_some()
                || status.update_existing
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "idle management job status contained active-job fields",
                ));
            }

            (0_u64, 0_u16, "")
        }

        ManagementJobPhase::ReceiverPrepared => {
            let job_id = status.job_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status omitted its job ID",
                )
            })?;

            if job_id == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status used job ID zero",
                ));
            }

            let transfer_port = status.transfer_port.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status omitted its transfer port",
                )
            })?;

            if transfer_port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status used transfer port zero",
                ));
            }

            let destination_root = status.destination_root.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status omitted its destination",
                )
            })?;

            validate_destination_path(destination_root)?;

            (job_id, transfer_port, destination_root)
        }
    };

    let path_length = u32::try_from(destination_root.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "management job destination length cannot be represented",
        )
    })?;

    let total_length = JOB_STATUS_HEADER_BYTES
        .checked_add(destination_root.len())
        .ok_or_else(|| io::Error::other("management job status length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&JOB_STATUS_VERSION.to_le_bytes());

    payload.push(status.phase as u8);

    payload.push(encode_job_flags(status.update_existing));

    payload.extend_from_slice(&job_id.to_le_bytes());

    payload.extend_from_slice(&transfer_port.to_le_bytes());

    payload.extend_from_slice(&0_u16.to_le_bytes());

    payload.extend_from_slice(&path_length.to_le_bytes());

    payload.extend_from_slice(destination_root.as_bytes());

    Ok(payload)
}

pub(crate) fn decode_status(payload: &[u8]) -> io::Result<ManagementJobStatus> {
    if payload.len() < JOB_STATUS_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management job status has {} bytes, expected at least {JOB_STATUS_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management job status version was malformed",
        )
    })?);

    if version != JOB_STATUS_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported management job status version {version}"),
        ));
    }

    let phase = ManagementJobPhase::try_from(payload[2])?;

    let update_existing = decode_job_flags(payload[3])?;

    let job_id = u64::from_le_bytes(payload[4..12].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management job ID was malformed",
        )
    })?);

    let transfer_port = u16::from_le_bytes(payload[12..14].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management transfer port was malformed",
        )
    })?);

    let reserved = u16::from_le_bytes(payload[14..16].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management job reserved field was malformed",
        )
    })?);

    if reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "management job reserved field was not zero",
        ));
    }

    let path_length = usize::try_from(u32::from_le_bytes(payload[16..20].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management job path length was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "management job path length cannot be represented",
        )
    })?;

    let expected_length = JOB_STATUS_HEADER_BYTES
        .checked_add(path_length)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management job status length overflowed",
            )
        })?;

    if payload.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "management job status has {} bytes, expected {expected_length}",
                payload.len(),
            ),
        ));
    }

    match phase {
        ManagementJobPhase::Idle => {
            if job_id != 0 || transfer_port != 0 || path_length != 0 || update_existing {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "idle management job status contained active-job fields",
                ));
            }

            Ok(ManagementJobStatus {
                phase,

                job_id: None,

                transfer_port: None,

                destination_root: None,

                update_existing: false,
            })
        }

        ManagementJobPhase::ReceiverPrepared => {
            if job_id == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prepared receiver status used job ID zero",
                ));
            }

            if transfer_port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prepared receiver status used transfer port zero",
                ));
            }

            let destination_root = decode_destination_path(&payload[JOB_STATUS_HEADER_BYTES..])?;

            Ok(ManagementJobStatus {
                phase,

                job_id: Some(job_id),

                transfer_port: Some(transfer_port),

                destination_root: Some(destination_root),

                update_existing,
            })
        }
    }
}

pub(crate) fn encode_cancel_request(job_id: u64) -> io::Result<Vec<u8>> {
    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "management job ID must not be zero",
        ));
    }

    let mut payload = Vec::with_capacity(CANCEL_REQUEST_BYTES);

    payload.extend_from_slice(&CANCEL_REQUEST_VERSION.to_le_bytes());

    payload.extend_from_slice(&0_u16.to_le_bytes());

    payload.extend_from_slice(&job_id.to_le_bytes());

    Ok(payload)
}

pub(crate) fn decode_cancel_request(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != CANCEL_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "cancel-job request has {} bytes, expected {CANCEL_REQUEST_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cancel-job version was malformed",
        )
    })?);

    if version != CANCEL_REQUEST_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported cancel-job version {version}"),
        ));
    }

    let reserved = u16::from_le_bytes(payload[2..4].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cancel-job reserved field was malformed",
        )
    })?);

    if reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cancel-job reserved field was not zero",
        ));
    }

    let job_id =
        u64::from_le_bytes(payload[4..12].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "cancel-job ID was malformed")
        })?);

    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cancel-job request used job ID zero",
        ));
    }

    Ok(job_id)
}

fn encode_job_flags(update_existing: bool) -> u8 {
    if update_existing {
        UPDATE_EXISTING_FLAG
    } else {
        0
    }
}

fn decode_job_flags(flags: u8) -> io::Result<bool> {
    if flags & !KNOWN_JOB_FLAGS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("management job flags contained unknown bits 0x{flags:02X}"),
        ));
    }

    Ok(flags & UPDATE_EXISTING_FLAG != 0)
}

fn decode_destination_path(bytes: &[u8]) -> io::Result<String> {
    let path = std::str::from_utf8(bytes)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("receiver destination was not valid UTF-8: {error}"),
            )
        })?
        .to_owned();

    validate_destination_path(&path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    Ok(path)
}

fn validate_destination_path(destination_root: &str) -> io::Result<()> {
    if destination_root.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver destination must not be empty",
        ));
    }

    if destination_root.len() > MAX_DESTINATION_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "receiver destination contains {} bytes, exceeding the {MAX_DESTINATION_PATH_BYTES} byte limit",
                destination_root.len(),
            ),
        ));
    }

    if !Path::new(destination_root).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver destination must be an absolute path",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ManagementJobPhase, ManagementJobRegistry, decode_prepare_request,
        decode_prepared_response, decode_status, encode_prepare_request, encode_prepared_response,
        encode_status,
    };
    use std::fs;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::process;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn prepare_request_round_trips() {
        let encoded = encode_prepare_request(r"C:\Destination", true).unwrap();

        let decoded = decode_prepare_request(&encoded).unwrap();

        assert_eq!(decoded.destination_root, r"C:\Destination",);

        assert!(decoded.update_existing);
    }

    #[test]
    fn registry_prepares_and_cancels_receiver() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let destination = std::env::temp_dir().join(format!(
            "networkcopy-management-job-{}-{unique}",
            process::id(),
        ));

        let destination_text = destination.to_str().unwrap();

        let registry = Arc::new(ManagementJobRegistry::new());

        let job = registry
            .prepare_receive_on(
                destination_text,
                true,
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            )
            .unwrap();

        assert!(registry.is_busy().unwrap());

        let status = registry.status().unwrap();

        assert_eq!(status.phase, ManagementJobPhase::ReceiverPrepared,);

        assert_eq!(status.job_id, Some(job.job_id),);

        assert_eq!(status.transfer_port, Some(job.transfer_port),);

        assert!(job.transfer_port > 0);

        assert!(
            registry
                .prepare_receive_on(
                    destination_text,
                    false,
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0,),),
                )
                .is_err(),
        );

        let cancelled = registry.cancel(job.job_id).unwrap();

        assert_eq!(cancelled, job);

        assert!(!registry.is_busy().unwrap());

        thread::sleep(Duration::from_millis(100));

        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn prepared_response_round_trips() {
        let registry = Arc::new(ManagementJobRegistry::new());

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let destination = std::env::temp_dir().join(format!(
            "networkcopy-management-payload-{}-{unique}",
            process::id(),
        ));

        let job = registry
            .prepare_receive_on(
                destination.to_str().unwrap(),
                false,
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            )
            .unwrap();

        let encoded = encode_prepared_response(&job).unwrap();

        let decoded = decode_prepared_response(&encoded).unwrap();

        assert_eq!(decoded, job);

        registry.cancel(job.job_id).unwrap();

        thread::sleep(Duration::from_millis(100));

        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn idle_status_round_trips() {
        let status = ManagementJobRegistry::new().status().unwrap();

        let encoded = encode_status(&status).unwrap();

        let decoded = decode_status(&encoded).unwrap();

        assert_eq!(decoded, status);
    }
}
