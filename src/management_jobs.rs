use crate::calibrated_transfer;
use crate::console_progress::{ProgressCounter, ProgressSnapshot};
use crate::destination_layout::DestinationLayout;
use crate::management_instance::AgentInstanceId;
use crate::management_snapshot::{
    ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
    ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
};
use crate::manifest_scan;
use crate::multistream_copy::{self, DestinationMode, MultistreamCopyReport, ReceiveReport};
use crate::network_calibration;
use crate::resume_state::ResumeJournal;
use crate::windows_desktop_layout;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

const PREPARE_REQUEST_VERSION: u16 = 1;
const PREPARE_REQUEST_HEADER_BYTES: usize = 8;

const START_SEND_REQUEST_VERSION_V1: u16 = 1;

const START_SEND_REQUEST_VERSION_V2: u16 = 2;

const START_SEND_REQUEST_VERSION: u16 = 3;

const START_SEND_REQUEST_HEADER_BYTES_V1: usize = 24;

const START_SEND_REQUEST_HEADER_BYTES: usize = 28;

const PRESERVE_DESKTOP_LAYOUT_FLAG: u16 = 0x0001;

const KNOWN_START_SEND_FLAGS: u16 = PRESERVE_DESKTOP_LAYOUT_FLAG;

const START_SEND_RESPONSE_VERSION: u16 = 1;
const START_SEND_RESPONSE_HEADER_BYTES: usize = 12;

const JOB_STATUS_VERSION: u16 = 3;
const JOB_STATUS_HEADER_BYTES: usize = 20;

const CANCEL_REQUEST_VERSION: u16 = 1;
const CANCEL_REQUEST_BYTES: usize = 12;

const UPDATE_EXISTING_FLAG: u8 = 0x01;
const RESUME_TRANSFER_FLAG: u8 = 0x02;

const KNOWN_JOB_FLAGS: u8 = UPDATE_EXISTING_FLAG;

const KNOWN_PREPARE_FLAGS: u8 = UPDATE_EXISTING_FLAG | RESUME_TRANSFER_FLAG;

const MAX_DESTINATION_PATH_BYTES: usize = 32 * 1024;
const MAX_SOURCE_PATH_BYTES: usize = 32 * 1024;
const MAX_RECEIVER_ENDPOINT_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManagementJobPhase {
    Idle = 0,
    ReceiverPrepared = 1,
    SenderRunning = 2,
}

impl ManagementJobPhase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",

            Self::ReceiverPrepared => "receiver waiting for sender",

            Self::SenderRunning => "sender running",
        }
    }
}

impl TryFrom<u8> for ManagementJobPhase {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Idle),

            1 => Ok(Self::ReceiverPrepared),

            2 => Ok(Self::SenderRunning),

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
pub struct StartedSendJob {
    pub job_id: u64,

    pub receiver_address: SocketAddr,

    pub source_root: String,

    pub worker_count: usize,

    pub calibration_mib: u64,
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

    pub(crate) resume: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartSendRequest {
    pub(crate) receiver_address: SocketAddr,

    pub(crate) source_root: String,

    pub(crate) worker_count: usize,

    pub(crate) calibration_mib: u64,

    pub(crate) forced_data_stream_count: Option<usize>,

    pub(crate) preserve_desktop_layout: bool,
}

#[derive(Clone, Debug)]
struct ActiveReceiveJob {
    job: PreparedReceiveJob,

    progress: ProgressCounter,
}

#[derive(Clone, Debug)]
struct ActiveSendJob {
    job: StartedSendJob,

    progress: ProgressCounter,
}

#[derive(Debug, Default)]
struct JobRegistryInner {
    active_receive: Option<ActiveReceiveJob>,

    active_send: Option<ActiveSendJob>,

    latest_result: Option<ManagementJobResult>,
}

fn active_job_id(inner: &JobRegistryInner) -> Option<u64> {
    inner
        .active_receive
        .as_ref()
        .map(|active| active.job.job_id)
        .or_else(|| inner.active_send.as_ref().map(|active| active.job.job_id))
}

#[derive(Debug)]
pub(crate) struct ManagementJobRegistry {
    instance_id: AgentInstanceId,

    next_job_id: AtomicU64,

    inner: Mutex<JobRegistryInner>,
}

impl ManagementJobRegistry {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            instance_id: AgentInstanceId::generate()?,

            next_job_id: AtomicU64::new(1),

