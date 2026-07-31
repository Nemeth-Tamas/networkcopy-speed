use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

const PREPARE_REQUEST_VERSION: u16 = 1;
const PREPARE_REQUEST_HEADER_BYTES: usize = 8;

const JOB_STATUS_VERSION: u16 = 1;
const JOB_STATUS_HEADER_BYTES: usize = 16;

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

            Self::ReceiverPrepared => "receiver prepared",
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

    pub destination_root: String,

    pub update_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementJobStatus {
    pub phase: ManagementJobPhase,

    pub job_id: Option<u64>,

    pub destination_root: Option<String>,

    pub update_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrepareReceiveRequest {
    pub(crate) destination_root: String,

    pub(crate) update_existing: bool,
}

#[derive(Debug, Default)]
struct JobRegistryInner {
    active_receive: Option<PreparedReceiveJob>,
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

    pub(crate) fn prepare_receive(
        &self,
        destination_root: &str,
        update_existing: bool,
    ) -> io::Result<PreparedReceiveJob> {
        validate_destination_path(destination_root)?;

        let mut inner = self.lock_inner()?;

        if let Some(active) = &inner.active_receive {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("management agent already has active job {}", active.job_id,),
            ));
        }

        let path = Path::new(destination_root);

        fs::create_dir_all(path)?;

        let metadata = fs::metadata(path)?;

        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver destination does not identify a directory",
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

            destination_root: destination_root.to_owned(),

            update_existing,
        };

        inner.active_receive = Some(job.clone());

        Ok(job)
    }

    pub(crate) fn status(&self) -> io::Result<ManagementJobStatus> {
        let inner = self.lock_inner()?;

        match &inner.active_receive {
            Some(job) => Ok(ManagementJobStatus {
                phase: ManagementJobPhase::ReceiverPrepared,

                job_id: Some(job.job_id),

                destination_root: Some(job.destination_root.clone()),

                update_existing: job.update_existing,
            }),

            None => Ok(ManagementJobStatus {
                phase: ManagementJobPhase::Idle,

                job_id: None,

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

        let mut inner = self.lock_inner()?;

        let Some(active) = &inner.active_receive else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "management agent has no active job",
            ));
        };

        if active.job_id != job_id {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "management agent has active job {}, not requested job {job_id}",
                    active.job_id,
                ),
            ));
        }

        inner.active_receive.take().ok_or_else(|| {
            io::Error::other("active management job disappeared during cancellation")
        })
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
    let (job_id, destination_root) = match status.phase {
        ManagementJobPhase::Idle => {
            if status.job_id.is_some()
                || status.destination_root.is_some()
                || status.update_existing
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "idle management job status contained active-job fields",
                ));
            }

            (0_u64, "")
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

            let destination_root = status.destination_root.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "prepared receiver status omitted its destination",
                )
            })?;

            validate_destination_path(destination_root)?;

            (job_id, destination_root)
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

    let path_length = usize::try_from(u32::from_le_bytes(payload[12..16].try_into().map_err(
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
            if job_id != 0 || path_length != 0 || update_existing {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "idle management job status contained active-job fields",
                ));
            }

            Ok(ManagementJobStatus {
                phase,

                job_id: None,

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

            let destination_root = decode_destination_path(&payload[JOB_STATUS_HEADER_BYTES..])?;

            Ok(ManagementJobStatus {
                phase,

                job_id: Some(job_id),

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
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

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

        let registry = ManagementJobRegistry::new();

        let job = registry.prepare_receive(destination_text, true).unwrap();

        assert!(registry.is_busy().unwrap());

        let status = registry.status().unwrap();

        assert_eq!(status.phase, ManagementJobPhase::ReceiverPrepared,);

        assert_eq!(status.job_id, Some(job.job_id),);

        assert!(registry.prepare_receive(destination_text, false,).is_err(),);

        let cancelled = registry.cancel(job.job_id).unwrap();

        assert_eq!(cancelled, job);

        assert!(!registry.is_busy().unwrap());

        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn prepared_response_round_trips() {
        let registry = ManagementJobRegistry::new();

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let destination = std::env::temp_dir().join(format!(
            "networkcopy-management-payload-{}-{unique}",
            process::id(),
        ));

        let job = registry
            .prepare_receive(destination.to_str().unwrap(), false)
            .unwrap();

        let encoded = encode_prepared_response(&job).unwrap();

        let decoded = decode_prepared_response(&encoded).unwrap();

        assert_eq!(decoded, job);

        registry.cancel(job.job_id).unwrap();

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