            inner: Mutex::new(JobRegistryInner::default()),
        })
    }

    pub(crate) const fn instance_id(&self) -> AgentInstanceId {
        self.instance_id
    }

    pub(crate) fn is_busy(&self) -> io::Result<bool> {
        let inner = self.lock_inner()?;

        Ok(active_job_id(&inner).is_some())
    }

    pub(crate) fn prepare_receive_on(
        self: &Arc<Self>,
        destination_root: &str,
        update_existing: bool,
        bind_address: SocketAddr,
    ) -> io::Result<PreparedReceiveJob> {
        self.prepare_receive_on_configured(destination_root, update_existing, bind_address, false)
    }

    pub(crate) fn prepare_receive_resume_on(
        self: &Arc<Self>,
        destination_root: &str,
        update_existing: bool,
        bind_address: SocketAddr,
    ) -> io::Result<PreparedReceiveJob> {
        self.prepare_receive_on_configured(destination_root, update_existing, bind_address, true)
    }

    fn prepare_receive_on_configured(
        self: &Arc<Self>,
        destination_root: &str,
        update_existing: bool,
        bind_address: SocketAddr,
        resume: bool,
    ) -> io::Result<PreparedReceiveJob> {
        validate_destination_path(destination_root)?;

        {
            let inner = self.lock_inner()?;

            if let Some(existing_job_id) = active_job_id(&inner) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("management agent already has active job {existing_job_id}"),
                ));
            }
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

            if let Some(existing_job_id) = active_job_id(&inner) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("management agent already has active job {existing_job_id}"),
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
                if resume {
                    worker_progress.set_label("Waiting for resumed transfer");

                    worker_progress.set_completed(0);
                    worker_progress.set_total(0);

                    let result =
                        multistream_copy::receive_on_listener_with_progress_mode_and_layout(
                            &listener,
                            &worker_destination,
                            worker_progress,
                            destination_mode,
                            DestinationLayout::Exact,
                        );

                    let terminal_result =
                        build_resumed_receive_result(job_id, &worker_destination, &result);

                    if let Err(error) = worker_registry.finish_receive(job_id, terminal_result) {
                        eprintln!(
                            "failed to finalize managed resumed \
                             receiver job {job_id}: {error}"
                        );
                    }

                    match result {
                        Ok(report) => {
                            println!(
                                "Managed resumed receiver job \
                                 {job_id} complete"
                            );

                            println!("  Files received: {}", report.files_received,);

                            println!("  Bytes received: {}", report.bytes_received,);

                            println!("  Data streams: {}", report.data_stream_count,);
                        }

                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            println!(
                                "Managed resumed receiver job \
                                 {job_id} cancelled"
                            );
                        }

                        Err(error) => {
                            eprintln!(
                                "Managed resumed receiver job \
                                 {job_id} failed: {error}"
                            );
                        }
                    }

                    return;
                }

                let result = calibrated_transfer::receive_once_with_progress_and_mode(
                    listener,
                    &worker_destination,
                    worker_progress,
                    destination_mode,
                );

                let terminal_result = build_receive_result(job_id, &worker_destination, &result);

                if let Err(error) = worker_registry.finish_receive(job_id, terminal_result) {
                    eprintln!(
                        "failed to finalize managed receiver job \
                         {job_id}: {error}"
                    );
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
                        eprintln!(
                            "Managed receiver job {job_id} failed: \
                             {error}"
                        );
                    }
                }
            });

        if let Err(error) = spawn_result {
            let _ = self.clear_receive(job_id);

            return Err(error);
        }

        Ok(job)
    }

    pub(crate) fn start_send_with_desktop_layout(
        self: &Arc<Self>,
        receiver_address: SocketAddr,
        source_root: &str,
        worker_count: usize,
        calibration_mib: u64,
        forced_data_stream_count: Option<usize>,
        preserve_desktop_layout: bool,
    ) -> io::Result<StartedSendJob> {
        validate_send_parameters(receiver_address, source_root, worker_count, calibration_mib)?;

        validate_forced_data_stream_count(forced_data_stream_count)?;

        let calibration_bytes = network_calibration::bytes_from_mib(calibration_mib)?;

        {
            let inner = self.lock_inner()?;

            if let Some(existing_job_id) = active_job_id(&inner) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("management agent already has active job {existing_job_id}"),
                ));
            }
        }

        let source_path = Path::new(source_root);

        let metadata = fs::metadata(source_path)?;

        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sender source does not identify a directory",
            ));
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);

        if job_id == 0 {
            return Err(io::Error::other(
                "management job ID counter wrapped to zero",
            ));
        }

        let job = StartedSendJob {
            job_id,

            receiver_address,

            source_root: source_root.to_owned(),

            worker_count,

            calibration_mib,
        };

        let progress = ProgressCounter::new("Starting managed sender", 0);

        {
            let mut inner = self.lock_inner()?;

            if let Some(existing_job_id) = active_job_id(&inner) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("management agent already has active job {existing_job_id}"),
                ));
            }

            inner.active_send = Some(ActiveSendJob {
                job: job.clone(),

                progress: progress.clone(),
            });
        }

        let worker_registry = Arc::clone(self);

        let worker_progress = progress;

        let worker_source = PathBuf::from(source_root);

        let spawn_result = thread::Builder::new()
            .name(format!("networkcopy-managed-sender-{job_id}"))
            .spawn(move || {
                let desktop_layout = if preserve_desktop_layout {
                    worker_progress.set_label("Capturing Desktop layout");

                    match windows_desktop_layout::is_current_desktop_path(&worker_source) {
                        Ok(true) => {
                            match windows_desktop_layout::capture_current_desktop_layout() {
                                Ok(snapshot) => Some(snapshot),

                                Err(error) => {
                                    eprintln!(
                                        "managed Desktop layout capture failed; continuing without layout metadata: {error}",
                                    );

                                    None
                                }
                            }
                        }

                        Ok(false) => {
                            eprintln!(
                                "managed Desktop layout preservation was requested for a non-Desktop source; continuing without layout metadata",
                            );

                            None
                        }

                        Err(error) => {
                            eprintln!(
                                "failed to verify managed Desktop source; continuing without layout metadata: {error}",
                            );

                            None
                        }
                    }
                } else {
                    None
                };

                match forced_data_stream_count {
                    Some(data_stream_count) => {
                        worker_progress.set_label(
                            format!(
                                "Scanning source - resuming with \
                                 {data_stream_count} streams"
                            ),
                        );

                        worker_progress.set_completed(0);
                        worker_progress.set_total(0);

                        let result =
                            multistream_copy::send_with_progress(
                                receiver_address,
                                &worker_source,
                                worker_count,
                                data_stream_count,
                                worker_progress,
                                desktop_layout,
                            );

                        let terminal_result =
                            build_resumed_send_result(
                                job_id,
                                &result,
                            );

                        if let Err(error) =
                            worker_registry.finish_send(
                                job_id,
                                terminal_result,
                            )
                        {
                            eprintln!(
                                "failed to finalize managed resumed \
                                 sender job {job_id}: {error}"
                            );
                        }

                        match result {
                            Ok(report) => {
                                println!(
                                    "Managed resumed sender job \
                                     {job_id} complete"
                                );

                                println!(
                                    "  Files sent: {}",
                                    report.files_copied,
                                );

                                println!(
                                    "  Bytes sent: {}",
                                    report.bytes_copied,
                                );

                                println!(
                                    "  Data streams: {}",
                                    report.data_stream_count,
                                );

                                println!(
                                    "  Wire bytes: {}",
                                    report.data_wire_bytes,
                                );
                            }

                            Err(error)
                                if error.kind()
                                    == io::ErrorKind::Interrupted =>
                            {
                                println!(
                                    "Managed resumed sender job \
                                     {job_id} cancelled"
                                );
                            }

                            Err(error) => {
                                eprintln!(
                                    "Managed resumed sender job \
                                     {job_id} failed: {error}"
                                );
                            }
                        }
                    }

                    None => {
                        let result =
                            calibrated_transfer::
                                send_with_progress_and_stream_count(
                                    receiver_address,
                                    &worker_source,
                                    worker_count,
                                    calibration_bytes,
                                    worker_progress,
                                    None,
                                    desktop_layout,
                                );

                        let terminal_result =
                            build_send_result(
                                job_id,
                                &result,
                            );

                        if let Err(error) =
                            worker_registry.finish_send(
                                job_id,
                                terminal_result,
                            )
                        {
                            eprintln!(
                                "failed to finalize managed sender job \
                                 {job_id}: {error}"
                            );
                        }

                        match result {
                            Ok(report) => {
                                println!(
                                    "Managed sender job {job_id} complete"
                                );

                                println!(
                                    "  Files sent: {}",
                                    report.transfer.files_copied,
                                );

                                println!(
                                    "  Bytes sent: {}",
                                    report.transfer.bytes_copied,
                                );

                                println!(
                                    "  Data streams: {}",
                                    report.transfer.data_stream_count,
                                );
                            }

                            Err(error)
                                if error.kind()
                                    == io::ErrorKind::Interrupted =>
                            {
                                println!(
                                    "Managed sender job {job_id} cancelled"
                                );
                            }

                            Err(error) => {
                                eprintln!(
                                    "Managed sender job {job_id} failed: \
                                     {error}"
                                );
                            }
                        }
                    }
                }
            });

        if let Err(error) = spawn_result {
            let _ = self.clear_send(job_id);

            return Err(error);
        }

        Ok(job)
    }

    pub(crate) fn snapshot(&self) -> io::Result<ManagementAgentSnapshot> {
        let inner = self.lock_inner()?;

        let active = if let Some(active) = &inner.active_receive {
            Some(build_active_snapshot(
                ManagementJobRole::Receiver,
                active.job.job_id,
                &active.progress,
                ManagementActiveJobDetails::Receiver {
                    transfer_port: active.job.transfer_port,

                    destination_root: active.job.destination_root.clone(),

                    update_existing: active.job.update_existing,
                },
            ))
        } else {
            inner.active_send.as_ref().map(|active| {
                build_active_snapshot(
                    ManagementJobRole::Sender,
                    active.job.job_id,
                    &active.progress,
                    ManagementActiveJobDetails::Sender {
                        receiver_address: active.job.receiver_address,

                        source_root: active.job.source_root.clone(),

                        worker_count: active.job.worker_count,

                        calibration_mib: active.job.calibration_mib,
                    },
                )
            })
        };

        Ok(ManagementAgentSnapshot {
            agent_instance_id: self.instance_id,

            active,

            latest_result: inner.latest_result.clone(),
        })
    }

    pub(crate) fn status(&self) -> io::Result<ManagementJobStatus> {
        let inner = self.lock_inner()?;

        if let Some(active) = &inner.active_receive {
            let job = &active.job;

            return Ok(ManagementJobStatus {
                phase: ManagementJobPhase::ReceiverPrepared,

                job_id: Some(job.job_id),

                transfer_port: Some(job.transfer_port),

                destination_root: Some(job.destination_root.clone()),

                update_existing: job.update_existing,
            });
        }

        if let Some(active) = &inner.active_send {
            return Ok(ManagementJobStatus {
                phase: ManagementJobPhase::SenderRunning,

                job_id: Some(active.job.job_id),

                transfer_port: None,

                destination_root: None,

                update_existing: false,
            });
        }

        Ok(ManagementJobStatus {
            phase: ManagementJobPhase::Idle,

            job_id: None,

            transfer_port: None,

            destination_root: None,

            update_existing: false,
        })
    }

    pub(crate) fn cancel(&self, job_id: u64) -> io::Result<u64> {
        if job_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "management job ID must not be zero",
            ));
        }

        let (progress, role, resume_destination) = {
            let mut inner = self.lock_inner()?;

            let existing_job_id = active_job_id(&inner).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "management agent has no active job",
                )
            })?;

            if existing_job_id != job_id {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "management agent has active job {existing_job_id}, not requested job {job_id}"
                    ),
                ));
            }

            if let Some(active) = inner.active_receive.take() {
                (
                    active.progress,
                    ManagementJobRole::Receiver,
                    Some(PathBuf::from(active.job.destination_root)),
                )
            } else if let Some(active) = inner.active_send.take() {
                (active.progress, ManagementJobRole::Sender, None)
            } else {
                return Err(io::Error::other(
                    "active management job disappeared during cancellation",
                ));
            }
        };

        progress.cancel();

        let data_stream_count = resume_destination.as_deref().map_or(0, resume_stream_count);

        {
            let mut inner = self.lock_inner()?;

            inner.latest_result = Some(ManagementJobResult {
                role,

                outcome: ManagementJobOutcome::Cancelled,

                job_id,

                files: 0,

                logical_bytes: 0,

                wire_bytes: 0,

                data_stream_count,

                message: "cancelled by management request".to_string(),
            });
        }

        Ok(job_id)
    }

    fn finish_receive(&self, job_id: u64, result: ManagementJobResult) -> io::Result<()> {
        let mut inner = self.lock_inner()?;

        let matches_job = inner
            .active_receive
            .as_ref()
            .is_some_and(|active| active.job.job_id == job_id);

        if matches_job {
            inner.active_receive.take();

            inner.latest_result = Some(result);
        }

        Ok(())
    }

    fn finish_send(&self, job_id: u64, result: ManagementJobResult) -> io::Result<()> {
        let mut inner = self.lock_inner()?;

        let matches_job = inner
            .active_send
            .as_ref()
            .is_some_and(|active| active.job.job_id == job_id);

        if matches_job {
            inner.active_send.take();

            inner.latest_result = Some(result);
        }

        Ok(())
    }

    fn clear_receive(&self, job_id: u64) -> io::Result<()> {
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

    fn clear_send(&self, job_id: u64) -> io::Result<()> {
        let mut inner = self.lock_inner()?;

        let matches_job = inner
            .active_send
            .as_ref()
            .is_some_and(|active| active.job.job_id == job_id);

        if matches_job {
            inner.active_send.take();
        }

        Ok(())
    }

    fn lock_inner(&self) -> io::Result<MutexGuard<'_, JobRegistryInner>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("management job registry lock was poisoned"))
    }
}

fn build_active_snapshot(
    role: ManagementJobRole,
    job_id: u64,
    progress: &ProgressCounter,
    details: ManagementActiveJobDetails,
) -> ManagementActiveJobSnapshot {
    let ProgressSnapshot {
        label,
        completed,
        total,
    } = progress.snapshot();

    ManagementActiveJobSnapshot {
        role,

        job_id,

        phase: label,

        completed,

        total,

        cancel_requested: progress.is_cancelled(),

        details,
    }
}

fn build_receive_result(
    job_id: u64,
    destination_root: &Path,
    result: &io::Result<calibrated_transfer::CalibratedReceiveReport>,
) -> ManagementJobResult {
    match result {
        Ok(report) => ManagementJobResult {
            role: ManagementJobRole::Receiver,

            outcome: ManagementJobOutcome::Completed,

            job_id,

            files: report.transfer.files_received,

            logical_bytes: report.transfer.bytes_received,

            wire_bytes: 0,

            data_stream_count: stream_count_u32(report.transfer.data_stream_count),

            message: String::new(),
        },

        Err(error) => {
            let mut result = build_error_result(ManagementJobRole::Receiver, job_id, error);

            result.data_stream_count = resume_stream_count(destination_root);

            result
        }
    }
}

fn build_send_result(
    job_id: u64,
    result: &io::Result<calibrated_transfer::CalibratedSendReport>,
) -> ManagementJobResult {
    match result {
        Ok(report) => ManagementJobResult {
            role: ManagementJobRole::Sender,

            outcome: ManagementJobOutcome::Completed,

            job_id,

            files: report.transfer.files_copied,

            logical_bytes: report.transfer.bytes_copied,

            wire_bytes: report.transfer.data_wire_bytes,

            data_stream_count: stream_count_u32(report.transfer.data_stream_count),

            message: String::new(),
        },

        Err(error) => build_error_result(ManagementJobRole::Sender, job_id, error),
    }
}

fn build_resumed_receive_result(
    job_id: u64,
    destination_root: &Path,
    result: &io::Result<ReceiveReport>,
) -> ManagementJobResult {
    match result {
        Ok(report) => ManagementJobResult {
            role: ManagementJobRole::Receiver,

            outcome: ManagementJobOutcome::Completed,

            job_id,

            files: report.files_received,

            logical_bytes: report.bytes_received,

            wire_bytes: 0,

            data_stream_count: stream_count_u32(report.data_stream_count),

            message: String::new(),
        },

        Err(error) => {
            let mut result = build_error_result(ManagementJobRole::Receiver, job_id, error);

            result.data_stream_count = resume_stream_count(destination_root);

            result
        }
    }
}

fn build_resumed_send_result(
    job_id: u64,
    result: &io::Result<MultistreamCopyReport>,
) -> ManagementJobResult {
    match result {
        Ok(report) => ManagementJobResult {
            role: ManagementJobRole::Sender,

            outcome: ManagementJobOutcome::Completed,

            job_id,

            files: report.files_copied,

            logical_bytes: report.bytes_copied,

            wire_bytes: report.data_wire_bytes,

            data_stream_count: stream_count_u32(report.data_stream_count),

            message: String::new(),
        },

        Err(error) => build_error_result(ManagementJobRole::Sender, job_id, error),
    }
}

fn build_error_result(
    role: ManagementJobRole,
    job_id: u64,
    error: &io::Error,
) -> ManagementJobResult {
    let outcome = if error.kind() == io::ErrorKind::Interrupted {
        ManagementJobOutcome::Cancelled
    } else {
        ManagementJobOutcome::Failed
    };

    ManagementJobResult {
        role,

        outcome,

        job_id,

        files: 0,

        logical_bytes: 0,

        wire_bytes: 0,

        data_stream_count: 0,

        message: error.to_string(),
    }
}

fn resume_stream_count(destination_root: &Path) -> u32 {
    ResumeJournal::stored_data_stream_count(destination_root)
        .ok()
        .flatten()
        .map_or(0, stream_count_u32)
}

fn stream_count_u32(stream_count: usize) -> u32 {
    u32::try_from(stream_count).unwrap_or(u32::MAX)
}

pub(crate) fn encode_prepare_request(
    destination_root: &str,
    update_existing: bool,
) -> io::Result<Vec<u8>> {
    encode_prepare_request_with_resume(destination_root, update_existing, false)
}

pub(crate) fn encode_prepare_request_with_resume(
    destination_root: &str,
    update_existing: bool,
    resume: bool,
) -> io::Result<Vec<u8>> {
    validate_destination_path(destination_root)?;

    let path_length = u32::try_from(destination_root.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver destination length \
                 cannot be represented",
        )
    })?;

    let total_length = PREPARE_REQUEST_HEADER_BYTES
        .checked_add(destination_root.len())
        .ok_or_else(|| {
            io::Error::other(
                "prepare-receive request \
                     length overflowed",
            )
        })?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&PREPARE_REQUEST_VERSION.to_le_bytes());

    payload.push(encode_prepare_flags(update_existing, resume));

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

    let (update_existing, resume) = decode_prepare_flags(payload[2])?;

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

        resume,
    })
}

pub(crate) fn encode_start_send_request(
    receiver_address: SocketAddr,
    source_root: &str,
    worker_count: usize,
    calibration_mib: u64,
) -> io::Result<Vec<u8>> {
    encode_start_send_request_with_stream_count(
        receiver_address,
        source_root,
        worker_count,
        calibration_mib,
        None,
    )
}

pub(crate) fn encode_start_send_request_with_stream_count(
    receiver_address: SocketAddr,
    source_root: &str,
    worker_count: usize,
    calibration_mib: u64,
    forced_data_stream_count: Option<usize>,
) -> io::Result<Vec<u8>> {
    encode_start_send_request_with_stream_count_and_desktop_layout(
        receiver_address,
        source_root,
        worker_count,
        calibration_mib,
        forced_data_stream_count,
        false,
    )
}

pub(crate) fn encode_start_send_request_with_stream_count_and_desktop_layout(
    receiver_address: SocketAddr,
    source_root: &str,
    worker_count: usize,
    calibration_mib: u64,
    forced_data_stream_count: Option<usize>,
    preserve_desktop_layout: bool,
) -> io::Result<Vec<u8>> {
    validate_send_parameters(receiver_address, source_root, worker_count, calibration_mib)?;

    validate_forced_data_stream_count(forced_data_stream_count)?;

    let receiver_text = receiver_address.to_string();

    let receiver_length = u16::try_from(receiver_text.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver endpoint length cannot be represented",
        )
    })?;

    let source_length = u32::try_from(source_root.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender source length cannot be represented",
        )
    })?;

    let worker_count = u32::try_from(worker_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender worker count cannot be represented",
        )
    })?;

    let forced_data_stream_count = match forced_data_stream_count {
        Some(data_stream_count) => u32::try_from(data_stream_count).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "forced data stream count cannot be represented",
            )
        })?,

        None => 0,
    };

    let total_length = START_SEND_REQUEST_HEADER_BYTES
        .checked_add(receiver_text.len())
        .and_then(|length| length.checked_add(source_root.len()))
        .ok_or_else(|| io::Error::other("start-send request length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&START_SEND_REQUEST_VERSION.to_le_bytes());

    let flags = if preserve_desktop_layout {
        PRESERVE_DESKTOP_LAYOUT_FLAG
    } else {
        0
    };

    payload.extend_from_slice(&flags.to_le_bytes());

    payload.extend_from_slice(&worker_count.to_le_bytes());

    payload.extend_from_slice(&calibration_mib.to_le_bytes());

    payload.extend_from_slice(&receiver_length.to_le_bytes());

    payload.extend_from_slice(&0_u16.to_le_bytes());

    payload.extend_from_slice(&source_length.to_le_bytes());

    payload.extend_from_slice(&forced_data_stream_count.to_le_bytes());

    payload.extend_from_slice(receiver_text.as_bytes());

    payload.extend_from_slice(source_root.as_bytes());

    Ok(payload)
}

pub(crate) fn decode_start_send_request(payload: &[u8]) -> io::Result<StartSendRequest> {
    if payload.len() < START_SEND_REQUEST_HEADER_BYTES_V1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "start-send request has {} bytes, expected at least {START_SEND_REQUEST_HEADER_BYTES_V1}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send version was malformed",
        )
    })?);

    let flags = u16::from_le_bytes(payload[2..4].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send flags field was malformed",
        )
    })?);

    let decode_forced_stream_count = |payload: &[u8]| -> io::Result<Option<usize>> {
        let encoded = u32::from_le_bytes(payload[24..28].try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "forced data stream count was malformed",
            )
        })?);

        if encoded == 0 {
            Ok(None)
        } else {
            usize::try_from(encoded).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "forced data stream count cannot be represented",
                )
            })
        }
    };

    let (header_bytes, forced_data_stream_count, preserve_desktop_layout) = match version {
        START_SEND_REQUEST_VERSION_V1 => {
            if flags != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "start-send v1 reserved field was not zero",
                ));
            }

            (START_SEND_REQUEST_HEADER_BYTES_V1, None, false)
        }

        START_SEND_REQUEST_VERSION_V2 => {
            if payload.len() < START_SEND_REQUEST_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "start-send v2 request has {} bytes, expected at least {START_SEND_REQUEST_HEADER_BYTES}",
                        payload.len(),
                    ),
                ));
            }

            if flags != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "start-send v2 reserved field was not zero",
                ));
            }

            (
                START_SEND_REQUEST_HEADER_BYTES,
                decode_forced_stream_count(payload)?,
                false,
            )
        }

        START_SEND_REQUEST_VERSION => {
            if payload.len() < START_SEND_REQUEST_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "start-send v3 request has {} bytes, expected at least {START_SEND_REQUEST_HEADER_BYTES}",
                        payload.len(),
                    ),
                ));
            }

            let unknown_flags = flags & !KNOWN_START_SEND_FLAGS;

            if unknown_flags != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("start-send request contains unknown flags 0x{unknown_flags:04X}",),
                ));
            }

            (
                START_SEND_REQUEST_HEADER_BYTES,
                decode_forced_stream_count(payload)?,
                flags & PRESERVE_DESKTOP_LAYOUT_FLAG != 0,
            )
        }

        unknown => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported start-send request version {unknown}",),
            ));
        }
    };

    let worker_count = usize::try_from(u32::from_le_bytes(payload[4..8].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "start-send worker count was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send worker count cannot be represented",
        )
    })?;

    let calibration_mib = u64::from_le_bytes(payload[8..16].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send calibration size was malformed",
        )
    })?);

    let receiver_length = usize::from(u16::from_le_bytes(payload[16..18].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "start-send receiver length was malformed",
            )
        },
    )?));

    let second_reserved = u16::from_le_bytes(payload[18..20].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send secondary reserved field was malformed",
        )
    })?);

    if second_reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send secondary reserved field was not zero",
        ));
    }

    let source_length = usize::try_from(u32::from_le_bytes(payload[20..24].try_into().map_err(
        |_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "start-send source length was malformed",
            )
        },
    )?))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send source length cannot be represented",
        )
    })?;

    let receiver_end = header_bytes.checked_add(receiver_length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send receiver position overflowed",
        )
    })?;

    let expected_length = receiver_end.checked_add(source_length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send request length overflowed",
        )
    })?;

    if payload.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "start-send request has {} bytes, expected {expected_length}",
                payload.len(),
            ),
        ));
    }

    let receiver_text =
        std::str::from_utf8(&payload[header_bytes..receiver_end]).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("receiver endpoint was not valid UTF-8: {error}"),
            )
        })?;

    if receiver_text.is_empty() || receiver_text.len() > MAX_RECEIVER_ENDPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver endpoint length was invalid",
        ));
    }

    let receiver_address = receiver_text.parse::<SocketAddr>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("receiver endpoint was invalid: {error}"),
        )
    })?;

    let source_root = std::str::from_utf8(&payload[receiver_end..])
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sender source was not valid UTF-8: {error}"),
            )
        })?
        .to_owned();

    validate_send_parameters(
        receiver_address,
        &source_root,
        worker_count,
        calibration_mib,
    )
    .and_then(|()| validate_forced_data_stream_count(forced_data_stream_count))
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    Ok(StartSendRequest {
        receiver_address,
        source_root,
        worker_count,
        calibration_mib,
        forced_data_stream_count,
        preserve_desktop_layout,
    })
}

pub(crate) fn encode_started_send_response(job: &StartedSendJob) -> io::Result<Vec<u8>> {
    if job.job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "started sender response used job ID zero",
        ));
    }

    let request = encode_start_send_request(
        job.receiver_address,
        &job.source_root,
        job.worker_count,
        job.calibration_mib,
    )?;

    let total_length = START_SEND_RESPONSE_HEADER_BYTES
        .checked_add(request.len())
        .ok_or_else(|| io::Error::other("start-send response length overflowed"))?;

    let mut payload = Vec::with_capacity(total_length);

    payload.extend_from_slice(&START_SEND_RESPONSE_VERSION.to_le_bytes());

    payload.extend_from_slice(&0_u16.to_le_bytes());

    payload.extend_from_slice(&job.job_id.to_le_bytes());

    payload.extend_from_slice(&request);

    Ok(payload)
}

pub(crate) fn decode_started_send_response(payload: &[u8]) -> io::Result<StartedSendJob> {
    if payload.len() < START_SEND_RESPONSE_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "start-send response has {} bytes, expected at least {START_SEND_RESPONSE_HEADER_BYTES}",
                payload.len(),
            ),
        ));
    }

    let version = u16::from_le_bytes(payload[0..2].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send response version was malformed",
        )
    })?);

    if version != START_SEND_RESPONSE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported start-send response version {version}"),
        ));
    }

    let reserved = u16::from_le_bytes(payload[2..4].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send response reserved field was malformed",
        )
    })?);

    if reserved != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send response reserved field was not zero",
        ));
    }

    let job_id = u64::from_le_bytes(payload[4..12].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send response job ID was malformed",
        )
    })?);

    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "start-send response used job ID zero",
        ));
    }

    let request = decode_start_send_request(&payload[START_SEND_RESPONSE_HEADER_BYTES..])?;

    Ok(StartedSendJob {
        job_id,

        receiver_address: request.receiver_address,

        source_root: request.source_root,

        worker_count: request.worker_count,

        calibration_mib: request.calibration_mib,
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

        ManagementJobPhase::SenderRunning => {
            let job_id = status.job_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sender status omitted its job ID",
                )
            })?;

            if job_id == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sender status used job ID zero",
                ));
            }

            if status.transfer_port.is_some()
                || status.destination_root.is_some()
                || status.update_existing
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sender status contained receiver-only fields",
                ));
            }

            (job_id, 0_u16, "")
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

        ManagementJobPhase::SenderRunning => {
            if job_id == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sender status used job ID zero",
                ));
            }

            if transfer_port != 0 || path_length != 0 || update_existing {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sender status contained receiver-only fields",
                ));
            }

            Ok(ManagementJobStatus {
                phase,

                job_id: Some(job_id),

                transfer_port: None,

                destination_root: None,

                update_existing: false,
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

fn encode_prepare_flags(update_existing: bool, resume: bool) -> u8 {
    let mut flags = encode_job_flags(update_existing);

    if resume {
        flags |= RESUME_TRANSFER_FLAG;
    }

    flags
}

fn decode_prepare_flags(flags: u8) -> io::Result<(bool, bool)> {
    if flags & !KNOWN_PREPARE_FLAGS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "prepare-receive flags contained \
                 unknown bits 0x{flags:02X}",
            ),
        ));
    }

    Ok((
        flags & UPDATE_EXISTING_FLAG != 0,
        flags & RESUME_TRANSFER_FLAG != 0,
    ))
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

fn validate_forced_data_stream_count(forced_data_stream_count: Option<usize>) -> io::Result<()> {
    if let Some(data_stream_count) = forced_data_stream_count {
        network_calibration::validate_matrix_stream_count(data_stream_count)?;
    }

    Ok(())
}

fn validate_send_parameters(
    receiver_address: SocketAddr,
    source_root: &str,
    worker_count: usize,
    calibration_mib: u64,
) -> io::Result<()> {
    if receiver_address.port() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver endpoint must not use port zero",
        ));
    }

    let receiver_text = receiver_address.to_string();

    if receiver_text.len() > MAX_RECEIVER_ENDPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "receiver endpoint contains {} bytes, exceeding the {MAX_RECEIVER_ENDPOINT_BYTES} byte limit",
                receiver_text.len(),
            ),
        ));
    }

    validate_source_path(source_root)?;

    manifest_scan::validate_worker_count(worker_count)?;

    network_calibration::bytes_from_mib(calibration_mib)?;

    Ok(())
}

fn validate_source_path(source_root: &str) -> io::Result<()> {
    if source_root.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender source must not be empty",
        ));
    }

    if source_root.len() > MAX_SOURCE_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "sender source contains {} bytes, exceeding the {MAX_SOURCE_PATH_BYTES} byte limit",
                source_root.len(),
            ),
        ));
    }

    if !Path::new(source_root).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender source must be an absolute path",
        ));
    }

    Ok(())
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
        ManagementJobPhase, ManagementJobRegistry, ManagementJobStatus, StartedSendJob,
        decode_prepare_request, decode_prepared_response, decode_start_send_request,
        decode_started_send_response, decode_status, encode_prepare_request,
        encode_prepare_request_with_resume, encode_prepared_response, encode_start_send_request,
        encode_start_send_request_with_stream_count,
        encode_start_send_request_with_stream_count_and_desktop_layout,
        encode_started_send_response, encode_status,
    };
    use crate::management_snapshot::{ManagementJobOutcome, ManagementJobRole};
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
        assert!(!decoded.resume);
    }

    #[test]
    fn resumed_prepare_request_round_trips() {
        let encoded = encode_prepare_request_with_resume(r"C:\Destination", false, true).unwrap();

        let decoded = decode_prepare_request(&encoded).unwrap();

        assert_eq!(decoded.destination_root, r"C:\Destination",);

        assert!(!decoded.update_existing);

        assert!(decoded.resume);
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

        let registry = Arc::new(ManagementJobRegistry::new().unwrap());

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

        assert_eq!(cancelled, job.job_id,);

        assert!(!registry.is_busy().unwrap());

        let snapshot = registry.snapshot().unwrap();

        assert!(snapshot.active.is_none());

        let result = snapshot.latest_result.expect("cancelled receiver result");

        assert_eq!(result.job_id, job.job_id,);

        assert_eq!(result.role, ManagementJobRole::Receiver,);

        assert_eq!(result.outcome, ManagementJobOutcome::Cancelled,);

        thread::sleep(Duration::from_millis(100));

        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn prepared_response_round_trips() {
        let registry = Arc::new(ManagementJobRegistry::new().unwrap());

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
    fn start_send_request_round_trips() {
        let receiver = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337));

        let encoded = encode_start_send_request(receiver, r"C:\Source", 4, 8).unwrap();

        let decoded = decode_start_send_request(&encoded).unwrap();

        assert_eq!(decoded.receiver_address, receiver,);

        assert_eq!(decoded.source_root, r"C:\Source",);

        assert_eq!(decoded.worker_count, 4,);

        assert_eq!(decoded.calibration_mib, 8,);

        assert_eq!(decoded.forced_data_stream_count, None,);
    }

    #[test]
    fn started_send_response_round_trips() {
        let expected = StartedSendJob {
            job_id: 42,

            receiver_address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337)),

            source_root: r"C:\Source".to_string(),

            worker_count: 4,

            calibration_mib: 8,
        };

        let encoded = encode_started_send_response(&expected).unwrap();

        let decoded = decode_started_send_response(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn sender_status_round_trips() {
        let expected = ManagementJobStatus {
            phase: ManagementJobPhase::SenderRunning,

            job_id: Some(99),

            transfer_port: None,

            destination_root: None,

            update_existing: false,
        };

        let encoded = encode_status(&expected).unwrap();

        let decoded = decode_status(&encoded).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn idle_status_round_trips() {
        let status = ManagementJobRegistry::new().unwrap().status().unwrap();

        let encoded = encode_status(&status).unwrap();

        let decoded = decode_status(&encoded).unwrap();

        assert_eq!(decoded, status);
    }

    #[test]
    fn resumed_start_send_request_round_trips() {
        let receiver = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337));

        let encoded =
            encode_start_send_request_with_stream_count(receiver, r"C:\Source", 4, 8, Some(4))
                .unwrap();

        let decoded = decode_start_send_request(&encoded).unwrap();

        assert_eq!(decoded.receiver_address, receiver,);

        assert_eq!(decoded.source_root, r"C:\Source",);

        assert_eq!(decoded.worker_count, 4,);

        assert_eq!(decoded.calibration_mib, 8,);

        assert_eq!(decoded.forced_data_stream_count, Some(4),);

        assert!(!decoded.preserve_desktop_layout);
    }

    #[test]
    fn desktop_layout_start_send_request_round_trips() {
        let receiver = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337));

        let encoded = encode_start_send_request_with_stream_count_and_desktop_layout(
            receiver,
            r"C:\Users\User\Desktop",
            4,
            8,
            Some(8),
            true,
        )
        .unwrap();

        let decoded = decode_start_send_request(&encoded).unwrap();

        assert_eq!(decoded.receiver_address, receiver,);

        assert_eq!(decoded.source_root, r"C:\Users\User\Desktop",);

        assert_eq!(decoded.worker_count, 4,);

        assert_eq!(decoded.calibration_mib, 8,);

        assert_eq!(decoded.forced_data_stream_count, Some(8),);

        assert!(decoded.preserve_desktop_layout,);
    }

    #[test]
    fn start_send_request_rejects_unknown_desktop_flags() {
        let receiver = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337));

        let mut encoded = encode_start_send_request_with_stream_count_and_desktop_layout(
            receiver,
            r"C:\Source",
            4,
            8,
            None,
            true,
        )
        .unwrap();

        encoded[2..4].copy_from_slice(&0x8000_u16.to_le_bytes());

        let error = decode_start_send_request(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("unknown flags"),);
    }

    #[test]
    fn resumed_start_send_rejects_non_matrix_stream_count() {
        let receiver = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337));

        assert!(
            encode_start_send_request_with_stream_count(receiver, r"C:\Source", 4, 8, Some(3),)
                .is_err(),
        );
    }

    #[test]
    fn registry_snapshot_keeps_instance_identity() {
        let registry = ManagementJobRegistry::new().unwrap();

        let expected = registry.instance_id();

        let first = registry.snapshot().unwrap();

        let second = registry.snapshot().unwrap();

        assert_eq!(first.agent_instance_id, expected,);

        assert_eq!(second.agent_instance_id, expected,);

        assert_eq!(first.agent_instance_id, second.agent_instance_id,);
    }
}
