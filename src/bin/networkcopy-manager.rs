#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use networkcopy_speed::destination_layout::{self, DestinationLayout};
use networkcopy_speed::management_active_binding::ActiveQueueBinding;
use networkcopy_speed::management_control;
use networkcopy_speed::management_direct::{self, DirectDiscoveredAgent};
use networkcopy_speed::management_directory::{ManagementDirectoryEntry, ManagementEntryKind};
use networkcopy_speed::management_discovery::{self, AgentState, DiscoveredAgent};
use networkcopy_speed::management_orchestration::{
    self, ManagedTransferRecord, ManagedTransferRequest,
};
use networkcopy_speed::management_persistence::{self, ManagerHistoryEntry, ManagerPersistedState};
use networkcopy_speed::management_queue::{
    MAX_QUEUE_ENTRIES, QueuedTransfer, QueuedTransferId, QueuedTransferKind, QueuedTransferRequest,
    QueuedTransferState, TransferQueue,
};
use networkcopy_speed::management_reconnect;
use networkcopy_speed::management_route::ManagementRouteMode;
use networkcopy_speed::management_snapshot::{
    ManagementAgentSnapshot, ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
};
use networkcopy_speed::release_update::{
    self, ReleaseArtifactKind, ReleaseCheck, UpdateInstallPlan, VerifiedStagedUpdate,
};
use networkcopy_speed::windows_notification::{self, NotificationKind};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "NetworkCopy Manager";

const POLL_INTERVAL: Duration = Duration::from_millis(500);

const REPAINT_INTERVAL: Duration = Duration::from_millis(100);

const REMOTE_BROWSER_HEIGHT: f32 = 460.0;

const MAX_TRANSFER_HISTORY: usize = 20;

const STATE_SAVE_INTERVAL: Duration = Duration::from_millis(750);

type DiscoveryResult = Result<Vec<DiscoveredAgent>, String>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectManagementRoute {
    interface_index: u32,

    local_agent: DiscoveredAgent,

    peer_agent: DiscoveredAgent,
}

type DirectDiscoveryResult = Result<Vec<DirectManagementRoute>, String>;

type UpdateCheckResult = Result<ReleaseCheck, String>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedManagerUpdate {
    plan: UpdateInstallPlan,

    verified: VerifiedStagedUpdate,
}

type UpdatePreparationResult = Result<PreparedManagerUpdate, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedStartFailureKind {
    Blocked,

    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedStartFailure {
    kind: ManagedStartFailureKind,

    message: String,
}

impl ManagedStartFailure {
    fn blocked(message: impl Into<String>) -> Self {
        Self {
            kind: ManagedStartFailureKind::Blocked,

            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: ManagedStartFailureKind::Failed,

            message: message.into(),
        }
    }
}

type StartResult = Result<ManagedTransferRecord, ManagedStartFailure>;

type AttachResult = Result<
    (
        ManagedTransferRecord,
        ManagementAgentSnapshot,
        ManagementAgentSnapshot,
    ),
    String,
>;

type QueueReattachResult = Result<
    (
        ManagedTransferRecord,
        ManagementAgentSnapshot,
        ManagementAgentSnapshot,
    ),
    String,
>;

struct PollResponse {
    sender: Result<ManagementAgentSnapshot, String>,

    receiver: Result<ManagementAgentSnapshot, String>,
}

struct CancelResponse {
    sender: Result<u64, String>,

    receiver: Result<u64, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedEndpointRole {
    Sender,

    Receiver,
}

impl ManagedEndpointRole {
    const fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",

            Self::Receiver => "receiver",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PeerCleanupTarget {
    endpoint_role: ManagedEndpointRole,

    endpoint: SocketAddr,

    job_id: u64,

    trigger_role: ManagementJobRole,

    trigger_job_id: u64,

    trigger_outcome: ManagementJobOutcome,
}

struct PeerCleanupResponse {
    target: PeerCleanupTarget,

    result: Result<u64, String>,
}

#[derive(Clone, Debug)]
struct PairedTransferHistoryEntry {
    transfer: ManagedTransferRecord,

    sender_result: ManagementJobResult,

    receiver_result: ManagementJobResult,
}

impl PairedTransferHistoryEntry {
    fn outcome(&self) -> ManagementJobOutcome {
        paired_outcome(&self.sender_result, &self.receiver_result)
    }

    fn files(&self) -> u64 {
        self.sender_result.files.max(self.receiver_result.files)
    }

    fn logical_bytes(&self) -> u64 {
        self.sender_result
            .logical_bytes
            .max(self.receiver_result.logical_bytes)
    }

    fn wire_bytes(&self) -> u64 {
        self.sender_result
            .wire_bytes
            .max(self.receiver_result.wire_bytes)
    }

    fn data_stream_count(&self) -> u32 {
        self.sender_result
            .data_stream_count
            .max(self.receiver_result.data_stream_count)
    }

    fn resume_data_stream_count(&self) -> Option<usize> {
        if self.outcome() == ManagementJobOutcome::Completed {
            return None;
        }

        let data_stream_count = self.receiver_result.data_stream_count;

        if data_stream_count == 0 {
            return None;
        }

        usize::try_from(data_stream_count).ok()
    }
}

enum BrowserPayload {
    Roots(Vec<management_control::ManagementRoot>),

    Directory {
        path: String,

        entries: Vec<ManagementDirectoryEntry>,
    },
}

struct BrowserResponse {
    endpoint: SocketAddr,

    result: Result<BrowserPayload, String>,
}

struct RemoteBrowserPane {
    endpoint: Option<SocketAddr>,

    current_path: String,

    roots: Vec<management_control::ManagementRoot>,

    entries: Vec<ManagementDirectoryEntry>,

    receiver: Option<Receiver<BrowserResponse>>,

    error: String,
}

impl RemoteBrowserPane {
    fn new() -> Self {
        Self {
            endpoint: None,

            current_path: String::new(),

            roots: Vec::new(),

            entries: Vec::new(),

            receiver: None,

            error: String::new(),
        }
    }

    fn sync_endpoint(&mut self, endpoint: Option<SocketAddr>) {
        if self.receiver.is_some() || self.endpoint == endpoint {
            return;
        }

        self.endpoint = endpoint;

        self.current_path.clear();

        self.roots.clear();

        self.entries.clear();

        self.error.clear();
    }

    fn begin_roots(&mut self, endpoint_text: &str) -> Result<(), String> {
        if self.receiver.is_some() {
            return Ok(());
        }

        let endpoint = parse_endpoint(endpoint_text, "remote browser agent")?;

        self.endpoint = Some(endpoint);

        self.current_path.clear();

        self.entries.clear();

        self.error.clear();

        let (sender, receiver) = mpsc::channel();

        self.receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_control::list_roots(endpoint)
                .map(BrowserPayload::Roots)
                .map_err(|error| format!("Failed to list remote drives: {error}"));

            let _ = sender.send(BrowserResponse { endpoint, result });
        });

        Ok(())
    }

    fn begin_directory(&mut self, endpoint_text: &str, path: String) -> Result<(), String> {
        if self.receiver.is_some() {
            return Ok(());
        }

        let endpoint = parse_endpoint(endpoint_text, "remote browser agent")?;

        self.endpoint = Some(endpoint);

        self.error.clear();

        let (sender, receiver) = mpsc::channel();

        self.receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_control::list_directory(endpoint, &path)
                .map(|entries| BrowserPayload::Directory { path, entries })
                .map_err(|error| format!("Failed to list remote directory: {error}"));

            let _ = sender.send(BrowserResponse { endpoint, result });
        });

        Ok(())
    }

    fn process_message(&mut self) {
        let message = self.receiver.as_ref().map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(response)) => {
                self.receiver = None;

                if self.endpoint != Some(response.endpoint) {
                    return;
                }

                match response.result {
                    Ok(BrowserPayload::Roots(roots)) => {
                        self.current_path.clear();

                        self.entries.clear();

                        self.roots = roots;

                        self.error.clear();
                    }

                    Ok(BrowserPayload::Directory { path, entries }) => {
                        self.current_path = path;

                        self.entries = entries;

                        self.error.clear();
                    }

                    Err(error) => {
                        self.error = error;
                    }
                }
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.receiver = None;

                self.error = "Remote browser worker disconnected.".to_string();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn is_loading(&self) -> bool {
        self.receiver.is_some()
    }
}

struct NetworkCopyManager {
    agents: Vec<DiscoveredAgent>,

    direct_routes: Vec<DirectManagementRoute>,

    sender_agent: String,

    receiver_agent: String,

    source_root: String,

    destination_root: String,

    batch_sources: Vec<String>,

    sender_browser: RemoteBrowserPane,

    receiver_browser: RemoteBrowserPane,

    worker_count: usize,

    calibration_mib: u64,

    update_existing: bool,

    route_mode: ManagementRouteMode,

    show_agents: bool,

    show_setup: bool,

    show_browsers: bool,

    show_queue: bool,

    show_history: bool,

    discovery_receiver: Option<Receiver<DiscoveryResult>>,

    direct_discovery_receiver: Option<Receiver<DirectDiscoveryResult>>,

    update_check_started: bool,

    update_receiver: Option<Receiver<UpdateCheckResult>>,

    update_check: Option<ReleaseCheck>,

    update_error: String,

    update_preparation_receiver: Option<Receiver<UpdatePreparationResult>>,

    prepared_update: Option<PreparedManagerUpdate>,

    update_preparation_error: String,

    update_handoff_confirmation: bool,

    start_receiver: Option<Receiver<StartResult>>,

    attach_receiver: Option<Receiver<AttachResult>>,

    queue_reattach_receiver: Option<Receiver<QueueReattachResult>>,

    poll_receiver: Option<Receiver<PollResponse>>,

    cancel_receiver: Option<Receiver<CancelResponse>>,

    peer_cleanup_receiver: Option<Receiver<PeerCleanupResponse>>,

    peer_cleanup_attempted: bool,

    transfer: Option<ManagedTransferRecord>,

    sender_snapshot: Option<ManagementAgentSnapshot>,

    receiver_snapshot: Option<ManagementAgentSnapshot>,

    queue: TransferQueue,

    queue_running: bool,

    active_queue_id: Option<QueuedTransferId>,

    history: VecDeque<PairedTransferHistoryEntry>,

    state_path: Option<PathBuf>,

    last_saved_state: Option<ManagerPersistedState>,

    last_state_save: Instant,

    persistence_error: String,

    monitoring_complete: bool,

    last_poll: Instant,

    notice: String,

    error: String,
}

impl NetworkCopyManager {
    fn new() -> Self {
        let state_path = management_persistence::default_state_path();

        let mut manager = Self {
            agents: Vec::new(),

            direct_routes: Vec::new(),

            sender_agent: String::new(),

            receiver_agent: String::new(),

            source_root: String::new(),

            destination_root: String::new(),

            batch_sources: Vec::new(),

            sender_browser: RemoteBrowserPane::new(),

            receiver_browser: RemoteBrowserPane::new(),

            worker_count: 4,

            calibration_mib: 8,

            update_existing: false,

            route_mode: ManagementRouteMode::AutomaticLan,

            show_agents: true,

            show_setup: true,

            show_browsers: false,

            show_queue: true,

            show_history: false,

            discovery_receiver: None,

            direct_discovery_receiver: None,

            update_check_started: false,

            update_receiver: None,

            update_check: None,

            update_error: String::new(),

            update_preparation_receiver: None,

            prepared_update: None,

            update_preparation_error: String::new(),

            update_handoff_confirmation: false,

            start_receiver: None,

            attach_receiver: None,

            queue_reattach_receiver: None,

            poll_receiver: None,

            cancel_receiver: None,

            peer_cleanup_receiver: None,

            peer_cleanup_attempted: false,

            transfer: None,

            sender_snapshot: None,

            receiver_snapshot: None,

            queue: TransferQueue::default(),

            queue_running: false,

            active_queue_id: None,

            history: VecDeque::new(),

            state_path: state_path.as_ref().ok().cloned(),

            last_saved_state: None,

            last_state_save: Instant::now(),

            persistence_error: String::new(),

            monitoring_complete: false,

            last_poll: Instant::now(),

            notice: String::new(),

            error: String::new(),
        };

        manager.begin_discovery();

        match state_path {
            Ok(_) => {
                manager.restore_persisted_state();
            }

            Err(error) => {
                manager.persistence_error = format!("Manager state path is unavailable: {error}");
            }
        }

        manager
    }

    fn restore_persisted_state(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };

        match management_persistence::load_from(&path) {
            Ok(Some(state)) => {
                let reattaching_queue_item = self.apply_persisted_state(state.clone());

                self.last_saved_state = Some(state);

                if !reattaching_queue_item {
                    self.notice =
                        "Restored saved manager configuration, queue, and transfer history."
                            .to_string();
                }
            }

            Ok(None) => {}

            Err(error) => {
                self.persistence_error = format!("Failed to load {}: {error}", path.display(),);
            }
        }
    }

    fn apply_persisted_state(&mut self, state: ManagerPersistedState) -> bool {
        self.sender_agent = state.sender_agent;

        self.receiver_agent = state.receiver_agent;

        self.source_root = state.source_root;

        self.destination_root = state.destination_root;

        self.worker_count = state.worker_count;

        self.calibration_mib = state.calibration_mib;

        self.update_existing = state.update_existing;

        self.route_mode = state.route_mode;

        let mut queue = state.queue;

        let recovery_item = match select_bound_recovery_item(&mut queue) {
            Ok(item) => item,

            Err(error) => {
                self.persistence_error =
                    format!("Failed to prepare exact queue recovery: {error}",);

                None
            }
        };

        self.queue = queue;

        self.queue_running = false;

        self.active_queue_id = None;

        self.history = state
            .history
            .into_iter()
            .take(MAX_TRANSFER_HISTORY)
            .map(|entry| PairedTransferHistoryEntry {
                transfer: entry.transfer,

                sender_result: entry.sender_result,

                receiver_result: entry.receiver_result,
            })
            .collect();

        if let Some(item) = recovery_item {
            self.begin_queue_reattach(item);

            true
        } else {
            false
        }
    }

    fn persisted_state(&self) -> ManagerPersistedState {
        ManagerPersistedState {
            sender_agent: self.sender_agent.clone(),

            receiver_agent: self.receiver_agent.clone(),

            source_root: self.source_root.clone(),

            destination_root: self.destination_root.clone(),

            worker_count: self.worker_count,

            calibration_mib: self.calibration_mib,

            update_existing: self.update_existing,

            route_mode: self.route_mode,

            queue: self.queue.clone(),

            history: self
                .history
                .iter()
                .take(MAX_TRANSFER_HISTORY)
                .map(|entry| ManagerHistoryEntry {
                    transfer: entry.transfer.clone(),

                    sender_result: entry.sender_result.clone(),

                    receiver_result: entry.receiver_result.clone(),
                })
                .collect(),
        }
    }

    fn persist_state_now(&mut self) -> Result<(), String> {
        let path = self
            .state_path
            .clone()
            .ok_or_else(|| "Manager state path is unavailable.".to_string())?;

        let state = self.persisted_state();

        management_persistence::save_to(&path, &state)
            .map_err(|error| format!("Failed to save {}: {error}", path.display(),))?;

        self.last_saved_state = Some(state);

        self.last_state_save = Instant::now();

        self.persistence_error.clear();

        Ok(())
    }

    fn bind_active_queue_if_possible(&mut self) {
        if self.queue.active_binding().is_some() {
            return;
        }

        let Some(id) = self.active_queue_id else {
            return;
        };

        let Some(transfer) = self.transfer.as_ref() else {
            return;
        };

        let Some(sender_snapshot) = self.sender_snapshot.as_ref() else {
            return;
        };

        let Some(receiver_snapshot) = self.receiver_snapshot.as_ref() else {
            return;
        };

        if !snapshot_has_active_job(Some(sender_snapshot), transfer.sender_job_id)
            || !snapshot_has_active_job(Some(receiver_snapshot), transfer.receiver_job_id)
        {
            return;
        }

        let binding = match active_queue_binding_for_snapshots(
            id,
            transfer,
            sender_snapshot,
            receiver_snapshot,
        ) {
            Ok(binding) => binding,

            Err(error) => {
                self.error = format!(
                    "Queued transfer #{id} could not create its exact recovery binding: {error}",
                );

                return;
            }
        };

        if let Err(error) = self.queue.set_active_binding(binding) {
            self.error = format!(
                "Queued transfer #{id} could not retain its exact recovery binding: {error}",
            );

            return;
        }

        match self.persist_state_now() {
            Ok(()) => {
                self.notice = format!(
                    "Queued transfer #{id} is protected by an exact endpoint-job binding.",
                );
            }

            Err(error) => {
                self.persistence_error = error.clone();

                self.error = format!(
                    "Queued transfer #{id} is running, but its exact recovery binding could not be saved: {error}",
                );

                notify_manager(
                    NotificationKind::Error,
                    "Queue recovery state was not saved",
                    &self.error,
                );
            }
        }
    }

    fn persist_state_if_needed(&mut self, context: &egui::Context) {
        let Some(path) = self.state_path.clone() else {
            return;
        };

        let state = self.persisted_state();

        if self.last_saved_state.as_ref() == Some(&state) {
            return;
        }

        let elapsed = self.last_state_save.elapsed();

        if elapsed < STATE_SAVE_INTERVAL {
            context.request_repaint_after(STATE_SAVE_INTERVAL - elapsed);

            return;
        }

        self.last_state_save = Instant::now();

        match management_persistence::save_to(&path, &state) {
            Ok(()) => {
                self.last_saved_state = Some(state);

                self.persistence_error.clear();
            }

            Err(error) => {
                self.persistence_error = format!("Failed to save {}: {error}", path.display(),);

                context.request_repaint_after(STATE_SAVE_INTERVAL);
            }
        }
    }

    fn configured_management_endpoints(&self) -> Result<(SocketAddr, SocketAddr), String> {
        resolve_management_endpoints(
            self.route_mode,
            &self.sender_agent,
            &self.receiver_agent,
            &self.agents,
            &self.direct_routes,
        )
    }

    fn queued_request_from_configuration(&self) -> Result<QueuedTransferRequest, String> {
        let (sender_agent, receiver_agent) = self.configured_management_endpoints()?;

        if self.source_root.trim().is_empty() {
            return Err("Enter the source path on the sender machine.".to_string());
        }

        if self.destination_root.trim().is_empty() {
            return Err("Enter the destination path on the receiver machine.".to_string());
        }

        Ok(QueuedTransferRequest {
            sender_agent,

            receiver_agent,

            route_mode: self.route_mode,

            source_root: self.source_root.clone(),

            destination_root: self.destination_root.clone(),

            update_existing: self.update_existing,

            worker_count: self.worker_count,

            calibration_mib: self.calibration_mib,

            kind: QueuedTransferKind::Fresh,
        })
    }

    fn add_current_source_to_batch(&mut self) {
        self.error.clear();

        let source_root = self.source_root.trim().to_string();

        if source_root.is_empty() {
            self.error =
                "Select or enter a source folder before adding it to the batch.".to_string();

            return;
        }

        if let Err(error) = destination_layout::source_directory_name(Path::new(&source_root)) {
            self.error = format!("The batch source must end with a usable folder name: {error}",);

            return;
        }

        let source_key = comparable_windows_path(&source_root);

        if self
            .batch_sources
            .iter()
            .any(|existing| comparable_windows_path(existing) == source_key)
        {
            self.error = format!("The source folder is already in the batch: {source_root}",);

            return;
        }

        self.batch_sources.push(source_root.clone());

        self.notice = format!(
            "Added batch source {}: {source_root}",
            self.batch_sources.len(),
        );
    }

    fn add_batch_to_queue(&mut self) {
        self.error.clear();

        let (sender_agent, receiver_agent) = match self.configured_management_endpoints() {
            Ok(endpoints) => endpoints,

            Err(error) => {
                self.error = error;

                return;
            }
        };

        let requests = match build_batch_queue_requests(
            sender_agent,
            receiver_agent,
            self.route_mode,
            &self.batch_sources,
            &self.destination_root,
            self.update_existing,
            self.worker_count,
            self.calibration_mib,
        ) {
            Ok(requests) => requests,

            Err(error) => {
                self.error = error;

                return;
            }
        };

        let request_count = requests.len();

        if self.queue.len().saturating_add(request_count) > MAX_QUEUE_ENTRIES {
            let remaining = MAX_QUEUE_ENTRIES.saturating_sub(self.queue.len());

            self.error = format!(
                "The batch contains {request_count} transfer(s), but the queue has room for only {remaining} more.",
            );

            return;
        }

        let mut updated_queue = self.queue.clone();

        for request in requests {
            if let Err(error) = updated_queue.add(request) {
                self.error = format!(
                    "The batch could not be added atomically. The existing queue was left unchanged: {error}",
                );

                return;
            }
        }

        self.queue = updated_queue;

        self.batch_sources.clear();

        self.show_queue = true;

        self.notice = format!("Added {request_count} mapped transfer(s) to the persistent queue.",);
    }

    fn add_current_to_queue(&mut self) {
        self.error.clear();

        let request = match self.queued_request_from_configuration() {
            Ok(request) => request,

            Err(error) => {
                self.error = error;

                return;
            }
        };

        let source_root = request.source_root.clone();

        match self.queue.add(request) {
            Ok(id) => {
                self.show_queue = true;

                self.notice = format!("Added queued transfer #{id}: {source_root}",);
            }

            Err(error) => {
                self.error = format!("Failed to add transfer to queue: {error}");
            }
        }
    }

    fn add_resume_to_queue(&mut self, entry: PairedTransferHistoryEntry) {
        self.error.clear();

        let Some(data_stream_count) = entry.resume_data_stream_count() else {
            self.error = "The receiver did not retain a usable resume journal for this transfer."
                .to_string();

            return;
        };

        let transfer = entry.transfer;

        let source_root = transfer.source_root.clone();

        let request = QueuedTransferRequest {
            sender_agent: transfer.sender_agent,

            receiver_agent: transfer.receiver_agent,

            route_mode: ManagementRouteMode::ExplicitIp,

            source_root: transfer.source_root,

            destination_root: transfer.destination_root,

            update_existing: transfer.update_existing,

            worker_count: transfer.worker_count,

            calibration_mib: transfer.calibration_mib,

            kind: QueuedTransferKind::Resume { data_stream_count },
        };

        match self.queue.add(request) {
            Ok(id) => {
                self.show_queue = true;

                self.notice = format!(
                    "Added resume #{id} to the queue: {source_root} · {data_stream_count} streams.",
                );
            }

            Err(error) => {
                self.error = format!("Failed to add resume to queue: {error}");
            }
        }
    }

    fn clear_transfer_card(&mut self) {
        self.transfer = None;

        self.peer_cleanup_receiver = None;

        self.peer_cleanup_attempted = false;

        self.sender_snapshot = None;

        self.receiver_snapshot = None;

        self.monitoring_complete = false;
    }

    fn retry_queue_item(&mut self, id: QueuedTransferId) {
        let Some(item) = self
            .queue
            .items()
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            self.error = format!("Queued transfer #{id} no longer exists.",);

            return;
        };

        if let Some(binding) = self.queue.active_binding() {
            if binding.queue_id != id {
                self.error = format!(
                    "Queued transfer #{} still retains the exact endpoint binding. Resolve that item before retrying transfer #{id}.",
                    binding.queue_id,
                );

                return;
            }

            let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

            if self.queue_running
                || self.start_receiver.is_some()
                || self.attach_receiver.is_some()
                || self.queue_reattach_receiver.is_some()
                || self.poll_receiver.is_some()
                || self.cancel_receiver.is_some()
                || self.peer_cleanup_receiver.is_some()
                || transfer_active
            {
                self.error =
                    "Exact queue reattachment cannot start while another managed operation is active."
                        .to_string();

                return;
            }

            self.error.clear();

            self.notice = format!(
                "Retrying exact reattachment for queued transfer #{id}. No new endpoint jobs will be started.",
            );

            self.begin_queue_reattach(item);

            return;
        }

        if self.queue_running {
            self.error =
                "A queued transfer cannot be reset while the queue is running.".to_string();

            return;
        }

        if self.queue.reset_to_pending(id) {
            self.error.clear();

            self.notice = format!("Queued transfer #{id} is pending again.",);
        } else {
            self.error =
                format!("Queued transfer #{id} cannot be retried from its current state.",);
        }
    }

    fn start_queue(&mut self) {
        if self.queue_running {
            return;
        }

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        if self.start_receiver.is_some()
            || self.attach_receiver.is_some()
            || self.poll_receiver.is_some()
            || self.cancel_receiver.is_some()
            || self.peer_cleanup_receiver.is_some()
            || transfer_active
        {
            self.error =
                "The queue cannot start while another managed operation is active.".to_string();

            return;
        }

        let Some(item) = self.queue.first_pending().cloned() else {
            self.error = "The transfer queue has no pending items.".to_string();

            return;
        };

        self.error.clear();

        self.show_queue = true;

        self.queue_running = true;

        self.begin_queued_transfer(item);
    }

    fn begin_queued_transfer(&mut self, item: QueuedTransfer) {
        let id = item.id;

        let request = item.request;

        let source_root = request.source_root.clone();

        self.sender_agent = request.sender_agent.to_string();

        self.receiver_agent = request.receiver_agent.to_string();

        self.source_root = request.source_root.clone();

        self.destination_root = request.destination_root.clone();

        self.worker_count = request.worker_count;

        self.calibration_mib = request.calibration_mib;

        self.update_existing = request.update_existing;

        self.route_mode = request.route_mode;

        let route_mode = request.route_mode;

        let kind = request.kind;

        let managed_request = ManagedTransferRequest {
            sender_agent: request.sender_agent,

            receiver_agent: request.receiver_agent,

            source_root: request.source_root,

            destination_root: request.destination_root,

            update_existing: request.update_existing,

            worker_count: request.worker_count,

            calibration_mib: request.calibration_mib,
        };

        self.clear_transfer_card();

        self.error.clear();

        if let Err(error) = self.queue.set_state(
            id,
            QueuedTransferState::Running,
            "Preparing receiver and sender endpoint jobs.",
        ) {
            self.queue_running = false;

            self.active_queue_id = None;

            self.error = format!("Failed to start queued transfer #{id}: {error}");

            return;
        }

        self.active_queue_id = Some(id);

        self.show_setup = false;

        self.show_queue = true;

        self.notice = format!("Starting queued transfer #{id}: {source_root}");

        let (sender, receiver) = mpsc::channel();

        self.start_receiver = Some(receiver);

        thread::spawn(move || {
            let result = match preflight_queue_route(route_mode)
                .and_then(|()| preflight_queue_endpoints(&managed_request))
            {
                Err(error) => Err(error),

                Ok(()) => match kind {
                    QueuedTransferKind::Fresh => {
                        management_orchestration::start_transfer(managed_request).map_err(|error| {
                            ManagedStartFailure::failed(format!(
                                "Queued managed transfer startup failed: {error}",
                            ))
                        })
                    }

                    QueuedTransferKind::Resume { data_stream_count } => {
                        management_orchestration::resume_transfer(
                            managed_request,
                            data_stream_count,
                        )
                        .map_err(|error| {
                            ManagedStartFailure::failed(format!(
                                "Queued managed transfer resume failed: {error}",
                            ))
                        })
                    }
                },
            };

            let _ = sender.send(result);
        });
    }

    fn fail_active_queue_start(&mut self, error: ManagedStartFailure) {
        let state = queue_state_for_start_failure(error.kind);

        let message = error.message;

        let Some(id) = self.active_queue_id.take() else {
            self.error = message;

            self.notice.clear();

            notify_manager(
                NotificationKind::Error,
                "Transfer could not start",
                &self.error,
            );

            return;
        };

        if let Err(state_error) = self.queue.set_state(id, state, message.clone()) {
            self.error = format!("{message} Queue item #{id} could not be updated: {state_error}",);
        } else {
            self.error = message;
        }

        self.queue_running = false;

        self.notice = match state {
            QueuedTransferState::Blocked => format!(
                "The queue is waiting because transfer #{id} could not reach two idle endpoint agents.",
            ),

            QueuedTransferState::Failed => {
                format!("The queue stopped because transfer #{id} could not start.",)
            }

            _ => unreachable!("startup failures only map to Blocked or Failed",),
        };

        let (kind, title) = match state {
            QueuedTransferState::Blocked => (NotificationKind::Warning, "Queue needs attention"),

            QueuedTransferState::Failed => {
                (NotificationKind::Error, "Queued transfer could not start")
            }

            _ => unreachable!("startup failures only map to Blocked or Failed",),
        };

        notify_manager(kind, title, &self.error);
    }

    fn finish_active_queue_item(&mut self) {
        let Some(id) = self.active_queue_id else {
            return;
        };

        let terminal_results = self.transfer.as_ref().and_then(|transfer| {
            let sender_result =
                terminal_result_for(self.sender_snapshot.as_ref(), transfer.sender_job_id)?.clone();

            let receiver_result =
                terminal_result_for(self.receiver_snapshot.as_ref(), transfer.receiver_job_id)?
                    .clone();

            Some((sender_result, receiver_result))
        });

        let Some((sender_result, receiver_result)) = terminal_results else {
            let message = "The paired endpoint results were unavailable after the transfer ended."
                .to_string();

            let _ = self
                .queue
                .set_state(id, QueuedTransferState::Blocked, message.clone());

            self.active_queue_id = None;

            self.queue_running = false;

            self.clear_transfer_card();

            self.error = message;

            self.notice = format!(
                "The queue stopped at transfer #{id} because its final state was incomplete.",
            );

            notify_manager(
                NotificationKind::Warning,
                "Queue needs attention",
                &self.error,
            );

            return;
        };

        let outcome = paired_outcome(&sender_result, &receiver_result);

        let state = queue_state_for_outcome(outcome);

        let endpoint_message = if !sender_result.message.is_empty() {
            sender_result.message.clone()
        } else if !receiver_result.message.is_empty() {
            receiver_result.message.clone()
        } else {
            format!("Both endpoint jobs reached {}.", outcome.label())
        };

        let status_message = if outcome == ManagementJobOutcome::Completed {
            format!(
                "Completed {} file(s), {} logical data.",
                sender_result.files.max(receiver_result.files),
                format_bytes(
                    sender_result
                        .logical_bytes
                        .max(receiver_result.logical_bytes),
                ),
            )
        } else {
            endpoint_message
        };

        if let Err(error) = self.queue.set_state(id, state, status_message.clone()) {
            self.active_queue_id = None;

            self.queue_running = false;

            self.clear_transfer_card();

            self.error = format!("Failed to finalize queued transfer #{id}: {error}",);

            notify_manager(NotificationKind::Error, "Queue update failed", &self.error);

            return;
        }

        self.active_queue_id = None;

        self.clear_transfer_card();

        match outcome {
            ManagementJobOutcome::Completed => {
                if self.queue.paused_after_current() {
                    self.queue_running = false;

                    self.notice = format!(
                        "Queued transfer #{id} completed. The queue is paused before the next item.",
                    );

                    notify_manager(NotificationKind::Information, "Queue paused", &self.notice);

                    return;
                }

                let next = self.queue.first_pending().cloned();

                if let Some(next) = next {
                    self.notice = format!(
                        "Queued transfer #{id} completed. Starting transfer #{}.",
                        next.id,
                    );

                    self.begin_queued_transfer(next);
                } else {
                    self.queue_running = false;

                    self.notice =
                        "Every pending queued transfer completed successfully.".to_string();

                    notify_manager(
                        NotificationKind::Information,
                        "Transfer queue complete",
                        &self.notice,
                    );
                }
            }

            ManagementJobOutcome::Cancelled => {
                self.queue_running = false;

                self.notice = format!(
                    "Queued transfer #{id} was cancelled. The remaining queue is waiting for an explicit restart.",
                );

                let body = format!("{} {}", self.notice, status_message,);

                notify_manager(NotificationKind::Warning, "Transfer queue stopped", &body);
            }

            ManagementJobOutcome::Failed => {
                self.queue_running = false;

                self.notice = format!(
                    "Queued transfer #{id} failed. The remaining queue is waiting for an explicit restart.",
                );

                let body = format!("{} {}", self.notice, status_message,);

                notify_manager(NotificationKind::Error, "Queued transfer failed", &body);
            }
        }
    }

    fn begin_discovery(&mut self) {
        if self.discovery_receiver.is_some() {
            return;
        }

        self.error.clear();

        self.notice = "Searching the local network...".to_string();

        let (sender, receiver) = mpsc::channel();

        self.discovery_receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_discovery::discover()
                .map_err(|error| format!("Management discovery failed: {error}"));

            let _ = sender.send(result);
        });
    }

    fn begin_direct_discovery(&mut self) {
        if self.direct_discovery_receiver.is_some() {
            return;
        }

        self.error.clear();

        self.direct_routes.clear();

        self.notice = "Searching dedicated Ethernet links...".to_string();

        let (sender, receiver) = mpsc::channel();

        self.direct_discovery_receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_direct::discover_agents()
                .map_err(|error| format!("Direct Link management discovery failed: {error}",))
                .and_then(build_direct_management_routes);

            let _ = sender.send(result);
        });
    }

    fn begin_update_check(&mut self) {
        if self.update_receiver.is_some() || self.update_preparation_receiver.is_some() {
            return;
        }

        self.update_error.clear();

        self.update_preparation_error.clear();

        self.update_check = None;

        self.prepared_update = None;

        self.update_handoff_confirmation = false;

        let (sender, receiver) = mpsc::channel();

        self.update_receiver = Some(receiver);

        thread::spawn(move || {
            let result = release_update::check_latest(env!("CARGO_PKG_VERSION"))
                .map_err(|error| format!("Update check failed: {error}"));

            let _ = sender.send(result);
        });
    }

    fn update_preparation_blocked_reason(&self) -> Option<&'static str> {
        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        if self.queue_running
            || self.active_queue_id.is_some()
            || self.queue_reattach_receiver.is_some()
        {
            return Some(
                "Finish or stop the active transfer queue before preparing a Manager update.",
            );
        }

        if self.start_receiver.is_some()
            || self.attach_receiver.is_some()
            || self.poll_receiver.is_some()
            || self.cancel_receiver.is_some()
            || self.peer_cleanup_receiver.is_some()
            || transfer_active
        {
            return Some(
                "Finish or cancel the active managed transfer before preparing a Manager update.",
            );
        }

        None
    }

    fn begin_update_preparation(&mut self) {
        if self.update_preparation_receiver.is_some() {
            return;
        }

        if let Some(reason) = self.update_preparation_blocked_reason() {
            self.update_preparation_error = reason.to_string();

            return;
        }

        let Some(check) = self
            .update_check
            .clone()
            .filter(|check| check.update_available)
        else {
            self.update_preparation_error =
                "No newer stable release is currently selected.".to_string();

            return;
        };

        if let Err(error) = self.persist_state_now() {
            self.update_preparation_error =
                format!("The Manager state could not be saved before update preparation: {error}",);

            return;
        }

        self.error.clear();

        self.update_preparation_error.clear();

        self.prepared_update = None;

        self.update_handoff_confirmation = false;

        self.notice = format!(
            "Preparing and verifying the {} Manager executable...",
            check.latest.tag_name,
        );

        let release = check.latest;

        let (sender, receiver) = mpsc::channel();

        self.update_preparation_receiver = Some(receiver);

        thread::spawn(move || {
            let result = (|| -> UpdatePreparationResult {
                let plan =
                    release_update::plan_current_update(&release, ReleaseArtifactKind::Manager)
                        .map_err(|error| format!("Manager update planning failed: {error}"))?;

                let verified =
                    release_update::download_and_stage_update(&plan).map_err(|error| {
                        format!("Manager update download or verification failed: {error}")
                    })?;

                release_update::write_update_handoff_plan(&plan, &verified, std::process::id())
                    .map_err(|error| {
                        format!("Manager update handoff preparation failed: {error}")
                    })?;

                Ok(PreparedManagerUpdate { plan, verified })
            })();

            let _ = sender.send(result);
        });
    }

    fn begin_update_handoff(&mut self, context: &egui::Context) {
        if let Some(reason) = self.update_preparation_blocked_reason() {
            self.update_preparation_error = reason.to_string();

            self.update_handoff_confirmation = false;

            return;
        }

        let Some(prepared) = self.prepared_update.clone() else {
            self.update_preparation_error =
                "No verified Manager update is currently prepared.".to_string();

            self.update_handoff_confirmation = false;

            return;
        };

        if let Err(error) = self.persist_state_now() {
            self.update_preparation_error =
                format!("The Manager state could not be saved before updater handoff: {error}",);

            self.update_handoff_confirmation = false;

            return;
        }

        match release_update::launch_update_handoff_wait_helper(
            &prepared.plan.handoff_plan,
            ReleaseArtifactKind::Manager,
        ) {
            Ok(report) => {
                self.update_preparation_error.clear();

                self.update_handoff_confirmation = false;

                self.notice = format!(
                    "Verified updater helper process {} started. Closing Manager so the helper can \
                     observe process {} exit. No executable will be replaced in this checkpoint.",
                    report.helper_process_id, report.parent_process_id,
                );

                notify_manager(
                    NotificationKind::Information,
                    "Manager updater helper started",
                    &self.notice,
                );

                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }

            Err(error) => {
                self.update_preparation_error =
                    format!("Manager updater handoff could not be started: {error}");

                self.update_handoff_confirmation = false;

                self.notice.clear();

                notify_manager(
                    NotificationKind::Error,
                    "Manager updater handoff failed",
                    &self.update_preparation_error,
                );
            }
        }
    }

    fn select_direct_route(&mut self, route: &DirectManagementRoute) {
        self.route_mode = ManagementRouteMode::DirectLink;

        self.sender_agent = route.local_agent.endpoint.to_string();

        self.receiver_agent = route.peer_agent.endpoint.to_string();

        self.show_setup = true;

        self.notice = format!(
            "Selected Direct Link interface {}: {} → {}.",
            route.interface_index, route.local_agent.hostname, route.peer_agent.hostname,
        );
    }

    fn begin_transfer(&mut self) {
        if self.start_receiver.is_some() {
            return;
        }

        self.error.clear();

        let (sender_agent, receiver_agent) = match self.configured_management_endpoints() {
            Ok(endpoints) => endpoints,

            Err(error) => {
                self.error = error;

                return;
            }
        };

        if self.source_root.trim().is_empty() {
            self.error = "Enter the source path on the sender machine.".to_string();

            return;
        }

        if self.destination_root.trim().is_empty() {
            self.error = "Enter the destination path on the receiver machine.".to_string();

            return;
        }

        let request = ManagedTransferRequest {
            sender_agent,

            receiver_agent,

            source_root: self.source_root.clone(),

            destination_root: self.destination_root.clone(),

            update_existing: self.update_existing,

            worker_count: self.worker_count,

            calibration_mib: self.calibration_mib,
        };

        self.transfer = None;

        self.peer_cleanup_receiver = None;

        self.peer_cleanup_attempted = false;

        self.sender_snapshot = None;

        self.receiver_snapshot = None;

        self.monitoring_complete = false;

        self.notice = "Preparing the receiver and starting the sender...".to_string();

        let (sender, receiver) = mpsc::channel();

        self.start_receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_orchestration::start_transfer(request).map_err(|error| {
                ManagedStartFailure::failed(format!("Managed transfer startup failed: {error}",))
            });

            let _ = sender.send(result);
        });
    }

    fn begin_resumed_transfer(&mut self, entry: PairedTransferHistoryEntry) {
        if self.start_receiver.is_some() || self.attach_receiver.is_some() {
            return;
        }

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        if transfer_active {
            self.error = "A managed transfer is already active.".to_string();

            return;
        }

        let Some(data_stream_count) = entry.resume_data_stream_count() else {
            self.error = "The receiver did not retain a usable resume journal for this transfer."
                .to_string();

            return;
        };

        let transfer = entry.transfer;

        self.sender_agent = transfer.sender_agent.to_string();

        self.receiver_agent = transfer.receiver_agent.to_string();

        self.source_root = transfer.source_root.clone();

        self.destination_root = transfer.destination_root.clone();

        self.worker_count = transfer.worker_count;

        self.calibration_mib = transfer.calibration_mib;

        self.update_existing = transfer.update_existing;

        self.route_mode = ManagementRouteMode::ExplicitIp;

        let request = ManagedTransferRequest {
            sender_agent: transfer.sender_agent,

            receiver_agent: transfer.receiver_agent,

            source_root: transfer.source_root,

            destination_root: transfer.destination_root,

            update_existing: transfer.update_existing,

            worker_count: transfer.worker_count,

            calibration_mib: transfer.calibration_mib,
        };

        self.transfer = None;

        self.peer_cleanup_receiver = None;

        self.peer_cleanup_attempted = false;

        self.sender_snapshot = None;

        self.receiver_snapshot = None;

        self.monitoring_complete = false;

        self.error.clear();

        self.notice = format!(
            "Preparing resume with the journal's original {data_stream_count} TCP stream(s)..."
        );

        self.show_setup = false;

        let (sender, receiver) = mpsc::channel();

        self.start_receiver = Some(receiver);

        thread::spawn(move || {
            let result = management_orchestration::resume_transfer(request, data_stream_count)
                .map_err(|error| {
                    ManagedStartFailure::failed(format!("Managed transfer resume failed: {error}",))
                });

            let _ = sender.send(result);
        });
    }

    fn begin_queue_reattach(&mut self, item: QueuedTransfer) {
        if self.queue_reattach_receiver.is_some() {
            return;
        }

        let id = item.id;

        let request = item.request;

        let Some(binding) = self
            .queue
            .active_binding()
            .filter(|binding| binding.queue_id == id)
        else {
            let message = format!(
                "Queued transfer #{id} has no exact endpoint binding. Automatic reattachment was refused.",
            );

            let _ = self
                .queue
                .set_state(id, QueuedTransferState::Blocked, message.clone());

            self.queue_running = false;

            self.active_queue_id = None;

            self.error = message;

            return;
        };

        self.sender_agent = request.sender_agent.to_string();

        self.receiver_agent = request.receiver_agent.to_string();

        self.source_root = request.source_root.clone();

        self.destination_root = request.destination_root.clone();

        self.worker_count = request.worker_count;

        self.calibration_mib = request.calibration_mib;

        self.update_existing = request.update_existing;

        self.route_mode = request.route_mode;

        self.clear_transfer_card();

        self.error.clear();

        self.queue_running = true;

        self.active_queue_id = Some(id);

        self.show_queue = true;

        self.show_setup = false;

        if let Err(error) = self.queue.set_state(
            id,
            QueuedTransferState::Running,
            "The Manager restarted. Checking both endpoints for matching active jobs.",
        ) {
            self.queue_running = false;

            self.active_queue_id = None;

            self.error = format!("Failed to prepare queue reattachment for #{id}: {error}",);

            return;
        }

        self.notice = format!("Reconnecting queued transfer #{id} to its endpoint jobs...",);

        let (sender, receiver) = mpsc::channel();

        self.queue_reattach_receiver = Some(receiver);

        thread::spawn(move || {
            let result = (|| {
                let sender_snapshot = management_control::agent_snapshot(request.sender_agent)
                    .map_err(|error| {
                        format!("Sender snapshot failed during restart recovery: {error}")
                    })?;

                let receiver_snapshot = management_control::agent_snapshot(request.receiver_agent)
                    .map_err(|error| {
                        format!("Receiver snapshot failed during restart recovery: {error}")
                    })?;

                binding
                    .validate_active_snapshots(
                        &sender_snapshot,
                        &receiver_snapshot,
                    )
                    .map_err(|error| {
                        format!(
                            "The persisted exact endpoint binding did not match the active jobs: {error}",
                        )
                    })?;

                let transfer = management_reconnect::reconstruct_active_transfer(
                    request.sender_agent,
                    request.receiver_agent,
                    &sender_snapshot,
                    &receiver_snapshot,
                )
                .map_err(|error| format!("Active endpoint jobs could not be paired: {error}"))?;

                if !transfer_matches_queue_request(&request, &transfer) {
                    return Err(
                        "The active endpoint jobs do not match the persisted queued transfer."
                            .to_string(),
                    );
                }

                Ok((transfer, sender_snapshot, receiver_snapshot))
            })();

            let _ = sender.send(result);
        });
    }

    fn block_queue_reattachment(&mut self, error: String) {
        let Some(id) = self.active_queue_id.take() else {
            self.queue_running = false;

            self.error = error;

            notify_manager(
                NotificationKind::Warning,
                "Queue reattachment needs attention",
                &self.error,
            );

            return;
        };

        let message = format!("Automatic restart reattachment failed: {error}",);

        if let Err(state_error) =
            self.queue
                .set_state(id, QueuedTransferState::Blocked, message.clone())
        {
            self.error = format!("{message} Queue item #{id} could not be updated: {state_error}",);
        } else {
            self.error = message;
        }

        self.queue_running = false;

        self.clear_transfer_card();

        self.notice = format!(
            "Queued transfer #{id} was not restarted. It was safely blocked to prevent duplicate endpoint jobs.",
        );

        notify_manager(
            NotificationKind::Warning,
            "Queue reattachment blocked",
            &self.error,
        );
    }

    fn begin_attach(&mut self) {
        if self.attach_receiver.is_some() {
            return;
        }

        self.error.clear();

        let (sender_agent, receiver_agent) = match self.configured_management_endpoints() {
            Ok(endpoints) => endpoints,

            Err(error) => {
                self.error = error;

                return;
            }
        };

        self.notice = "Reading active jobs from both endpoints...".to_string();

        let (sender, receiver) = mpsc::channel();

        self.attach_receiver = Some(receiver);

        thread::spawn(move || {
            let result = (|| {
                let sender_snapshot = management_control::agent_snapshot(sender_agent)
                    .map_err(|error| format!("Sender snapshot failed: {error}"))?;

                let receiver_snapshot = management_control::agent_snapshot(receiver_agent)
                    .map_err(|error| format!("Receiver snapshot failed: {error}"))?;

                let transfer = management_reconnect::reconstruct_active_transfer(
                    sender_agent,
                    receiver_agent,
                    &sender_snapshot,
                    &receiver_snapshot,
                )
                .map_err(|error| format!("Active jobs could not be paired: {error}"))?;

                Ok((transfer, sender_snapshot, receiver_snapshot))
            })();

            let _ = sender.send(result);
        });
    }

    fn begin_poll(&mut self) {
        if self.monitoring_complete
            || self.poll_receiver.is_some()
            || self.last_poll.elapsed() < POLL_INTERVAL
        {
            return;
        }

        let Some(transfer) = self.transfer.clone() else {
            return;
        };

        self.last_poll = Instant::now();

        let (sender, receiver) = mpsc::channel();

        self.poll_receiver = Some(receiver);

        thread::spawn(move || {
            let sender_snapshot = management_control::agent_snapshot(transfer.sender_agent)
                .map_err(|error| format!("Sender snapshot failed: {error}"));

            let receiver_snapshot = management_control::agent_snapshot(transfer.receiver_agent)
                .map_err(|error| format!("Receiver snapshot failed: {error}"));

            let _ = sender.send(PollResponse {
                sender: sender_snapshot,

                receiver: receiver_snapshot,
            });
        });
    }

    fn begin_peer_cleanup_if_needed(&mut self) {
        if self.monitoring_complete
            || self.peer_cleanup_attempted
            || self.peer_cleanup_receiver.is_some()
            || self.cancel_receiver.is_some()
        {
            return;
        }

        let Some(transfer) = self.transfer.as_ref() else {
            return;
        };

        let Some(target) = peer_cleanup_target(
            transfer,
            self.sender_snapshot.as_ref(),
            self.receiver_snapshot.as_ref(),
        ) else {
            return;
        };

        self.peer_cleanup_attempted = true;

        self.notice = format!(
            "{} job {} reached {}. Cancelling still-active {} job {}...",
            target.trigger_role.label(),
            target.trigger_job_id,
            target.trigger_outcome.label(),
            target.endpoint_role.label(),
            target.job_id,
        );

        let (sender, receiver) = mpsc::channel();

        self.peer_cleanup_receiver = Some(receiver);

        thread::spawn(move || {
            let result =
                management_control::cancel_job(target.endpoint, target.job_id).map_err(|error| {
                    format!(
                        "automatic {} cleanup failed: {error}",
                        target.endpoint_role.label(),
                    )
                });

            let _ = sender.send(PeerCleanupResponse { target, result });
        });
    }

    fn begin_cancel(&mut self) {
        if self.cancel_receiver.is_some() {
            return;
        }

        let Some(transfer) = self.transfer.clone() else {
            return;
        };

        self.error.clear();

        self.peer_cleanup_attempted = true;

        self.notice = "Sending cancellation to both endpoints...".to_string();

        let (sender, receiver) = mpsc::channel();

        self.cancel_receiver = Some(receiver);

        thread::spawn(move || {
            let sender_result =
                management_control::cancel_job(transfer.sender_agent, transfer.sender_job_id)
                    .map_err(|error| format!("Sender cancellation failed: {error}"));

            let receiver_result =
                management_control::cancel_job(transfer.receiver_agent, transfer.receiver_job_id)
                    .map_err(|error| format!("Receiver cancellation failed: {error}"));

            let _ = sender.send(CancelResponse {
                sender: sender_result,

                receiver: receiver_result,
            });
        });
    }

    fn process_messages(&mut self) {
        self.process_discovery_message();

        self.process_direct_discovery_message();

        self.process_update_message();

        self.process_update_preparation_message();

        self.process_start_message();

        self.process_attach_message();

        self.process_queue_reattach_message();

        self.process_poll_message();

        self.process_cancel_message();

        self.process_peer_cleanup_message();

        self.sender_browser.process_message();

        self.receiver_browser.process_message();
    }

    fn process_discovery_message(&mut self) {
        let message = self
            .discovery_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok(agents))) => {
                self.discovery_receiver = None;

                self.notice = format!("Discovered {} management agent(s).", agents.len(),);

                self.agents = agents;
            }

            Some(Ok(Err(error))) => {
                self.discovery_receiver = None;

                self.error = error;

                self.notice.clear();
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.discovery_receiver = None;

                self.error = "Management discovery worker disconnected.".to_string();

                self.notice.clear();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_direct_discovery_message(&mut self) {
        let message = self
            .direct_discovery_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok(routes))) => {
                self.direct_discovery_receiver = None;

                let automatic_selection = if routes.len() == 1 {
                    routes.first().cloned()
                } else {
                    None
                };

                let route_count = routes.len();

                self.direct_routes = routes;

                self.error.clear();

                if let Some(route) = automatic_selection {
                    self.select_direct_route(&route);

                    self.notice = format!(
                        "Discovered and selected Direct Link interface {}: {} → {}.",
                        route.interface_index,
                        route.local_agent.hostname,
                        route.peer_agent.hostname,
                    );
                } else {
                    self.notice = format!(
                        "Discovered {route_count} Direct Link route(s). Choose one below.",
                    );
                }
            }

            Some(Ok(Err(error))) => {
                self.direct_discovery_receiver = None;

                self.direct_routes.clear();

                self.error = error;

                self.notice.clear();
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.direct_discovery_receiver = None;

                self.direct_routes.clear();

                self.error = "Direct Link discovery worker disconnected.".to_string();

                self.notice.clear();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_update_message(&mut self) {
        let message = self
            .update_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok(check))) => {
                self.update_receiver = None;

                self.update_error.clear();

                if check.update_available {
                    let body = format!(
                        "{} is available. This build is {}.",
                        check.latest.tag_name, check.current_version,
                    );

                    notify_manager(
                        NotificationKind::Information,
                        "NetworkCopy update available",
                        &body,
                    );
                }

                self.update_check = Some(check);
            }

            Some(Ok(Err(error))) => {
                self.update_receiver = None;

                self.update_check = None;

                self.update_error = error;
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.update_receiver = None;

                self.update_check = None;

                self.update_error = "Update-check worker disconnected.".to_string();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_update_preparation_message(&mut self) {
        let message = self
            .update_preparation_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok(prepared))) => {
                self.update_preparation_receiver = None;

                self.update_preparation_error.clear();

                self.update_handoff_confirmation = false;

                let release_name = self
                    .update_check
                    .as_ref()
                    .map(|check| check.latest.tag_name.clone())
                    .unwrap_or_else(|| prepared.plan.selected_asset.name.clone());

                let staged_path = prepared.verified.executable.display().to_string();

                self.notice = format!(
                    "Downloaded and SHA-256 verified {} for {release_name}. Staged at \
                     {staged_path}. The running Manager was not replaced.",
                    format_bytes(prepared.verified.size),
                );

                notify_manager(
                    NotificationKind::Information,
                    "Manager update verified",
                    &self.notice,
                );

                self.prepared_update = Some(prepared);
            }

            Some(Ok(Err(error))) => {
                self.update_preparation_receiver = None;

                self.prepared_update = None;

                self.update_handoff_confirmation = false;

                self.update_preparation_error = error;

                self.notice.clear();

                notify_manager(
                    NotificationKind::Error,
                    "Manager update preparation failed",
                    &self.update_preparation_error,
                );
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.update_preparation_receiver = None;

                self.prepared_update = None;

                self.update_handoff_confirmation = false;

                self.update_preparation_error =
                    "Manager update-preparation worker disconnected.".to_string();

                self.notice.clear();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_start_message(&mut self) {
        let message = self
            .start_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok(transfer))) => {
                self.start_receiver = None;

                self.sender_agent = transfer.sender_agent.to_string();

                self.receiver_agent = transfer.receiver_agent.to_string();

                if let Some(id) = self.active_queue_id {
                    if let Err(error) = self.queue.set_state(
                        id,
                        QueuedTransferState::Running,
                        "Both endpoint jobs were accepted. Transfer is in progress.",
                    ) {
                        self.error = format!(
                            "Queued transfer #{id} started, but its queue state could not be updated: {error}",
                        );
                    }

                    self.notice = format!("Queued transfer #{id} was accepted by both endpoints.",);
                } else {
                    self.notice =
                        "Both endpoint jobs were accepted. The manager is now polling them."
                            .to_string();
                }

                self.transfer = Some(transfer);

                self.peer_cleanup_receiver = None;

                self.peer_cleanup_attempted = false;

                self.last_poll = Instant::now()
                    .checked_sub(POLL_INTERVAL)
                    .unwrap_or_else(Instant::now);

                self.monitoring_complete = false;

                self.show_setup = false;
            }

            Some(Ok(Err(error))) => {
                self.start_receiver = None;

                self.fail_active_queue_start(error);
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.start_receiver = None;

                self.fail_active_queue_start(ManagedStartFailure::failed(
                    "Transfer startup worker disconnected.",
                ));
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_attach_message(&mut self) {
        let message = self
            .attach_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok((transfer, sender_snapshot, receiver_snapshot)))) => {
                self.attach_receiver = None;

                self.sender_agent = transfer.sender_agent.to_string();

                self.receiver_agent = transfer.receiver_agent.to_string();

                self.source_root = transfer.source_root.clone();

                self.destination_root = transfer.destination_root.clone();

                self.worker_count = transfer.worker_count;

                self.calibration_mib = transfer.calibration_mib;

                self.update_existing = transfer.update_existing;

                self.sender_snapshot = Some(sender_snapshot);

                self.receiver_snapshot = Some(receiver_snapshot);

                self.transfer = Some(transfer);

                self.peer_cleanup_receiver = None;

                self.peer_cleanup_attempted = false;

                self.monitoring_complete = false;

                self.show_setup = false;

                self.last_poll = Instant::now();

                self.notice = "Attached to the active paired transfer.".to_string();

                self.error.clear();
            }

            Some(Ok(Err(error))) => {
                self.attach_receiver = None;

                self.error = error;

                self.notice.clear();
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.attach_receiver = None;

                self.error = "Active-job attachment worker disconnected.".to_string();

                self.notice.clear();
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_queue_reattach_message(&mut self) {
        let message = self
            .queue_reattach_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        match message {
            Some(Ok(Ok((transfer, sender_snapshot, receiver_snapshot)))) => {
                self.queue_reattach_receiver = None;

                let Some(id) = self.active_queue_id else {
                    self.queue_running = false;

                    self.error =
                        "Queue reattachment completed without an active queue item.".to_string();

                    return;
                };

                if let Err(error) = self.queue.set_state(
                    id,
                    QueuedTransferState::Running,
                    "Reattached after Manager restart. Transfer is in progress.",
                ) {
                    self.error = format!(
                        "Queued transfer #{id} was reattached, but its state could not be updated: {error}",
                    );
                } else {
                    self.error.clear();
                }

                self.sender_agent = transfer.sender_agent.to_string();

                self.receiver_agent = transfer.receiver_agent.to_string();

                self.source_root = transfer.source_root.clone();

                self.destination_root = transfer.destination_root.clone();

                self.worker_count = transfer.worker_count;

                self.calibration_mib = transfer.calibration_mib;

                self.update_existing = transfer.update_existing;

                self.sender_snapshot = Some(sender_snapshot);

                self.receiver_snapshot = Some(receiver_snapshot);

                self.transfer = Some(transfer);

                self.peer_cleanup_receiver = None;

                self.peer_cleanup_attempted = false;

                self.monitoring_complete = false;

                self.last_poll = Instant::now();

                self.show_setup = false;

                self.show_queue = true;

                self.notice = format!("Reattached queued transfer #{id} after Manager restart.",);
            }

            Some(Ok(Err(error))) => {
                self.queue_reattach_receiver = None;

                self.block_queue_reattachment(error);
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.queue_reattach_receiver = None;

                self.block_queue_reattachment(
                    "Queue reattachment worker disconnected.".to_string(),
                );
            }

            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn process_poll_message(&mut self) {
        let message = self
            .poll_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        let Some(message) = message else {
            return;
        };

        match message {
            Ok(response) => {
                self.poll_receiver = None;

                let mut errors = Vec::new();

                match response.sender {
                    Ok(snapshot) => {
                        self.sender_snapshot = Some(snapshot);
                    }

                    Err(error) => {
                        errors.push(error);
                    }
                }

                match response.receiver {
                    Ok(snapshot) => {
                        self.receiver_snapshot = Some(snapshot);
                    }

                    Err(error) => {
                        errors.push(error);
                    }
                }

                if !errors.is_empty() {
                    self.error = errors.join(" ");
                } else {
                    self.bind_active_queue_if_possible();
                }

                let monitoring_complete = self.transfer.as_ref().is_some_and(|transfer| {
                    let sender_complete =
                        snapshot_is_terminal(self.sender_snapshot.as_ref(), transfer.sender_job_id);

                    let receiver_complete = snapshot_is_terminal(
                        self.receiver_snapshot.as_ref(),
                        transfer.receiver_job_id,
                    );

                    sender_complete && receiver_complete
                });

                self.monitoring_complete = monitoring_complete;

                if monitoring_complete {
                    self.archive_terminal_transfer();

                    if self.active_queue_id.is_some() {
                        self.finish_active_queue_item();
                    } else {
                        self.notify_current_terminal_transfer();

                        self.notice = "Both endpoint jobs reached a terminal state.".to_string();
                    }
                } else {
                    self.begin_peer_cleanup_if_needed();
                }
            }

            Err(TryRecvError::Disconnected) => {
                self.poll_receiver = None;

                self.error = "Snapshot polling worker disconnected.".to_string();
            }

            Err(TryRecvError::Empty) => {}
        }
    }

    fn notify_current_terminal_transfer(&self) {
        let Some(transfer) = self.transfer.as_ref() else {
            return;
        };

        let Some(sender_result) =
            terminal_result_for(self.sender_snapshot.as_ref(), transfer.sender_job_id)
        else {
            return;
        };

        let Some(receiver_result) =
            terminal_result_for(self.receiver_snapshot.as_ref(), transfer.receiver_job_id)
        else {
            return;
        };

        let outcome = paired_outcome(sender_result, receiver_result);

        let files = sender_result.files.max(receiver_result.files);

        let logical_bytes = sender_result
            .logical_bytes
            .max(receiver_result.logical_bytes);

        let path = format!("{} → {}", transfer.source_root, transfer.destination_root,);

        match outcome {
            ManagementJobOutcome::Completed => {
                let body = format!(
                    "{files} file(s), {} logical data. {path}",
                    format_bytes(logical_bytes),
                );

                notify_manager(NotificationKind::Information, "Transfer complete", &body);
            }

            ManagementJobOutcome::Cancelled => {
                let body = format!("The transfer was cancelled. {path}",);

                notify_manager(NotificationKind::Warning, "Transfer cancelled", &body);
            }

            ManagementJobOutcome::Failed => {
                let details = if !sender_result.message.is_empty() {
                    sender_result.message.as_str()
                } else if !receiver_result.message.is_empty() {
                    receiver_result.message.as_str()
                } else {
                    "Both endpoint jobs reported a failure."
                };

                let body = format!("{path}. {details}",);

                notify_manager(NotificationKind::Error, "Transfer failed", &body);
            }
        }
    }

    fn archive_terminal_transfer(&mut self) {
        let Some(transfer) = self.transfer.clone() else {
            return;
        };

        let Some(sender_result) =
            terminal_result_for(self.sender_snapshot.as_ref(), transfer.sender_job_id).cloned()
        else {
            return;
        };

        let Some(receiver_result) =
            terminal_result_for(self.receiver_snapshot.as_ref(), transfer.receiver_job_id).cloned()
        else {
            return;
        };

        remember_history(
            &mut self.history,
            PairedTransferHistoryEntry {
                transfer,

                sender_result,

                receiver_result,
            },
        );

        self.show_history = true;
    }

    fn process_cancel_message(&mut self) {
        let message = self
            .cancel_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        let Some(message) = message else {
            return;
        };

        match message {
            Ok(response) => {
                self.cancel_receiver = None;

                let mut errors = Vec::new();

                if let Err(error) = response.sender {
                    errors.push(error);
                }

                if let Err(error) = response.receiver {
                    errors.push(error);
                }

                if errors.is_empty() {
                    self.notice = "Cancellation was accepted by both endpoints.".to_string();
                } else {
                    self.error = errors.join(" ");

                    self.notice = "Cancellation finished with endpoint warnings.".to_string();
                }

                self.last_poll = Instant::now();
            }

            Err(TryRecvError::Disconnected) => {
                self.cancel_receiver = None;

                self.error = "Cancellation worker disconnected.".to_string();
            }

            Err(TryRecvError::Empty) => {}
        }
    }

    fn process_peer_cleanup_message(&mut self) {
        let message = self
            .peer_cleanup_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());

        let Some(message) = message else {
            return;
        };

        match message {
            Ok(response) => {
                self.peer_cleanup_receiver = None;

                match response.result {
                    Ok(cancelled_job_id) if cancelled_job_id == response.target.job_id => {
                        self.notice = format!(
                            "Automatic cleanup cancelled {} job {} after paired {} job {} reached {}.",
                            response.target.endpoint_role.label(),
                            cancelled_job_id,
                            response.target.trigger_role.label(),
                            response.target.trigger_job_id,
                            response.target.trigger_outcome.label(),
                        );
                    }

                    Ok(cancelled_job_id) => {
                        self.error = format!(
                            "Automatic peer cleanup returned job ID {cancelled_job_id}, expected {}.",
                            response.target.job_id,
                        );

                        self.notice =
                            "Automatic peer cleanup returned an unexpected response.".to_string();
                    }

                    Err(error) => {
                        self.error = error;

                        self.notice = format!(
                            "Automatic cleanup of {} job {} failed. Manual cancellation remains available.",
                            response.target.endpoint_role.label(),
                            response.target.job_id,
                        );
                    }
                }

                self.last_poll = Instant::now();
            }

            Err(TryRecvError::Disconnected) => {
                self.peer_cleanup_receiver = None;

                self.error = "Automatic peer-cleanup worker disconnected.".to_string();
            }

            Err(TryRecvError::Empty) => {}
        }
    }

    fn has_background_work(&self) -> bool {
        self.discovery_receiver.is_some()
            || self.direct_discovery_receiver.is_some()
            || self.update_receiver.is_some()
            || self.update_preparation_receiver.is_some()
            || self.start_receiver.is_some()
            || self.attach_receiver.is_some()
            || self.queue_reattach_receiver.is_some()
            || self.poll_receiver.is_some()
            || self.cancel_receiver.is_some()
            || self.peer_cleanup_receiver.is_some()
            || self.sender_browser.is_loading()
            || self.receiver_browser.is_loading()
            || (self.transfer.is_some() && !self.monitoring_complete)
    }

    fn manager_status(&self) -> (&'static str, egui::Color32) {
        if self.peer_cleanup_receiver.is_some() {
            (
                "Cleaning up paired endpoint",
                egui::Color32::from_rgb(255, 190, 82),
            )
        } else if self.cancel_receiver.is_some() {
            ("Cancelling transfer", egui::Color32::from_rgb(255, 190, 82))
        } else if self.start_receiver.is_some() {
            ("Starting transfer", egui::Color32::from_rgb(95, 194, 255))
        } else if self.attach_receiver.is_some() {
            (
                "Attaching to active jobs",
                egui::Color32::from_rgb(95, 194, 255),
            )
        } else if self.queue_reattach_receiver.is_some() {
            (
                "Reattaching queued transfer",
                egui::Color32::from_rgb(95, 194, 255),
            )
        } else if self.transfer.is_some() && !self.monitoring_complete {
            ("Transfer active", egui::Color32::from_rgb(126, 230, 64))
        } else if self.monitoring_complete {
            ("Transfer finished", egui::Color32::from_rgb(95, 194, 255))
        } else if self.update_preparation_receiver.is_some() {
            (
                "Preparing Manager update",
                egui::Color32::from_rgb(95, 194, 255),
            )
        } else if self.direct_discovery_receiver.is_some() {
            (
                "Discovering Direct Link",
                egui::Color32::from_rgb(95, 194, 255),
            )
        } else if self.discovery_receiver.is_some() {
            (
                "Discovering LAN agents",
                egui::Color32::from_rgb(95, 194, 255),
            )
        } else {
            ("Ready", egui::Color32::from_rgb(126, 230, 64))
        }
    }

    fn render_app_header(&mut self, ui: &mut egui::Ui) {
        let (status, status_color) = self.manager_status();

        let checking_updates = self.update_receiver.is_some();

        let preparing_update = self.update_preparation_receiver.is_some();

        let update_check = self.update_check.clone();

        let update_error = self.update_error.clone();

        let prepared_update = self.prepared_update.clone();

        let update_preparation_error = self.update_preparation_error.clone();

        let update_handoff_confirmation = self.update_handoff_confirmation;

        let update_blocked_reason = self.update_preparation_blocked_reason().map(str::to_string);

        let mut check_requested = false;

        let mut preparation_requested = false;

        let mut arm_handoff = false;

        let mut cancel_handoff = false;

        let mut launch_handoff = false;

        let mut release_to_open = None::<String>;

        ui.horizontal_wrapped(|ui| {
            ui.heading(APP_NAME);

            ui.separator();

            status_label(ui, status, status_color);

            ui.separator();

            ui.label(format!("v{}", env!("CARGO_PKG_VERSION"),));

            ui.separator();

            if checking_updates {
                ui.spinner();

                ui.label("Checking GitHub Releases...");
            } else if let Some(check) = &update_check {
                if check.update_available {
                    status_label(
                        ui,
                        &format!("{} available", check.latest.tag_name,),
                        egui::Color32::from_rgb(126, 230, 64),
                    );

                    if ui.button("Open release").clicked() {
                        release_to_open = Some(check.latest.html_url.clone());
                    }

                    if preparing_update {
                        ui.spinner();

                        ui.label("Downloading and SHA-256 verifying Manager update...");
                    } else if let Some(prepared) = &prepared_update {
                        status_label(
                            ui,
                            "Verified update staged",
                            egui::Color32::from_rgb(126, 230, 64),
                        );

                        ui.label(format!("{} ready", format_bytes(prepared.verified.size),))
                            .on_hover_text(format!(
                                "Staged executable: {}\nPlanned install path: {}\nHandoff plan: {}",
                                prepared.verified.executable.display(),
                                prepared.plan.install_path.display(),
                                prepared.plan.handoff_plan.display(),
                            ));

                        if update_handoff_confirmation {
                            ui.label(
                                egui::RichText::new(
                                    "This checkpoint starts the verified updater helper and closes \
                                     Manager. It does not replace or relaunch an executable yet.",
                                )
                                .color(egui::Color32::from_rgb(255, 196, 92)),
                            );

                            let confirm = ui.add_enabled(
                                update_blocked_reason.is_none(),
                                egui::Button::new(
                                    egui::RichText::new("Confirm close and start updater").strong(),
                                )
                                .fill(egui::Color32::from_rgb(112, 64, 28)),
                            );

                            let confirm = if let Some(reason) = &update_blocked_reason {
                                confirm.on_hover_text(reason)
                            } else {
                                confirm
                            };

                            if confirm.clicked() {
                                launch_handoff = true;
                            }

                            if ui.small_button("Cancel").clicked() {
                                cancel_handoff = true;
                            }
                        } else {
                            let install = ui.add_enabled(
                                update_blocked_reason.is_none(),
                                egui::Button::new(egui::RichText::new("Install update").strong())
                                    .fill(egui::Color32::from_rgb(42, 78, 72)),
                            );

                            let install = if let Some(reason) = &update_blocked_reason {
                                install.on_hover_text(reason)
                            } else {
                                install
                            };

                            if install.clicked() {
                                arm_handoff = true;
                            }
                        }
                    } else {
                        let response = ui.add_enabled(
                            update_blocked_reason.is_none(),
                            egui::Button::new(egui::RichText::new("Prepare update").strong())
                                .fill(egui::Color32::from_rgb(42, 78, 72)),
                        );

                        let response = if let Some(reason) = &update_blocked_reason {
                            response.on_hover_text(reason)
                        } else {
                            response
                        };

                        if response.clicked() {
                            preparation_requested = true;
                        }
                    }

                    if !update_preparation_error.is_empty() {
                        ui.label(
                            egui::RichText::new("Update preparation or handoff failed")
                                .color(egui::Color32::from_rgb(255, 112, 120)),
                        )
                        .on_hover_text(&update_preparation_error);
                    }
                } else {
                    ui.label(format!("Latest stable: {}", check.latest.tag_name,));
                }

                if !preparing_update && ui.small_button("Check again").clicked() {
                    check_requested = true;
                }
            } else {
                if !update_error.is_empty() {
                    ui.label(
                        egui::RichText::new("Update check unavailable")
                            .color(egui::Color32::from_rgb(160, 170, 184)),
                    )
                    .on_hover_text(&update_error);
                }

                if ui.small_button("Check updates").clicked() {
                    check_requested = true;
                }
            }
        });

        ui.label(
            "Automatic LAN, Direct Link, and explicit-IP orchestration with direct sender-to-receiver payload transfer.",
        );

        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} LAN agent(s)", self.agents.len(),));

            ui.separator();

            ui.label(format!("{} Direct Link route(s)", self.direct_routes.len(),));

            ui.separator();

            ui.label(format!("{} queued transfer(s)", self.queue.len(),));

            ui.separator();

            ui.label(format!("{} retained transfer(s)", self.history.len(),));

            ui.separator();

            ui.label("Trusted LAN · management traffic is not yet encrypted");
        });

        if arm_handoff {
            self.update_handoff_confirmation = true;

            self.update_preparation_error.clear();
        }

        if cancel_handoff {
            self.update_handoff_confirmation = false;
        }

        if launch_handoff {
            self.begin_update_handoff(ui.ctx());
        }

        if preparation_requested {
            self.begin_update_preparation();
        }

        if check_requested {
            self.begin_update_check();
        }

        if let Some(release_url) = release_to_open
            && let Err(error) = release_update::open_release_page(&release_url)
        {
            self.error = format!("The release page could not be opened: {error}",);
        }
    }

    fn render_messages(&mut self, ui: &mut egui::Ui) {
        if !self.notice.is_empty() {
            let mut dismiss = false;

            ui.group(|ui| {
                ui.set_min_width(ui.available_width());

                ui.horizontal_wrapped(|ui| {
                    status_label(ui, "Information", egui::Color32::from_rgb(95, 194, 255));

                    ui.label(&self.notice);

                    if ui.button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
            });

            if dismiss {
                self.notice.clear();
            }
        }

        if !self.error.is_empty() {
            let mut dismiss = false;

            ui.group(|ui| {
                ui.set_min_width(ui.available_width());

                ui.horizontal_wrapped(|ui| {
                    status_label(ui, "Action needed", egui::Color32::from_rgb(255, 112, 120));

                    ui.label(&self.error);

                    if ui.button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
            });

            if dismiss {
                self.error.clear();
            }
        }

        if !self.persistence_error.is_empty() {
            let mut dismiss = false;

            ui.group(|ui| {
                ui.set_min_width(ui.available_width());

                ui.horizontal_wrapped(|ui| {
                    status_label(
                        ui,
                        "State persistence",
                        egui::Color32::from_rgb(255, 190, 82),
                    );

                    ui.label(&self.persistence_error);

                    if ui.button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
            });

            if dismiss {
                self.persistence_error.clear();
            }
        }
    }

    fn render_discovery(&mut self, ui: &mut egui::Ui) {
        if self.route_mode == ManagementRouteMode::DirectLink {
            self.render_direct_discovery(ui);

            return;
        }

        ui.horizontal_wrapped(|ui| {
            let discovering = self.discovery_receiver.is_some();

            if ui
                .add_enabled(
                    !discovering,
                    egui::Button::new(
                        "Refresh LAN discovery",
                    ),
                )
                .clicked()
            {
                self.begin_discovery();
            }

            if discovering {
                ui.spinner();

                ui.label(
                    "Searching local network...",
                );
            }

            ui.label(
                "Run networkcopy-agent.exe on each endpoint. Selecting an agent uses Automatic LAN mode.",
            );
        });

        ui.add_space(6.0);

        if self.agents.is_empty() {
            ui.label("No agents discovered. Manual addresses remain available in Transfer setup.");

            return;
        }

        let agents = self.agents.clone();

        for agent in agents {
            let endpoint_text = agent.endpoint.to_string();

            let sender_selected = self.sender_agent == endpoint_text;

            let receiver_selected = self.receiver_agent == endpoint_text;

            let sender_enabled = agent.capabilities.can_send();

            let receiver_enabled = agent.capabilities.can_receive();

            ui.group(|ui| {
                ui.horizontal_wrapped(|ui| {
                    status_label(
                        ui,
                        agent.state.label(),
                        agent_state_color(agent.state.label()),
                    );

                    ui.strong(&agent.hostname);

                    ui.label(egui::RichText::new(&endpoint_text).monospace());

                    ui.label(format!("Protocol {}", agent.protocol_version,));

                    if sender_selected {
                        status_label(ui, "Sender", egui::Color32::from_rgb(95, 194, 255));
                    }

                    if receiver_selected {
                        status_label(ui, "Receiver", egui::Color32::from_rgb(182, 134, 255));
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    let sender_button_label = match (sender_selected, receiver_selected) {
                        (true, true) => "Keep as sender",

                        (true, false) => "Clear sender",

                        _ => "Use as sender",
                    };

                    if ui
                        .add_enabled(
                            sender_selected || sender_enabled,
                            egui::Button::new(sender_button_label),
                        )
                        .clicked()
                    {
                        self.route_mode = ManagementRouteMode::AutomaticLan;

                        select_or_clear_discovered_endpoint(
                            &mut self.sender_agent,
                            &mut self.receiver_agent,
                            &endpoint_text,
                        );
                    }

                    let receiver_button_label = match (receiver_selected, sender_selected) {
                        (true, true) => "Keep as receiver",

                        (true, false) => "Clear receiver",

                        _ => "Use as receiver",
                    };

                    if ui
                        .add_enabled(
                            receiver_selected || receiver_enabled,
                            egui::Button::new(receiver_button_label),
                        )
                        .clicked()
                    {
                        self.route_mode = ManagementRouteMode::AutomaticLan;

                        select_or_clear_discovered_endpoint(
                            &mut self.receiver_agent,
                            &mut self.sender_agent,
                            &endpoint_text,
                        );
                    }

                    let capability = match (sender_enabled, receiver_enabled) {
                        (true, true) => "send + receive",

                        (true, false) => "send",

                        (false, true) => "receive",

                        (false, false) => "none",
                    };

                    ui.label(format!("Capabilities: {capability}"));
                });
            });

            ui.add_space(6.0);
        }
    }

    fn render_direct_discovery(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let discovering = self
                .direct_discovery_receiver
                .is_some();

            if ui
                .add_enabled(
                    !discovering,
                    egui::Button::new(
                        "Refresh Direct Link",
                    ),
                )
                .clicked()
            {
                self.begin_direct_discovery();
            }

            if discovering {
                ui.spinner();

                ui.label(
                    "Searching gateway-free Ethernet cables...",
                );
            }

            ui.label(
                "Run networkcopy-agent.exe on this source PC and on the directly connected destination PC.",
            );
        });

        ui.add_space(6.0);

        if self.direct_routes.is_empty() {
            ui.label(
                "No Direct Link route is selected. Connect the two PCs with Ethernet, start both agents, and refresh.",
            );

            ui.label(
                "Wi-Fi and Ethernet interfaces carrying a default route are deliberately ignored.",
            );

            return;
        }

        let routes = self.direct_routes.clone();

        for route in routes {
            let sender_endpoint = route.local_agent.endpoint.to_string();

            let receiver_endpoint = route.peer_agent.endpoint.to_string();

            let selected = self.route_mode == ManagementRouteMode::DirectLink
                && self.sender_agent == sender_endpoint
                && self.receiver_agent == receiver_endpoint;

            let usable = route.local_agent.capabilities.can_send()
                && route.peer_agent.capabilities.can_receive();

            ui.group(|ui| {
                ui.set_min_width(
                    ui.available_width(),
                );

                ui.horizontal_wrapped(|ui| {
                    status_label(
                        ui,
                        if selected {
                            "Selected"
                        } else {
                            "Direct Link"
                        },
                        if selected {
                            egui::Color32::from_rgb(
                                126, 230, 64,
                            )
                        } else {
                            egui::Color32::from_rgb(
                                95, 194, 255,
                            )
                        },
                    );

                    ui.strong(format!(
                        "Interface {}",
                        route.interface_index,
                    ));
                });

                ui.add_space(4.0);

                ui.columns(2, |columns| {
                    let (local, peer) =
                        columns.split_at_mut(1);

                    local[0].group(|ui| {
                        ui.strong("This PC · sender");

                        status_label(
                            ui,
                            route.local_agent
                                .state
                                .label(),
                            agent_state_color(
                                route.local_agent
                                    .state
                                    .label(),
                            ),
                        );

                        ui.label(
                            &route.local_agent.hostname,
                        );

                        ui.label(
                            egui::RichText::new(
                                &sender_endpoint,
                            )
                            .monospace(),
                        );
                    });

                    peer[0].group(|ui| {
                        ui.strong(
                            "Cable peer · receiver",
                        );

                        status_label(
                            ui,
                            route.peer_agent
                                .state
                                .label(),
                            agent_state_color(
                                route.peer_agent
                                    .state
                                    .label(),
                            ),
                        );

                        ui.label(
                            &route.peer_agent.hostname,
                        );

                        ui.label(
                            egui::RichText::new(
                                &receiver_endpoint,
                            )
                            .monospace(),
                        );
                    });
                });

                ui.add_space(6.0);

                if ui
                    .add_enabled(
                        usable && !selected,
                        egui::Button::new(
                            if selected {
                                "Direct Link selected"
                            } else {
                                "Use this Direct Link"
                            },
                        ),
                    )
                    .clicked()
                {
                    self.select_direct_route(
                        &route,
                    );
                }

                if !usable {
                    ui.label(
                        "The local agent must support sending and the cable peer must support receiving.",
                    );
                }
            });

            ui.add_space(6.0);
        }
    }

    fn render_configuration(&mut self, ui: &mut egui::Ui) {
        ui.label("Paths are evaluated on the selected remote endpoint machines.");

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());

            ui.horizontal_wrapped(|ui| {
                ui.strong("Management route");

                ui.separator();

                ui.radio_value(
                    &mut self.route_mode,
                    ManagementRouteMode::AutomaticLan,
                    "Automatic LAN",
                );

                ui.radio_value(
                    &mut self.route_mode,
                    ManagementRouteMode::DirectLink,
                    "Direct Link",
                );

                ui.radio_value(
                    &mut self.route_mode,
                    ManagementRouteMode::ExplicitIp,
                    "Explicit IP",
                );
            });

            ui.add_space(4.0);

            ui.label(self.route_mode.description());

            if self.route_mode
                == ManagementRouteMode::DirectLink
            {
                ui.label(
                    "For scoped IPv6 safety, the Manager runs on the source PC: the local agent sends and the cable peer receives.",
                );
            }
        });

        ui.add_space(8.0);

        let endpoint_fields_editable = self.route_mode != ManagementRouteMode::DirectLink;

        ui.columns(2, |columns| {
            let (sender_column, receiver_column) = columns.split_at_mut(1);

            sender_column[0].group(|ui| {
                ui.set_min_width(ui.available_width());

                status_label(ui, "Sender endpoint", egui::Color32::from_rgb(95, 194, 255));

                ui.add_space(4.0);

                ui.label("Management agent");

                let width = ui.available_width();

                let sender_response = ui.add_enabled(
                    endpoint_fields_editable,
                    egui::TextEdit::singleline(&mut self.sender_agent).desired_width(width),
                );

                if sender_response.changed() && self.route_mode == ManagementRouteMode::AutomaticLan
                {
                    self.route_mode = ManagementRouteMode::ExplicitIp;
                }

                ui.label("Source folder / batch candidate");

                let width = ui.available_width();

                ui.add(egui::TextEdit::singleline(&mut self.source_root).desired_width(width));
            });

            receiver_column[0].group(|ui| {
                ui.set_min_width(ui.available_width());

                status_label(
                    ui,
                    "Receiver endpoint",
                    egui::Color32::from_rgb(182, 134, 255),
                );

                ui.add_space(4.0);

                ui.label("Management agent");

                let width = ui.available_width();

                let receiver_response = ui.add_enabled(
                    endpoint_fields_editable,
                    egui::TextEdit::singleline(&mut self.receiver_agent).desired_width(width),
                );

                if receiver_response.changed()
                    && self.route_mode == ManagementRouteMode::AutomaticLan
                {
                    self.route_mode = ManagementRouteMode::ExplicitIp;
                }

                ui.label("Destination folder / batch root");

                let width = ui.available_width();

                ui.add(egui::TextEdit::singleline(&mut self.destination_root).desired_width(width));
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());

            ui.horizontal_wrapped(|ui| {
                ui.strong("Transfer options");

                ui.separator();

                ui.label("Scanner workers");

                ui.add(egui::DragValue::new(&mut self.worker_count).range(1..=64));

                ui.separator();

                ui.label("Calibration");

                ui.add(egui::DragValue::new(&mut self.calibration_mib).range(1..=4096));

                ui.label("MiB");

                ui.separator();

                ui.checkbox(
                    &mut self.update_existing,
                    "Update and verify existing destination",
                );
            });
        });

        ui.add_space(8.0);

        let batch_sources = self.batch_sources.clone();

        let batch_destination_root = self.destination_root.clone();

        let mut add_source_to_batch = false;

        let mut clear_batch = false;

        let mut remove_batch_index = None::<usize>;

        let mut add_batch_to_queue = false;

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());

            ui.horizontal_wrapped(|ui| {
                ui.strong("Batch queue builder");

                ui.separator();

                ui.label(
                    "Each source folder is placed beneath the receiver root using its final folder name.",
                );
            });

            ui.add_space(6.0);

            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.source_root.trim().is_empty(),
                        egui::Button::new("Add current source"),
                    )
                    .clicked()
                {
                    add_source_to_batch = true;
                }

                if ui
                    .add_enabled(
                        !batch_sources.is_empty(),
                        egui::Button::new("Clear source list"),
                    )
                    .clicked()
                {
                    clear_batch = true;
                }

                ui.label(format!(
                    "{} source folder(s) selected",
                    batch_sources.len(),
                ));
            });

            ui.add_space(6.0);

            if batch_sources.is_empty() {
                ui.label(
                    "Select or type a source folder, then click Add current source. Repeat for Desktop, Documents, Downloads, and the other folders you want.",
                );
            } else {
                for (index, source_root) in
                    batch_sources.iter().enumerate()
                {
                    let mapped_destination =
                        destination_layout::resolve_destination_text(
                            DestinationLayout::SourceNameUnderRoot,
                            source_root,
                            &batch_destination_root,
                        );

                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());

                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(source_root)
                                    .monospace(),
                            );

                            ui.label("→");

                            match &mapped_destination {
                                Ok(destination) => {
                                    ui.label(
                                        egui::RichText::new(destination)
                                            .monospace(),
                                    );
                                }

                                Err(error) => {
                                    ui.label(
                                        egui::RichText::new(
                                            error.to_string(),
                                        )
                                        .color(
                                            egui::Color32::from_rgb(
                                                255, 112, 120,
                                            ),
                                        ),
                                    );
                                }
                            }

                            if ui.small_button("Remove").clicked() {
                                remove_batch_index = Some(index);
                            }
                        });
                    });

                    ui.add_space(4.0);
                }
            }

            ui.add_space(6.0);

            let batch_ready = !batch_sources.is_empty()
                && !self.sender_agent.trim().is_empty()
                && !self.receiver_agent.trim().is_empty()
                && !self.destination_root.trim().is_empty()
                && self.sender_agent != self.receiver_agent;

            if ui
                .add_enabled(
                    batch_ready,
                    egui::Button::new(
                        egui::RichText::new(
                            "Add mapped batch to queue",
                        )
                        .strong(),
                    )
                    .fill(egui::Color32::from_rgb(42, 78, 72)),
                )
                .clicked()
            {
                add_batch_to_queue = true;
            }
        });

        if add_source_to_batch {
            self.add_current_source_to_batch();
        }

        if clear_batch {
            self.batch_sources.clear();

            self.notice = "Cleared the batch source list.".to_string();
        } else if let Some(index) = remove_batch_index
            && index < self.batch_sources.len()
        {
            let removed = self.batch_sources.remove(index);

            self.notice = format!("Removed batch source: {removed}",);
        }

        if add_batch_to_queue {
            self.add_batch_to_queue();
        }

        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            let label = if self.show_browsers {
                "Hide remote browser"
            } else {
                "Browse remote folders"
            };

            if ui.button(label).clicked() {
                self.show_browsers = !self.show_browsers;
            }

            if !self.show_browsers {
                ui.label("The source and destination paths can also be entered manually above.");
            }
        });

        if self.show_browsers {
            ui.add_space(8.0);

            let sender_agent = self.sender_agent.clone();

            let receiver_agent = self.receiver_agent.clone();

            ui.columns(2, |columns| {
                let (left, right) = columns.split_at_mut(1);

                left[0].push_id("sender_remote_browser", |ui| {
                    render_remote_browser(
                        ui,
                        "Sender folders",
                        &sender_agent,
                        &mut self.source_root,
                        &mut self.sender_browser,
                    );
                });

                right[0].push_id("receiver_remote_browser", |ui| {
                    render_remote_browser(
                        ui,
                        "Receiver folders",
                        &receiver_agent,
                        &mut self.destination_root,
                        &mut self.receiver_browser,
                    );
                });
            });
        }

        if !self.sender_agent.is_empty() && self.sender_agent == self.receiver_agent {
            ui.label("Sender and receiver cannot be the same agent.");
        }

        ui.add_space(8.0);

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let configuration_ready = !self.sender_agent.trim().is_empty()
            && !self.receiver_agent.trim().is_empty()
            && !self.source_root.trim().is_empty()
            && !self.destination_root.trim().is_empty()
            && self.sender_agent != self.receiver_agent;

        let can_start = self.start_receiver.is_none()
            && self.attach_receiver.is_none()
            && !self.queue_running
            && !transfer_active
            && configuration_ready;

        let can_queue = configuration_ready;

        let can_attach = self.attach_receiver.is_none()
            && self.start_receiver.is_none()
            && !self.queue_running
            && !transfer_active
            && !self.sender_agent.trim().is_empty()
            && !self.receiver_agent.trim().is_empty()
            && self.sender_agent != self.receiver_agent;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_start,
                    egui::Button::new(egui::RichText::new("Start managed transfer").strong())
                        .fill(egui::Color32::from_rgb(0, 112, 170)),
                )
                .clicked()
            {
                self.begin_transfer();
            }

            if ui
                .add_enabled(
                    can_queue,
                    egui::Button::new(egui::RichText::new("Add to queue").strong())
                        .fill(egui::Color32::from_rgb(42, 78, 72)),
                )
                .clicked()
            {
                self.add_current_to_queue();
            }

            if ui
                .add_enabled(
                    can_attach,
                    egui::Button::new(egui::RichText::new("Attach to active jobs").strong())
                        .fill(egui::Color32::from_rgb(42, 58, 78)),
                )
                .clicked()
            {
                self.begin_attach();
            }

            if self.start_receiver.is_some() {
                ui.spinner();

                ui.label("Starting endpoints...");
            }

            if self.attach_receiver.is_some() {
                ui.spinner();

                ui.label("Reading active jobs...");
            }
        });
    }

    fn render_transfer(&mut self, ui: &mut egui::Ui) {
        let (status, status_color) = self.manager_status();

        ui.horizontal_wrapped(|ui| {
            ui.heading("Current transfer");

            status_label(ui, status, status_color);
        });

        let Some(transfer) = self.transfer.clone() else {
            ui.label(
                "No active transfer. Select two endpoint agents and configure a source and destination.",
            );

            return;
        };

        ui.label(format!(
            "{}  →  {}",
            transfer.source_root, transfer.destination_root,
        ));

        ui.label(format!("Payload path: {}", transfer.receiver_payload,));

        ui.label(format!(
            "Sender job {} · Receiver job {}",
            transfer.sender_job_id, transfer.receiver_job_id,
        ));

        ui.add_space(8.0);

        ui.columns(2, |columns| {
            let (left, right) = columns.split_at_mut(1);

            render_endpoint_snapshot(
                &mut left[0],
                "Sender",
                transfer.sender_agent,
                transfer.sender_job_id,
                self.sender_snapshot.as_ref(),
            );

            render_endpoint_snapshot(
                &mut right[0],
                "Receiver",
                transfer.receiver_agent,
                transfer.receiver_job_id,
                self.receiver_snapshot.as_ref(),
            );
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let can_cancel = !self.monitoring_complete
                && self.cancel_receiver.is_none()
                && self.peer_cleanup_receiver.is_none();

            if ui
                .add_enabled(can_cancel, egui::Button::new("Cancel both endpoints"))
                .clicked()
            {
                self.begin_cancel();
            }

            if self.cancel_receiver.is_some() {
                ui.spinner();

                ui.label("Cancelling...");
            }

            if self.peer_cleanup_receiver.is_some() {
                ui.spinner();

                ui.label("Cleaning up paired endpoint...");
            }

            if self.monitoring_complete && ui.button("Clear transfer card").clicked() {
                self.clear_transfer_card();
            }
        });
    }

    fn render_queue(&mut self, ui: &mut egui::Ui) {
        let mut paused_after_current = self.queue.paused_after_current();

        let has_pending = self
            .queue
            .items()
            .iter()
            .any(|item| item.state == QueuedTransferState::Pending);

        let has_interrupted_items = self.queue.items().iter().any(|item| {
            matches!(
                item.state,
                QueuedTransferState::Blocked
                    | QueuedTransferState::Failed
                    | QueuedTransferState::Cancelled
            )
        });

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let can_start_queue = !self.queue_running
            && has_pending
            && self.start_receiver.is_none()
            && self.attach_receiver.is_none()
            && self.poll_receiver.is_none()
            && self.cancel_receiver.is_none()
            && self.peer_cleanup_receiver.is_none()
            && !transfer_active;

        ui.horizontal_wrapped(|ui| {
            let start_label = if has_interrupted_items {
                "Continue queue"
            } else {
                "Start queue"
            };

            if ui
                .add_enabled(
                    can_start_queue,
                    egui::Button::new(
                        egui::RichText::new(start_label).strong(),
                    )
                    .fill(egui::Color32::from_rgb(0, 112, 170)),
                )
                .clicked()
            {
                self.start_queue();
            }

            if self.queue_running {
                status_label(
                    ui,
                    "Queue running",
                    egui::Color32::from_rgb(126, 230, 64),
                );
            }

            ui.label(
                "Queued transfers are retained across manager restarts and run in their displayed order.",
            );

            if ui
                .checkbox(
                    &mut paused_after_current,
                    "Pause after current transfer",
                )
                .changed()
            {
                self.queue
                    .set_paused_after_current(paused_after_current);
            }

            if ui
                .add_enabled(
                    self.queue
                        .items()
                        .iter()
                        .any(|item| item.state == QueuedTransferState::Completed),
                    egui::Button::new("Clear completed"),
                )
                .clicked()
            {
                let removed = self.queue.clear_completed();

                self.notice = format!(
                    "Removed {removed} completed queued transfer(s).",
                );
            }
        });

        ui.add_space(6.0);

        if self.queue.is_empty() {
            ui.label("The queue is empty. Configure a transfer and click Add to queue.");

            return;
        }

        let items = self.queue.items().to_vec();

        let mut move_up = None::<QueuedTransferId>;

        let mut move_down = None::<QueuedTransferId>;

        let mut remove = None::<QueuedTransferId>;

        let mut retry = None::<QueuedTransferId>;

        let mut skip = None::<QueuedTransferId>;

        for (index, item) in items.iter().enumerate() {
            let previous_running = index
                .checked_sub(1)
                .is_some_and(|previous| items[previous].state == QueuedTransferState::Running);

            let next_running = items
                .get(index + 1)
                .is_some_and(|next| next.state == QueuedTransferState::Running);

            let item_running = item.state == QueuedTransferState::Running;

            let can_retry = !self.queue_running
                && matches!(
                    item.state,
                    QueuedTransferState::Blocked
                        | QueuedTransferState::Failed
                        | QueuedTransferState::Completed
                        | QueuedTransferState::Cancelled
                );

            let can_skip = matches!(
                item.state,
                QueuedTransferState::Pending
                    | QueuedTransferState::Blocked
                    | QueuedTransferState::Failed
            );

            let can_move_up = index > 0 && !item_running && !previous_running;

            let can_move_down = index + 1 < items.len() && !item_running && !next_running;

            ui.group(|ui| {
                ui.set_min_width(ui.available_width());

                ui.horizontal_wrapped(|ui| {
                    status_label(
                        ui,
                        queued_transfer_state_label(item.state),
                        queued_transfer_state_color(item.state),
                    );

                    ui.strong(format!(
                        "#{} · {}",
                        item.id,
                        queued_transfer_kind_label(item.request.kind),
                    ));

                    ui.separator();

                    ui.label(item.request.route_mode.label());
                });

                ui.add_space(4.0);

                ui.label(format!(
                    "{}  →  {}",
                    item.request.source_root, item.request.destination_root,
                ));

                ui.label(
                    egui::RichText::new(format!(
                        "{}  →  {}",
                        item.request.sender_agent, item.request.receiver_agent,
                    ))
                    .monospace(),
                );

                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} worker(s)", item.request.worker_count,));

                    ui.separator();

                    ui.label(format!("{} MiB calibration", item.request.calibration_mib,));

                    ui.separator();

                    ui.label(if item.request.update_existing {
                        "Update mode"
                    } else {
                        "Fresh destination mode"
                    });
                });

                if !item.status_message.is_empty() {
                    ui.add_space(4.0);

                    ui.label(&item.status_message);
                }

                ui.add_space(6.0);

                ui.horizontal_wrapped(|ui| {
                    let retry_label = if item.state == QueuedTransferState::Completed {
                        "Run again"
                    } else {
                        "Retry"
                    };

                    if ui
                        .add_enabled(can_retry, egui::Button::new(retry_label))
                        .clicked()
                    {
                        retry = Some(item.id);
                    }

                    if ui
                        .add_enabled(can_skip, egui::Button::new("Skip"))
                        .clicked()
                    {
                        skip = Some(item.id);
                    }

                    if ui
                        .add_enabled(can_move_up, egui::Button::new("Move up"))
                        .clicked()
                    {
                        move_up = Some(item.id);
                    }

                    if ui
                        .add_enabled(can_move_down, egui::Button::new("Move down"))
                        .clicked()
                    {
                        move_down = Some(item.id);
                    }

                    if ui
                        .add_enabled(!item_running, egui::Button::new("Remove"))
                        .clicked()
                    {
                        remove = Some(item.id);
                    }

                    if item_running {
                        ui.label("A running queue item cannot be moved or removed.");
                    }
                });
            });

            ui.add_space(8.0);
        }

        if let Some(id) = retry {
            self.retry_queue_item(id);
        } else if let Some(id) = skip {
            if self.queue.skip(id) {
                self.notice = format!("Queued transfer #{id} was skipped.");

                self.error.clear();
            }
        } else if let Some(id) = move_up {
            if self.queue.move_up(id) {
                self.notice = format!("Moved queued transfer #{id} up.");
            }
        } else if let Some(id) = move_down {
            if self.queue.move_down(id) {
                self.notice = format!("Moved queued transfer #{id} down.");
            }
        } else if let Some(id) = remove
            && self.queue.remove(id).is_some()
        {
            self.notice = format!("Removed queued transfer #{id}.");
        }
    }

    fn render_history(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                "Completed, cancelled, and failed paired transfers are retained across manager restarts.",
            );

            if ui
                .add_enabled(!self.history.is_empty(), egui::Button::new("Clear history"))
                .clicked()
            {
                self.history.clear();

                self.show_history = false;

                self.notice = "Transfer history cleared.".to_string();
            }
        });

        ui.add_space(6.0);

        if self.history.is_empty() {
            ui.label("No terminal paired transfers have been recorded in this manager session.");

            return;
        }

        let entries = self.history.iter().cloned().collect::<Vec<_>>();

        let mut reuse = None::<ManagedTransferRecord>;

        let mut resume = None::<PairedTransferHistoryEntry>;

        let mut queue_resume = None::<PairedTransferHistoryEntry>;

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let can_resume = self.start_receiver.is_none()
            && self.attach_receiver.is_none()
            && self.cancel_receiver.is_none()
            && !self.queue_running
            && !transfer_active;

        for entry in entries {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());

                let outcome = entry.outcome();

                ui.horizontal_wrapped(|ui| {
                    ui.strong(
                        egui::RichText::new(format!("Result: {}", outcome.label(),))
                            .color(outcome_color(outcome)),
                    );

                    ui.separator();

                    ui.label(format!("Sender job {}", entry.transfer.sender_job_id,));

                    ui.label(format!("Receiver job {}", entry.transfer.receiver_job_id,));
                });

                ui.add_space(6.0);

                ui.label(format!(
                    "{}  →  {}",
                    entry.transfer.source_root, entry.transfer.destination_root,
                ));

                ui.label(format!(
                    "{}  →  {}",
                    entry.transfer.sender_agent, entry.transfer.receiver_agent,
                ));

                ui.add_space(8.0);

                egui::Grid::new(format!(
                    "history-{}-{}",
                    entry.transfer.sender_job_id, entry.transfer.receiver_job_id,
                ))
                .num_columns(2)
                .spacing([24.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Files");

                    ui.strong(entry.files().to_string());

                    ui.end_row();

                    ui.label("Logical data");

                    ui.strong(format_bytes(entry.logical_bytes()));

                    ui.end_row();

                    ui.label("Wire data");

                    ui.strong(format_bytes(entry.wire_bytes()));

                    ui.end_row();

                    if let Some(savings) =
                        wire_savings_percent(entry.logical_bytes(), entry.wire_bytes())
                    {
                        ui.label("Wire savings");

                        ui.strong(format!("{savings:.2}%"));

                        ui.end_row();
                    }

                    ui.label("Data streams");

                    ui.strong(entry.data_stream_count().to_string());

                    ui.end_row();

                    ui.label("Update mode");

                    ui.strong(if entry.transfer.update_existing {
                        "enabled"
                    } else {
                        "disabled"
                    });

                    ui.end_row();

                    ui.label("Scanner workers");

                    ui.strong(entry.transfer.worker_count.to_string());

                    ui.end_row();

                    ui.label("Calibration");

                    ui.strong(format!("{} MiB", entry.transfer.calibration_mib,));

                    ui.end_row();
                });

                if !entry.sender_result.message.is_empty() {
                    ui.add_space(6.0);

                    ui.label(format!("Sender: {}", entry.sender_result.message,));
                }

                if !entry.receiver_result.message.is_empty()
                    && entry.receiver_result.message != entry.sender_result.message
                {
                    ui.add_space(4.0);

                    ui.label(format!("Receiver: {}", entry.receiver_result.message,));
                }

                ui.add_space(8.0);

                if ui.button("Use this setup again").clicked() {
                    reuse = Some(entry.transfer.clone());
                }

                if let Some(data_stream_count) = entry.resume_data_stream_count() {
                    if ui
                        .add_enabled(
                            can_resume,
                            egui::Button::new(format!(
                                "Resume interrupted transfer ({data_stream_count} streams)"
                            )),
                        )
                        .clicked()
                    {
                        resume = Some(entry.clone());
                    }

                    if ui
                        .button(format!("Add resume to queue ({data_stream_count} streams)"))
                        .clicked()
                    {
                        queue_resume = Some(entry.clone());
                    }
                } else if outcome != ManagementJobOutcome::Completed {
                    ui.label("Receiver resume journal unavailable.");
                }
            });

            ui.add_space(8.0);
        }

        if let Some(entry) = resume {
            self.begin_resumed_transfer(entry);
        } else if let Some(entry) = queue_resume {
            self.add_resume_to_queue(entry);
        } else if let Some(transfer) = reuse {
            self.sender_agent = transfer.sender_agent.to_string();

            self.receiver_agent = transfer.receiver_agent.to_string();

            self.source_root = transfer.source_root;

            self.destination_root = transfer.destination_root;

            self.update_existing = transfer.update_existing;

            self.worker_count = transfer.worker_count;

            self.calibration_mib = transfer.calibration_mib;

            self.show_setup = true;

            self.notice =
                "The selected history entry was copied into the transfer setup.".to_string();
        }
    }
}

impl eframe::App for NetworkCopyManager {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.update_check_started {
            self.update_check_started = true;

            self.begin_update_check();
        }

        self.process_messages();

        self.begin_poll();

        if self.has_background_work() {
            ui.ctx().request_repaint_after(REPAINT_INTERVAL);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());

                    self.render_app_header(ui);

                    ui.add_space(10.0);

                    self.render_messages(ui);

                    let show_transfer_panel = self.transfer.is_some()
                        || self.start_receiver.is_some()
                        || self.attach_receiver.is_some()
                        || self.cancel_receiver.is_some()
                        || self.peer_cleanup_receiver.is_some();

                    if show_transfer_panel {
                        ui.add_space(12.0);

                        ui.group(|ui| {
                            ui.set_min_width(ui.available_width());

                            self.render_transfer(ui);
                        });

                        ui.add_space(10.0);
                    } else {
                        ui.add_space(8.0);
                    }

                    let agent_summary = format!("{} discovered", self.agents.len());

                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());

                        render_section_toggle(
                            ui,
                            "LAN agents",
                            &agent_summary,
                            &mut self.show_agents,
                        );

                        if self.show_agents {
                            ui.add_space(8.0);

                            self.render_discovery(ui);
                        }
                    });

                    ui.add_space(10.0);

                    let setup_summary = if self.sender_agent.trim().is_empty()
                        || self.receiver_agent.trim().is_empty()
                    {
                        "Choose sender and receiver".to_string()
                    } else if self.source_root.trim().is_empty()
                        || self.destination_root.trim().is_empty()
                    {
                        "Choose source and destination".to_string()
                    } else {
                        format!("{} · endpoints and paths selected", self.route_mode.label(),)
                    };

                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());

                        render_section_toggle(
                            ui,
                            "Transfer setup",
                            &setup_summary,
                            &mut self.show_setup,
                        );

                        if self.show_setup {
                            ui.add_space(8.0);

                            self.render_configuration(ui);
                        }
                    });

                    ui.add_space(10.0);

                    let pending_queue_items = self
                        .queue
                        .items()
                        .iter()
                        .filter(|item| item.state == QueuedTransferState::Pending)
                        .count();

                    let queue_summary = if self.queue.is_empty() {
                        "Empty".to_string()
                    } else if self.queue_running {
                        format!(
                            "{} item(s) · {} pending · running",
                            self.queue.len(),
                            pending_queue_items,
                        )
                    } else if self.queue.paused_after_current() {
                        format!(
                            "{} item(s) · {} pending · paused",
                            self.queue.len(),
                            pending_queue_items,
                        )
                    } else {
                        format!(
                            "{} item(s) · {} pending",
                            self.queue.len(),
                            pending_queue_items,
                        )
                    };

                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());

                        render_section_toggle(
                            ui,
                            "Transfer queue",
                            &queue_summary,
                            &mut self.show_queue,
                        );

                        if self.show_queue {
                            ui.add_space(8.0);

                            self.render_queue(ui);
                        }
                    });

                    ui.add_space(10.0);

                    let history_summary =
                        format!("{} / {} retained", self.history.len(), MAX_TRANSFER_HISTORY,);

                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());

                        render_section_toggle(
                            ui,
                            "Transfer history",
                            &history_summary,
                            &mut self.show_history,
                        );

                        if self.show_history {
                            ui.add_space(8.0);

                            self.render_history(ui);
                        }
                    });

                    ui.add_space(10.0);

                    if let Some(path) = &self.state_path {
                        ui.label(
                            egui::RichText::new(format!("Manager state: {}", path.display(),))
                                .small()
                                .monospace(),
                        );
                    }

                    ui.add_space(16.0);
                });
        });

        self.persist_state_if_needed(ui.ctx());
    }
}

fn notify_manager(kind: NotificationKind, title: &str, body: &str) {
    if let Err(error) = windows_notification::show(kind, title, body) {
        eprintln!("Failed to start Windows notification: {error}",);
    }
}

fn build_direct_management_routes(
    discovered: Vec<DirectDiscoveredAgent>,
) -> Result<Vec<DirectManagementRoute>, String> {
    let mut routes = Vec::new();

    for discovered in discovered {
        let local_hello =
            management_control::hello(discovered.local_endpoint)
                .map_err(|error| {
                    format!(
                        "The local management agent did not answer at {} for Direct Link interface {}: {error}",
                        discovered.local_endpoint,
                        discovered.interface_index,
                    )
                })?;

        let local_agent = DiscoveredAgent {
            hostname: local_hello.hostname,

            endpoint: discovered.local_endpoint,

            protocol_version: local_hello.protocol_version,

            state: local_hello.state,

            capabilities: local_hello.capabilities,
        };

        if local_agent.endpoint == discovered.agent.endpoint {
            return Err(format!(
                "Direct Link interface {} returned the same local and peer management endpoint.",
                discovered.interface_index,
            ));
        }

        routes.push(DirectManagementRoute {
            interface_index: discovered.interface_index,

            local_agent,

            peer_agent: discovered.agent,
        });
    }

    routes.sort_by(|left, right| {
        left.interface_index
            .cmp(&right.interface_index)
            .then_with(|| left.peer_agent.hostname.cmp(&right.peer_agent.hostname))
    });

    routes.dedup_by(|left, right| {
        left.interface_index == right.interface_index
            && left.local_agent.endpoint == right.local_agent.endpoint
            && left.peer_agent.endpoint == right.peer_agent.endpoint
    });

    if routes.is_empty() {
        return Err("No usable Direct Link management route was discovered.".to_string());
    }

    Ok(routes)
}

fn resolve_management_endpoints(
    route_mode: ManagementRouteMode,
    sender_text: &str,
    receiver_text: &str,
    agents: &[DiscoveredAgent],
    direct_routes: &[DirectManagementRoute],
) -> Result<(SocketAddr, SocketAddr), String> {
    let sender_agent = parse_endpoint(sender_text, "sender management agent")?;

    let receiver_agent = parse_endpoint(receiver_text, "receiver management agent")?;

    if sender_agent == receiver_agent {
        return Err("Sender and receiver must be different management agents.".to_string());
    }

    match route_mode {
        ManagementRouteMode::ExplicitIp => Ok((sender_agent, receiver_agent)),

        ManagementRouteMode::AutomaticLan => {
            let sender = agents
                .iter()
                .find(|agent| {
                    agent.endpoint
                        == sender_agent
                })
                .ok_or_else(|| {
                    "Automatic LAN requires the sender to be selected from the discovered-agent list."
                        .to_string()
                })?;

            if !sender.capabilities.can_send() {
                return Err(format!(
                    "Discovered agent {} cannot act as a sender.",
                    sender.hostname,
                ));
            }

            let receiver = agents
                .iter()
                .find(|agent| {
                    agent.endpoint
                        == receiver_agent
                })
                .ok_or_else(|| {
                    "Automatic LAN requires the receiver to be selected from the discovered-agent list."
                        .to_string()
                })?;

            if !receiver.capabilities.can_receive() {
                return Err(format!(
                    "Discovered agent {} cannot act as a receiver.",
                    receiver.hostname,
                ));
            }

            Ok((sender_agent, receiver_agent))
        }

        ManagementRouteMode::DirectLink => {
            let route = direct_routes
                .iter()
                .find(|route| {
                    route.local_agent.endpoint
                        == sender_agent
                        && route.peer_agent.endpoint
                            == receiver_agent
                })
                .ok_or_else(|| {
                    "Direct Link requires a matching local-sender and cable-peer receiver route. Refresh Direct Link discovery and select a route."
                        .to_string()
                })?;

            if !route.local_agent.capabilities.can_send() {
                return Err(format!(
                    "Local Direct Link agent {} cannot act as a sender.",
                    route.local_agent.hostname,
                ));
            }

            if !route.peer_agent.capabilities.can_receive() {
                return Err(format!(
                    "Direct Link peer {} cannot act as a receiver.",
                    route.peer_agent.hostname,
                ));
            }

            Ok((sender_agent, receiver_agent))
        }
    }
}

fn select_or_clear_discovered_endpoint(
    selected_role: &mut String,
    other_role: &mut String,
    endpoint: &str,
) {
    if selected_role == endpoint && other_role != endpoint {
        selected_role.clear();

        return;
    }

    selected_role.clear();

    selected_role.push_str(endpoint);

    if other_role == endpoint {
        other_role.clear();
    }
}

fn comparable_windows_path(path: &str) -> String {
    path.trim()
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_lowercase()
}

#[allow(clippy::too_many_arguments)]
fn build_batch_queue_requests(
    sender_agent: SocketAddr,
    receiver_agent: SocketAddr,
    route_mode: ManagementRouteMode,
    source_roots: &[String],
    destination_root: &str,
    update_existing: bool,
    worker_count: usize,
    calibration_mib: u64,
) -> Result<Vec<QueuedTransferRequest>, String> {
    if source_roots.is_empty() {
        return Err("Add at least one source folder to the batch.".to_string());
    }

    let destination_root = destination_root.trim();

    if destination_root.is_empty() {
        return Err("Select or enter one destination root for the batch.".to_string());
    }

    let mut source_keys = HashSet::new();

    let mut destination_keys = HashSet::new();

    let mut requests = Vec::with_capacity(source_roots.len());

    for source_root in source_roots {
        let source_root = source_root.trim().to_string();

        if source_root.is_empty() {
            return Err("A batch source folder must not be empty.".to_string());
        }

        let source_key = comparable_windows_path(&source_root);

        if !source_keys.insert(source_key) {
            return Err(format!(
                "The batch contains the same source folder more than once: {source_root}",
            ));
        }

        let resolved_destination = destination_layout::resolve_destination_text(
            DestinationLayout::SourceNameUnderRoot,
            &source_root,
            destination_root,
        )
        .map_err(|error| format!("Could not map batch source {source_root}: {error}",))?;

        let destination_key = comparable_windows_path(&resolved_destination);

        if !destination_keys.insert(destination_key) {
            return Err(format!(
                "Two batch sources would map to the same destination folder: {resolved_destination}",
            ));
        }

        let request = QueuedTransferRequest {
            sender_agent,

            receiver_agent,

            route_mode,

            source_root,

            destination_root: resolved_destination,

            update_existing,

            worker_count,

            calibration_mib,

            kind: QueuedTransferKind::Fresh,
        };

        request
            .validate()
            .map_err(|error| format!("Generated batch transfer was invalid: {error}"))?;

        requests.push(request);
    }

    Ok(requests)
}

fn active_queue_binding_for_snapshots(
    queue_id: QueuedTransferId,
    transfer: &ManagedTransferRecord,
    sender_snapshot: &ManagementAgentSnapshot,
    receiver_snapshot: &ManagementAgentSnapshot,
) -> Result<ActiveQueueBinding, String> {
    let binding = ActiveQueueBinding::new(
        queue_id,
        sender_snapshot.agent_instance_id,
        transfer.sender_job_id,
        receiver_snapshot.agent_instance_id,
        transfer.receiver_job_id,
    )
    .map_err(|error| format!("exact queue binding was invalid: {error}",))?;

    binding
        .validate_active_snapshots(sender_snapshot, receiver_snapshot)
        .map_err(|error| format!("endpoint snapshots did not match the new binding: {error}",))?;

    Ok(binding)
}

fn select_bound_recovery_item(queue: &mut TransferQueue) -> Result<Option<QueuedTransfer>, String> {
    let Some(binding) = queue.active_binding() else {
        let running_ids = queue
            .items()
            .iter()
            .filter(|item| item.state == QueuedTransferState::Running)
            .map(|item| item.id)
            .collect::<Vec<_>>();

        for id in running_ids {
            queue.set_state(
                id,
                QueuedTransferState::Blocked,
                "The Manager restarted without an exact endpoint binding. Automatic reattachment was refused.",
            )?;
        }

        return Ok(None);
    };

    let extra_running_ids = queue
        .items()
        .iter()
        .filter(|item| item.state == QueuedTransferState::Running && item.id != binding.queue_id)
        .map(|item| item.id)
        .collect::<Vec<_>>();

    for id in extra_running_ids {
        queue.set_state(
            id,
            QueuedTransferState::Blocked,
            "Another queue item retained the exact endpoint binding. This extra Running item was blocked.",
        )?;
    }

    let item = queue
        .items()
        .iter()
        .find(|item| item.id == binding.queue_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "exact endpoint binding references missing queue item #{}",
                binding.queue_id,
            )
        })?;

    Ok(Some(item))
}

fn preflight_queue_route(route_mode: ManagementRouteMode) -> Result<(), ManagedStartFailure> {
    match route_mode {
        ManagementRouteMode::AutomaticLan
        | ManagementRouteMode::DirectLink
        | ManagementRouteMode::ExplicitIp => Ok(()),
    }
}

fn preflight_queue_endpoints(request: &ManagedTransferRequest) -> Result<(), ManagedStartFailure> {
    preflight_queue_endpoint("Sender", request.sender_agent)?;

    preflight_queue_endpoint("Receiver", request.receiver_agent)
}

fn preflight_queue_endpoint(role: &str, endpoint: SocketAddr) -> Result<(), ManagedStartFailure> {
    let agent = management_control::hello(endpoint).map_err(|error| {
        ManagedStartFailure::blocked(format!("{role} agent {endpoint} is unavailable: {error}",))
    })?;

    match agent.state {
        AgentState::Idle => Ok(()),

        AgentState::Busy => Err(ManagedStartFailure::blocked(format!(
            "{role} agent {} at {endpoint} is busy with another operation.",
            agent.hostname,
        ))),
    }
}

const fn queue_state_for_start_failure(kind: ManagedStartFailureKind) -> QueuedTransferState {
    match kind {
        ManagedStartFailureKind::Blocked => QueuedTransferState::Blocked,

        ManagedStartFailureKind::Failed => QueuedTransferState::Failed,
    }
}

fn transfer_matches_queue_request(
    request: &QueuedTransferRequest,
    transfer: &ManagedTransferRecord,
) -> bool {
    request.sender_agent == transfer.sender_agent
        && request.receiver_agent == transfer.receiver_agent
        && request.source_root == transfer.source_root
        && request.destination_root == transfer.destination_root
        && request.update_existing == transfer.update_existing
        && request.worker_count == transfer.worker_count
        && request.calibration_mib == transfer.calibration_mib
}

const fn queue_state_for_outcome(outcome: ManagementJobOutcome) -> QueuedTransferState {
    match outcome {
        ManagementJobOutcome::Completed => QueuedTransferState::Completed,

        ManagementJobOutcome::Cancelled => QueuedTransferState::Cancelled,

        ManagementJobOutcome::Failed => QueuedTransferState::Failed,
    }
}

fn queued_transfer_kind_label(kind: QueuedTransferKind) -> String {
    match kind {
        QueuedTransferKind::Fresh => "Fresh transfer".to_string(),

        QueuedTransferKind::Resume { data_stream_count } => {
            format!("Resume · {data_stream_count} streams")
        }
    }
}

const fn queued_transfer_state_label(state: QueuedTransferState) -> &'static str {
    match state {
        QueuedTransferState::Pending => "Pending",
        QueuedTransferState::Running => "Running",
        QueuedTransferState::Blocked => "Blocked",
        QueuedTransferState::Failed => "Failed",
        QueuedTransferState::Completed => "Completed",
        QueuedTransferState::Cancelled => "Cancelled",
    }
}

fn queued_transfer_state_color(state: QueuedTransferState) -> egui::Color32 {
    match state {
        QueuedTransferState::Pending => egui::Color32::from_rgb(95, 194, 255),

        QueuedTransferState::Running => egui::Color32::from_rgb(126, 230, 64),

        QueuedTransferState::Blocked => egui::Color32::from_rgb(255, 190, 82),

        QueuedTransferState::Failed => egui::Color32::from_rgb(255, 112, 120),

        QueuedTransferState::Completed => egui::Color32::from_rgb(126, 230, 64),

        QueuedTransferState::Cancelled => egui::Color32::from_rgb(255, 190, 82),
    }
}

fn render_section_toggle(ui: &mut egui::Ui, title: &str, summary: &str, open: &mut bool) {
    ui.horizontal_wrapped(|ui| {
        let toggle = if *open { "-" } else { "+" };

        if ui.small_button(toggle).clicked() {
            *open = !*open;
        }

        ui.heading(title);

        if !summary.is_empty() {
            ui.label(summary);
        }
    });
}

fn status_label(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.label(
        egui::RichText::new(text.to_ascii_uppercase())
            .color(color)
            .strong(),
    );
}

fn agent_state_color(state: &str) -> egui::Color32 {
    match state {
        "idle" => egui::Color32::from_rgb(126, 230, 64),

        "busy" => egui::Color32::from_rgb(255, 190, 82),

        _ => egui::Color32::from_rgb(160, 170, 184),
    }
}

fn terminal_result_for(
    snapshot: Option<&ManagementAgentSnapshot>,
    job_id: u64,
) -> Option<&ManagementJobResult> {
    snapshot?
        .latest_result
        .as_ref()
        .filter(|result| result.job_id == job_id)
}

fn paired_outcome(
    sender: &ManagementJobResult,
    receiver: &ManagementJobResult,
) -> ManagementJobOutcome {
    if sender.outcome == ManagementJobOutcome::Failed
        || receiver.outcome == ManagementJobOutcome::Failed
    {
        ManagementJobOutcome::Failed
    } else if sender.outcome == ManagementJobOutcome::Cancelled
        || receiver.outcome == ManagementJobOutcome::Cancelled
    {
        ManagementJobOutcome::Cancelled
    } else {
        ManagementJobOutcome::Completed
    }
}

fn same_transfer_identity(left: &ManagedTransferRecord, right: &ManagedTransferRecord) -> bool {
    left.sender_agent == right.sender_agent
        && left.sender_job_id == right.sender_job_id
        && left.receiver_agent == right.receiver_agent
        && left.receiver_job_id == right.receiver_job_id
}

fn remember_history(
    history: &mut VecDeque<PairedTransferHistoryEntry>,
    entry: PairedTransferHistoryEntry,
) {
    if history
        .iter()
        .any(|existing| same_transfer_identity(&existing.transfer, &entry.transfer))
    {
        return;
    }

    history.push_front(entry);

    history.truncate(MAX_TRANSFER_HISTORY);
}

fn wire_savings_percent(logical_bytes: u64, wire_bytes: u64) -> Option<f64> {
    if logical_bytes == 0 {
        return None;
    }

    Some(100.0 - wire_bytes as f64 / logical_bytes as f64 * 100.0)
}

fn outcome_color(outcome: ManagementJobOutcome) -> egui::Color32 {
    match outcome {
        ManagementJobOutcome::Completed => egui::Color32::from_rgb(126, 230, 64),

        ManagementJobOutcome::Cancelled => egui::Color32::from_rgb(255, 190, 82),

        ManagementJobOutcome::Failed => egui::Color32::from_rgb(255, 112, 120),
    }
}

fn render_remote_browser(
    ui: &mut egui::Ui,
    title: &str,
    endpoint_text: &str,
    selected_path: &mut String,
    browser: &mut RemoteBrowserPane,
) {
    let parsed_endpoint = endpoint_text.trim().parse::<SocketAddr>().ok();

    browser.sync_endpoint(parsed_endpoint);

    let mut request_roots = false;

    let mut request_directory = None::<String>;

    let mut use_current = false;

    ui.group(|ui| {
        ui.set_min_width(ui.available_width());

        ui.set_min_height(REMOTE_BROWSER_HEIGHT + 120.0);

        ui.heading(title);

        if endpoint_text.trim().is_empty() {
            ui.label("Select or enter a management agent first.");

            return;
        }

        if parsed_endpoint.is_none() {
            ui.label("The management agent address is invalid.");

            return;
        }

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!browser.is_loading(), egui::Button::new("Load drives"))
                .clicked()
            {
                request_roots = true;
            }

            let can_go_up = !browser.current_path.is_empty();

            if ui
                .add_enabled(can_go_up && !browser.is_loading(), egui::Button::new("Up"))
                .clicked()
            {
                match parent_remote_path(&browser.current_path) {
                    Some(parent) => {
                        request_directory = Some(parent);
                    }

                    None => {
                        request_roots = true;
                    }
                }
            }

            let can_refresh = browser.endpoint.is_some() && !browser.is_loading();

            if ui
                .add_enabled(can_refresh, egui::Button::new("Refresh"))
                .clicked()
            {
                if browser.current_path.is_empty() {
                    request_roots = true;
                } else {
                    request_directory = Some(browser.current_path.clone());
                }
            }

            if ui
                .add_enabled(
                    !browser.current_path.is_empty(),
                    egui::Button::new("Use current folder"),
                )
                .clicked()
            {
                use_current = true;
            }
        });

        if browser.is_loading() {
            ui.horizontal(|ui| {
                ui.spinner();

                ui.label("Loading remote directory...");
            });
        }

        if !browser.error.is_empty() {
            ui.label(
                egui::RichText::new(&browser.error).color(egui::Color32::from_rgb(255, 112, 120)),
            );
        }

        ui.add_space(6.0);

        if browser.current_path.is_empty() {
            ui.strong("Remote drives");

            if browser.roots.is_empty() && !browser.is_loading() {
                ui.label("Click Load drives to browse this machine.");
            }

            let roots = browser.roots.clone();

            for root in roots {
                if ui.button(&root.path).clicked() {
                    request_directory = Some(root.path);
                }
            }

            return;
        }

        ui.label(egui::RichText::new(&browser.current_path).strong());

        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .max_height(REMOTE_BROWSER_HEIGHT)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let entries = browser.entries.clone();

                if entries.is_empty() && !browser.is_loading() {
                    ui.label("This directory is empty.");
                }

                for entry in entries {
                    match entry.kind {
                        ManagementEntryKind::Directory => {
                            let label = format!("DIR  {}", entry.name,);

                            if ui.button(label).clicked() {
                                request_directory =
                                    Some(join_remote_path(&browser.current_path, &entry.name));
                            }
                        }

                        ManagementEntryKind::File => {
                            ui.horizontal(|ui| {
                                ui.label(entry.name);

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(format_bytes(entry.size));
                                    },
                                );
                            });
                        }

                        ManagementEntryKind::Other => {
                            ui.label(format!("OTHER  {}", entry.name,));
                        }
                    }
                }
            });
    });

    if use_current {
        *selected_path = browser.current_path.clone();
    }

    if request_roots && let Err(error) = browser.begin_roots(endpoint_text) {
        browser.error = error;
    }

    if let Some(path) = request_directory
        && let Err(error) = browser.begin_directory(endpoint_text, path)
    {
        browser.error = error;
    }
}

fn join_remote_path(parent: &str, child: &str) -> String {
    Path::new(parent).join(child).to_string_lossy().into_owned()
}

fn parent_remote_path(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy().into_owned())
}

fn render_endpoint_snapshot(
    ui: &mut egui::Ui,
    title: &str,
    endpoint: SocketAddr,
    expected_job_id: u64,
    snapshot: Option<&ManagementAgentSnapshot>,
) {
    ui.group(|ui| {
        ui.heading(title);

        ui.label(endpoint.to_string());

        let Some(snapshot) = snapshot else {
            ui.spinner();

            ui.label("Waiting for first snapshot...");

            return;
        };

        if let Some(active) = &snapshot.active
            && active.job_id == expected_job_id
        {
            ui.strong(format!("{} job {}", active.role.label(), active.job_id,));

            ui.label(&active.phase);

            if active.total == 0 {
                ui.horizontal(|ui| {
                    ui.spinner();

                    ui.label(format!("{} processed", format_bytes(active.completed),));
                });
            } else {
                let fraction = active.completed.min(active.total) as f64 / active.total as f64;

                ui.add(
                    egui::ProgressBar::new(fraction as f32)
                        .show_percentage()
                        .text(format!(
                            "{} / {}",
                            format_bytes(active.completed),
                            format_bytes(active.total),
                        )),
                );
            }

            if active.cancel_requested {
                ui.label("Cancellation requested");
            }
        }

        let result = snapshot
            .latest_result
            .as_ref()
            .filter(|result| result.job_id == expected_job_id);

        if let Some(result) = result {
            ui.separator();

            status_label(
                ui,
                &format!("Result: {}", result.outcome.label(),),
                outcome_color(result.outcome),
            );

            ui.label(format!("Files: {}", result.files,));

            ui.label(format!(
                "Logical data: {}",
                format_bytes(result.logical_bytes,),
            ));

            if result.wire_bytes > 0 {
                ui.label(format!("Wire data: {}", format_bytes(result.wire_bytes,),));
            }

            if result.data_stream_count > 0 {
                ui.label(format!("Data streams: {}", result.data_stream_count,));
            }

            if !result.message.is_empty() {
                ui.label(format!("Message: {}", result.message,));
            }
        } else if snapshot.active.is_none() {
            ui.label("No retained result for this job yet.");
        }
    });
}

fn peer_cleanup_target(
    transfer: &ManagedTransferRecord,
    sender_snapshot: Option<&ManagementAgentSnapshot>,
    receiver_snapshot: Option<&ManagementAgentSnapshot>,
) -> Option<PeerCleanupTarget> {
    let sender_result = terminal_result_for(sender_snapshot, transfer.sender_job_id)
        .filter(|result| result.outcome != ManagementJobOutcome::Completed);

    let receiver_result = terminal_result_for(receiver_snapshot, transfer.receiver_job_id)
        .filter(|result| result.outcome != ManagementJobOutcome::Completed);

    let sender_active = snapshot_has_active_job(sender_snapshot, transfer.sender_job_id);

    let receiver_active = snapshot_has_active_job(receiver_snapshot, transfer.receiver_job_id);

    if let Some(result) = sender_result
        && receiver_active
    {
        return Some(PeerCleanupTarget {
            endpoint_role: ManagedEndpointRole::Receiver,

            endpoint: transfer.receiver_agent,

            job_id: transfer.receiver_job_id,

            trigger_role: ManagementJobRole::Sender,

            trigger_job_id: result.job_id,

            trigger_outcome: result.outcome,
        });
    }

    if let Some(result) = receiver_result
        && sender_active
    {
        return Some(PeerCleanupTarget {
            endpoint_role: ManagedEndpointRole::Sender,

            endpoint: transfer.sender_agent,

            job_id: transfer.sender_job_id,

            trigger_role: ManagementJobRole::Receiver,

            trigger_job_id: result.job_id,

            trigger_outcome: result.outcome,
        });
    }

    None
}

fn snapshot_has_active_job(snapshot: Option<&ManagementAgentSnapshot>, job_id: u64) -> bool {
    snapshot
        .and_then(|snapshot| snapshot.active.as_ref())
        .is_some_and(|active| active.job_id == job_id)
}

fn snapshot_is_terminal(snapshot: Option<&ManagementAgentSnapshot>, job_id: u64) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };

    if snapshot
        .active
        .as_ref()
        .is_some_and(|active| active.job_id == job_id)
    {
        return false;
    }

    snapshot
        .latest_result
        .as_ref()
        .is_some_and(|result| result.job_id == job_id)
}

fn parse_endpoint(value: &str, description: &str) -> Result<SocketAddr, String> {
    value
        .trim()
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid {description} address: {error}"))
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;

    const MIB: f64 = 1024.0 * KIB;

    const GIB: f64 = 1024.0 * MIB;

    const TIB: f64 = 1024.0 * GIB;

    let bytes = bytes as f64;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes / TIB,)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB,)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB,)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB,)
    } else {
        format!("{bytes:.0} B")
    }
}

fn configure_style(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);

    context.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);

        style.spacing.button_padding = egui::vec2(12.0, 7.0);

        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(22.0));

        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));

        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    });

    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = egui::Color32::from_rgb(10, 17, 27);

    visuals.window_fill = egui::Color32::from_rgb(16, 27, 42);

    visuals.extreme_bg_color = egui::Color32::from_rgb(5, 10, 17);

    visuals.selection.bg_fill = egui::Color32::from_rgb(0, 128, 194);

    context.set_visuals_of(egui::Theme::Dark, visuals);
}

const UPDATE_HANDOFF_WAIT_ARGUMENT: &str = release_update::UPDATE_HANDOFF_WAIT_ARGUMENT;

const UPDATE_STARTUP_CONFIRM_ARGUMENT: &str = release_update::UPDATE_STARTUP_CONFIRM_ARGUMENT;

fn parse_update_handoff_wait_argument(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut arguments = arguments.into_iter();

    let _program_name = arguments.next();

    let Some(first_argument) = arguments.next() else {
        return Ok(None);
    };

    if first_argument.as_os_str() != OsStr::new(UPDATE_HANDOFF_WAIT_ARGUMENT) {
        return Ok(None);
    }

    let handoff_path = arguments.next().ok_or_else(|| {
        format!("{UPDATE_HANDOFF_WAIT_ARGUMENT} requires an absolute handoff-plan path")
    })?;

    if arguments.next().is_some() {
        return Err(format!(
            "{UPDATE_HANDOFF_WAIT_ARGUMENT} accepts exactly one handoff-plan path",
        ));
    }

    Ok(Some(PathBuf::from(handoff_path)))
}

fn parse_update_startup_confirmation_argument(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut arguments = arguments.into_iter();

    let _program_name = arguments.next();

    let Some(first_argument) = arguments.next() else {
        return Ok(None);
    };

    if first_argument.as_os_str() != OsStr::new(UPDATE_STARTUP_CONFIRM_ARGUMENT) {
        return Ok(None);
    }

    let handoff_path = arguments.next().ok_or_else(|| {
        format!("{UPDATE_STARTUP_CONFIRM_ARGUMENT} requires an absolute handoff-plan path",)
    })?;

    if arguments.next().is_some() {
        return Err(format!(
            "{UPDATE_STARTUP_CONFIRM_ARGUMENT} accepts exactly one handoff-plan path",
        ));
    }

    Ok(Some(PathBuf::from(handoff_path)))
}

fn prepare_update_startup_confirmation_if_requested()
-> Result<Option<release_update::UpdateStartupConfirmation>, String> {
    let Some(handoff_path) = parse_update_startup_confirmation_argument(env::args_os())? else {
        return Ok(None);
    };

    release_update::prepare_update_startup_confirmation(&handoff_path, ReleaseArtifactKind::Manager)
        .map(Some)
        .map_err(|error| format!("Manager update startup confirmation failed: {error}",))
}

fn run_update_handoff_wait_if_requested() -> Result<bool, String> {
    let Some(handoff_path) = parse_update_handoff_wait_argument(env::args_os())? else {
        return Ok(false);
    };

    let report =
        release_update::run_update_handoff_wait_mode(&handoff_path, ReleaseArtifactKind::Manager)
            .map_err(|error| format!("Manager update handoff wait failed: {error}"))?;

    let startup = report.startup_confirmation.as_ref().ok_or_else(|| {
        "Manager update helper completed without startup confirmation".to_string()
    })?;

    match &report.publication {
        release_update::UpdateInstallationPublication::PublishedSideBySide {
            installed_executable,
            ..
        } => {
            eprintln!(
                "Manager update helper validated {}, observed parent process {} as {:?}, prepared \
                 backup {}, published the verified officially named executable {}, relaunched it \
                 as process {}, and received healthy-startup marker {}.",
                report.handoff.staged_executable.display(),
                report.handoff.parent_process_id,
                report.parent_wait,
                report.installation.backup_executable.display(),
                installed_executable.display(),
                startup.process_id,
                startup.startup_marker.display(),
            );
        }

        release_update::UpdateInstallationPublication::ReplacedInPlace {
            installed_executable,
        } => {
            eprintln!(
                "Manager update helper validated {}, observed parent process {} as {:?}, prepared \
                 backup {}, atomically replaced the custom-named executable {}, relaunched it as \
                 process {}, and received healthy-startup marker {}.",
                report.handoff.staged_executable.display(),
                report.handoff.parent_process_id,
                report.parent_wait,
                report.installation.backup_executable.display(),
                installed_executable.display(),
                startup.process_id,
                startup.startup_marker.display(),
            );
        }
    }

    Ok(true)
}

fn main() -> eframe::Result {
    match run_update_handoff_wait_if_requested() {
        Ok(true) => return Ok(()),

        Ok(false) => {}

        Err(error) => {
            eprintln!("{error}");

            std::process::exit(2);
        }
    }

    let startup_confirmation = match prepare_update_startup_confirmation_if_requested() {
        Ok(confirmation) => confirmation,

        Err(error) => {
            eprintln!("{error}");

            std::process::exit(2);
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 900.0])
            .with_min_inner_size([860.0, 640.0]),

        centered: true,

        renderer: eframe::Renderer::Wgpu,

        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation_context| {
            configure_style(&creation_context.egui_ctx);

            let application = NetworkCopyManager::new();

            if let Some(confirmation) = startup_confirmation.as_ref() {
                release_update::write_update_startup_marker(confirmation).map_err(
                    |error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) },
                )?;
            }

            Ok(Box::new(application))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DirectManagementRoute, MAX_TRANSFER_HISTORY, ManagedEndpointRole, ManagedStartFailureKind,
        PairedTransferHistoryEntry, UPDATE_STARTUP_CONFIRM_ARGUMENT, build_batch_queue_requests,
        join_remote_path, paired_outcome, parent_remote_path, parse_update_handoff_wait_argument,
        parse_update_startup_confirmation_argument, peer_cleanup_target, preflight_queue_route,
        queue_state_for_outcome, queue_state_for_start_failure, remember_history,
        resolve_management_endpoints, select_bound_recovery_item,
        select_or_clear_discovered_endpoint, transfer_matches_queue_request,
    };
    use networkcopy_speed::management_active_binding::ActiveQueueBinding;
    use networkcopy_speed::management_discovery::{AgentCapabilities, AgentState, DiscoveredAgent};
    use networkcopy_speed::management_instance::AgentInstanceId;
    use networkcopy_speed::management_orchestration::ManagedTransferRecord;
    use networkcopy_speed::management_queue::{
        QueuedTransferKind, QueuedTransferRequest, QueuedTransferState, TransferQueue,
    };
    use networkcopy_speed::management_route::ManagementRouteMode;
    use networkcopy_speed::management_snapshot::{
        ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
        ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
    };
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn update_handoff_wait_argument_is_optional() {
        let parsed =
            parse_update_handoff_wait_argument([OsString::from("networkcopy-manager.exe")])
                .unwrap();

        assert_eq!(parsed, None);
    }

    #[test]
    fn update_handoff_wait_argument_accepts_one_path() {
        let handoff_path =
            OsString::from(r"C:\Users\User\AppData\Local\NetworkCopy\handoff-plan.bin");

        let parsed = parse_update_handoff_wait_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from("--update-handoff-wait"),
            handoff_path.clone(),
        ])
        .unwrap();

        assert_eq!(parsed, Some(PathBuf::from(handoff_path)));
    }

    #[test]
    fn update_handoff_wait_argument_requires_path() {
        let error = parse_update_handoff_wait_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from("--update-handoff-wait"),
        ])
        .unwrap_err();

        assert!(error.contains("requires"));
    }

    #[test]
    fn update_handoff_wait_argument_rejects_extra_values() {
        let error = parse_update_handoff_wait_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from("--update-handoff-wait"),
            OsString::from(r"C:\handoff-plan.bin"),
            OsString::from("unexpected"),
        ])
        .unwrap_err();

        assert!(error.contains("exactly one"));
    }

    #[test]
    fn update_handoff_wait_argument_ignores_unrelated_arguments() {
        let parsed = parse_update_handoff_wait_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from("--ordinary-manager-argument"),
        ])
        .unwrap();

        assert_eq!(parsed, None);
    }

    #[test]
    fn update_startup_confirmation_argument_accepts_exact_path() {
        let handoff = PathBuf::from(r"C:\Updates\handoff-plan.bin");

        let parsed = parse_update_startup_confirmation_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from(UPDATE_STARTUP_CONFIRM_ARGUMENT),
            handoff.clone().into_os_string(),
        ])
        .unwrap();

        assert_eq!(parsed, Some(handoff));
    }

    #[test]
    fn update_startup_confirmation_argument_rejects_missing_path() {
        let error = parse_update_startup_confirmation_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from(UPDATE_STARTUP_CONFIRM_ARGUMENT),
        ])
        .unwrap_err();

        assert!(error.contains("requires"));
    }

    #[test]
    fn update_startup_confirmation_argument_rejects_trailing_arguments() {
        let error = parse_update_startup_confirmation_argument([
            OsString::from("networkcopy-manager.exe"),
            OsString::from(UPDATE_STARTUP_CONFIRM_ARGUMENT),
            OsString::from(r"C:\Updates\handoff-plan.bin"),
            OsString::from("unexpected"),
        ])
        .unwrap_err();

        assert!(error.contains("exactly one"));
    }

    #[test]
    fn remote_path_navigation_works() {
        assert_eq!(join_remote_path(r"C:\Users", "Public",), r"C:\Users\Public",);

        assert_eq!(
            parent_remote_path(r"C:\Users\Public",),
            Some(r"C:\Users".to_string(),),
        );
    }

    #[test]
    fn drive_root_has_no_browser_parent() {
        assert_eq!(parent_remote_path(r"C:\"), None,);
    }

    #[test]
    fn discovered_endpoint_selection_resolves_role_conflict() {
        let endpoint = "192.168.1.2:7339";

        let mut sender = endpoint.to_string();

        let mut receiver = endpoint.to_string();

        select_or_clear_discovered_endpoint(&mut sender, &mut receiver, endpoint);

        assert_eq!(sender, endpoint);

        assert!(receiver.is_empty());

        select_or_clear_discovered_endpoint(&mut sender, &mut receiver, endpoint);

        assert!(sender.is_empty());

        select_or_clear_discovered_endpoint(&mut receiver, &mut sender, endpoint);

        assert_eq!(receiver, endpoint);

        assert!(sender.is_empty());
    }

    fn history_entry(sequence: u64, outcome: ManagementJobOutcome) -> PairedTransferHistoryEntry {
        let sender_agent = "127.0.0.1:7339".parse().unwrap();

        let receiver_agent = "127.0.0.1:7340".parse().unwrap();

        let sender_job_id = sequence.saturating_mul(2).saturating_add(1);

        let receiver_job_id = sequence.saturating_mul(2).saturating_add(2);

        PairedTransferHistoryEntry {
            transfer: ManagedTransferRecord {
                sender_agent,

                sender_job_id,

                receiver_agent,

                receiver_job_id,

                receiver_payload: "127.0.0.1:7337".parse().unwrap(),

                source_root: format!(r"C:\Source-{sequence}"),

                destination_root: format!(r"D:\Destination-{sequence}"),

                update_existing: false,

                worker_count: 4,

                calibration_mib: 8,
            },

            sender_result: ManagementJobResult {
                role: ManagementJobRole::Sender,

                outcome,

                job_id: sender_job_id,

                files: 10,

                logical_bytes: 1_000_000,

                wire_bytes: 750_000,

                data_stream_count: 4,

                message: String::new(),
            },

            receiver_result: ManagementJobResult {
                role: ManagementJobRole::Receiver,

                outcome,

                job_id: receiver_job_id,

                files: 10,

                logical_bytes: 1_000_000,

                wire_bytes: 0,

                data_stream_count: 4,

                message: String::new(),
            },
        }
    }

    fn active_snapshot(role: ManagementJobRole, job_id: u64) -> ManagementAgentSnapshot {
        let details = match role {
            ManagementJobRole::Sender => ManagementActiveJobDetails::Sender {
                receiver_address: "127.0.0.1:7337".parse().unwrap(),

                source_root: r"C:\Source".to_string(),

                worker_count: 4,

                calibration_mib: 8,
            },

            ManagementJobRole::Receiver => ManagementActiveJobDetails::Receiver {
                transfer_port: 7337,

                destination_root: r"D:\Destination".to_string(),

                update_existing: false,
            },
        };

        ManagementAgentSnapshot {
            agent_instance_id: AgentInstanceId::from_raw(u128::from(job_id)).unwrap(),

            active: Some(ManagementActiveJobSnapshot {
                role,

                job_id,

                phase: "Transfer".to_string(),

                completed: 10,

                total: 100,

                cancel_requested: false,

                details,
            }),

            latest_result: None,
        }
    }

    fn terminal_snapshot(result: ManagementJobResult) -> ManagementAgentSnapshot {
        ManagementAgentSnapshot {
            agent_instance_id: AgentInstanceId::from_raw(u128::from(result.job_id)).unwrap(),

            active: None,

            latest_result: Some(result),
        }
    }

    #[test]
    fn paired_outcome_uses_worst_endpoint_result() {
        let completed = history_entry(1, ManagementJobOutcome::Completed);

        assert_eq!(
            paired_outcome(&completed.sender_result, &completed.receiver_result,),
            ManagementJobOutcome::Completed,
        );

        let mut cancelled = completed.clone();

        cancelled.receiver_result.outcome = ManagementJobOutcome::Cancelled;

        assert_eq!(
            paired_outcome(&cancelled.sender_result, &cancelled.receiver_result,),
            ManagementJobOutcome::Cancelled,
        );

        cancelled.sender_result.outcome = ManagementJobOutcome::Failed;

        assert_eq!(
            paired_outcome(&cancelled.sender_result, &cancelled.receiver_result,),
            ManagementJobOutcome::Failed,
        );
    }

    #[test]
    fn history_is_deduplicated_and_bounded() {
        let mut history = VecDeque::new();

        let first = history_entry(1, ManagementJobOutcome::Completed);

        remember_history(&mut history, first.clone());

        remember_history(&mut history, first);

        assert_eq!(history.len(), 1);

        for sequence in 2..=(MAX_TRANSFER_HISTORY as u64 + 5) {
            remember_history(
                &mut history,
                history_entry(sequence, ManagementJobOutcome::Completed),
            );
        }

        assert_eq!(history.len(), MAX_TRANSFER_HISTORY,);

        assert_eq!(
            history.front().unwrap().transfer.sender_job_id,
            (MAX_TRANSFER_HISTORY as u64 + 5)
                .saturating_mul(2)
                .saturating_add(1),
        );
    }

    #[test]
    fn interrupted_history_exposes_receiver_resume_stream_count() {
        let cancelled = history_entry(1, ManagementJobOutcome::Cancelled);

        assert_eq!(cancelled.resume_data_stream_count(), Some(4),);

        let completed = history_entry(2, ManagementJobOutcome::Completed);

        assert_eq!(completed.resume_data_stream_count(), None,);

        let mut missing_journal = history_entry(3, ManagementJobOutcome::Failed);

        missing_journal.receiver_result.data_stream_count = 0;

        assert_eq!(missing_journal.resume_data_stream_count(), None,);
    }

    #[test]
    fn failed_sender_cleans_up_active_receiver() {
        let entry = history_entry(1, ManagementJobOutcome::Failed);

        let transfer = entry.transfer.clone();

        let sender_snapshot = terminal_snapshot(entry.sender_result);

        let receiver_snapshot =
            active_snapshot(ManagementJobRole::Receiver, transfer.receiver_job_id);

        let target =
            peer_cleanup_target(&transfer, Some(&sender_snapshot), Some(&receiver_snapshot))
                .unwrap();

        assert_eq!(target.endpoint_role, ManagedEndpointRole::Receiver,);

        assert_eq!(target.job_id, transfer.receiver_job_id,);

        assert_eq!(target.trigger_outcome, ManagementJobOutcome::Failed,);
    }

    #[test]
    fn queue_state_matches_paired_terminal_outcome() {
        assert_eq!(
            queue_state_for_outcome(ManagementJobOutcome::Completed),
            QueuedTransferState::Completed,
        );

        assert_eq!(
            queue_state_for_outcome(ManagementJobOutcome::Cancelled),
            QueuedTransferState::Cancelled,
        );

        assert_eq!(
            queue_state_for_outcome(ManagementJobOutcome::Failed),
            QueuedTransferState::Failed,
        );
    }

    #[test]
    fn queue_start_failures_distinguish_blocked_from_failed() {
        assert_eq!(
            queue_state_for_start_failure(ManagedStartFailureKind::Blocked,),
            QueuedTransferState::Blocked,
        );

        assert_eq!(
            queue_state_for_start_failure(ManagedStartFailureKind::Failed,),
            QueuedTransferState::Failed,
        );
    }

    #[test]
    fn explicit_ip_does_not_require_discovery() {
        let expected = (
            "192.0.2.10:7339".parse().unwrap(),
            "192.0.2.11:7339".parse().unwrap(),
        );

        assert_eq!(
            resolve_management_endpoints(
                ManagementRouteMode::ExplicitIp,
                "192.0.2.10:7339",
                "192.0.2.11:7339",
                &[],
                &[],
            )
            .unwrap(),
            expected,
        );
    }

    #[test]
    fn automatic_lan_requires_discovered_capable_agents() {
        let sender_endpoint = "192.0.2.10:7339".parse().unwrap();

        let receiver_endpoint = "192.0.2.11:7339".parse().unwrap();

        let agents = vec![
            DiscoveredAgent {
                hostname: "SENDER-PC".to_string(),

                endpoint: sender_endpoint,

                protocol_version: 1,

                state: AgentState::Idle,

                capabilities: AgentCapabilities::SEND_RECEIVE,
            },
            DiscoveredAgent {
                hostname: "RECEIVER-PC".to_string(),

                endpoint: receiver_endpoint,

                protocol_version: 1,

                state: AgentState::Idle,

                capabilities: AgentCapabilities::SEND_RECEIVE,
            },
        ];

        assert_eq!(
            resolve_management_endpoints(
                ManagementRouteMode::AutomaticLan,
                &sender_endpoint.to_string(),
                &receiver_endpoint.to_string(),
                &agents,
                &[],
            )
            .unwrap(),
            (sender_endpoint, receiver_endpoint,),
        );

        assert!(
            resolve_management_endpoints(
                ManagementRouteMode::AutomaticLan,
                &sender_endpoint.to_string(),
                &receiver_endpoint.to_string(),
                &[],
                &[],
            )
            .is_err(),
        );
    }

    #[test]
    fn direct_link_requires_matching_local_sender_and_peer_receiver() {
        let local_endpoint = "[fe80::10%42]:7339".parse().unwrap();

        let peer_endpoint = "[fe80::20%42]:7339".parse().unwrap();

        let route = DirectManagementRoute {
            interface_index: 42,

            local_agent: DiscoveredAgent {
                hostname: "SOURCE-PC".to_string(),

                endpoint: local_endpoint,

                protocol_version: 1,

                state: AgentState::Idle,

                capabilities: AgentCapabilities::SEND_RECEIVE,
            },

            peer_agent: DiscoveredAgent {
                hostname: "DESTINATION-PC".to_string(),

                endpoint: peer_endpoint,

                protocol_version: 1,

                state: AgentState::Idle,

                capabilities: AgentCapabilities::SEND_RECEIVE,
            },
        };

        assert_eq!(
            resolve_management_endpoints(
                ManagementRouteMode::DirectLink,
                &local_endpoint.to_string(),
                &peer_endpoint.to_string(),
                &[],
                std::slice::from_ref(&route),
            )
            .unwrap(),
            (local_endpoint, peer_endpoint,),
        );

        assert!(
            resolve_management_endpoints(
                ManagementRouteMode::DirectLink,
                &peer_endpoint.to_string(),
                &local_endpoint.to_string(),
                &[],
                &[route],
            )
            .is_err(),
        );

        assert!(preflight_queue_route(ManagementRouteMode::DirectLink,).is_ok(),);
    }

    #[test]
    fn batch_queue_builder_maps_sources_beneath_one_root() {
        let requests = build_batch_queue_requests(
            "127.0.0.1:7339".parse().unwrap(),
            "127.0.0.1:7340".parse().unwrap(),
            ManagementRouteMode::ExplicitIp,
            &[
                r"C:\Users\User\Desktop".to_string(),
                r"C:\Users\User\Documents".to_string(),
                r"C:\Users\User\Downloads".to_string(),
            ],
            r"D:\Backup\User",
            true,
            4,
            8,
        )
        .unwrap();

        assert_eq!(requests.len(), 3);

        assert_eq!(requests[0].destination_root, r"D:\Backup\User\Desktop",);

        assert_eq!(requests[1].destination_root, r"D:\Backup\User\Documents",);

        assert_eq!(requests[2].destination_root, r"D:\Backup\User\Downloads",);

        assert!(requests.iter().all(|request| request.update_existing),);
    }

    #[test]
    fn batch_queue_builder_rejects_duplicate_sources() {
        let error = build_batch_queue_requests(
            "127.0.0.1:7339".parse().unwrap(),
            "127.0.0.1:7340".parse().unwrap(),
            ManagementRouteMode::ExplicitIp,
            &[
                r"C:\Users\User\Desktop".to_string(),
                r"c:/users/user/desktop/".to_string(),
            ],
            r"D:\Backup\User",
            false,
            4,
            8,
        )
        .unwrap_err();

        assert!(error.contains("same source folder"));
    }

    #[test]
    fn batch_queue_builder_rejects_destination_collisions() {
        let error = build_batch_queue_requests(
            "127.0.0.1:7339".parse().unwrap(),
            "127.0.0.1:7340".parse().unwrap(),
            ManagementRouteMode::ExplicitIp,
            &[
                r"C:\Users\Alice\Desktop".to_string(),
                r"C:\Users\Bob\Desktop".to_string(),
            ],
            r"D:\Backup",
            false,
            4,
            8,
        )
        .unwrap_err();

        assert!(error.contains("same destination folder"));
    }

    #[test]
    fn persisted_running_item_without_binding_is_blocked() {
        let mut queue = TransferQueue::default();

        let id = queue
            .add(QueuedTransferRequest {
                sender_agent: "127.0.0.1:7339".parse().unwrap(),

                receiver_agent: "127.0.0.1:7340".parse().unwrap(),

                route_mode: ManagementRouteMode::AutomaticLan,

                source_root: r"C:\Source".to_string(),

                destination_root: r"D:\Destination".to_string(),

                update_existing: true,

                worker_count: 4,

                calibration_mib: 8,

                kind: QueuedTransferKind::Fresh,
            })
            .unwrap();

        queue
            .set_state(id, QueuedTransferState::Running, "Transfer active")
            .unwrap();

        assert!(select_bound_recovery_item(&mut queue,).unwrap().is_none(),);

        let item = queue.items().iter().find(|item| item.id == id).unwrap();

        assert_eq!(item.state, QueuedTransferState::Blocked,);

        assert_eq!(queue.active_binding(), None,);
    }

    #[test]
    fn persisted_bound_item_is_selected_for_exact_recovery() {
        let mut queue = TransferQueue::default();

        let id = queue
            .add(QueuedTransferRequest {
                sender_agent: "127.0.0.1:7339".parse().unwrap(),

                receiver_agent: "127.0.0.1:7340".parse().unwrap(),

                route_mode: ManagementRouteMode::AutomaticLan,

                source_root: r"C:\Source".to_string(),

                destination_root: r"D:\Destination".to_string(),

                update_existing: true,

                worker_count: 4,

                calibration_mib: 8,

                kind: QueuedTransferKind::Fresh,
            })
            .unwrap();

        queue
            .set_state(id, QueuedTransferState::Blocked, "Manager restarted")
            .unwrap();

        let binding = ActiveQueueBinding::new(
            id,
            AgentInstanceId::from_raw(11).unwrap(),
            11,
            AgentInstanceId::from_raw(17).unwrap(),
            17,
        )
        .unwrap();

        queue.set_active_binding(binding).unwrap();

        let selected = select_bound_recovery_item(&mut queue).unwrap().unwrap();

        assert_eq!(selected.id, id);

        assert_eq!(queue.active_binding(), Some(binding),);
    }

    #[test]
    fn restart_reattachment_requires_exact_queue_match() {
        let request = QueuedTransferRequest {
            sender_agent: "127.0.0.1:7339".parse().unwrap(),

            receiver_agent: "127.0.0.1:7340".parse().unwrap(),

            route_mode: ManagementRouteMode::AutomaticLan,

            source_root: r"C:\Source".to_string(),

            destination_root: r"D:\Destination".to_string(),

            update_existing: true,

            worker_count: 4,

            calibration_mib: 8,

            kind: QueuedTransferKind::Fresh,
        };

        let transfer = ManagedTransferRecord {
            sender_agent: request.sender_agent,

            sender_job_id: 11,

            receiver_agent: request.receiver_agent,

            receiver_job_id: 17,

            receiver_payload: "127.0.0.1:7337".parse().unwrap(),

            source_root: request.source_root.clone(),

            destination_root: request.destination_root.clone(),

            update_existing: request.update_existing,

            worker_count: request.worker_count,

            calibration_mib: request.calibration_mib,
        };

        assert!(transfer_matches_queue_request(&request, &transfer,));

        let mut mismatch = transfer.clone();

        mismatch.source_root = r"C:\Different".to_string();

        assert!(!transfer_matches_queue_request(&request, &mismatch,));

        let mut mismatch = transfer.clone();

        mismatch.destination_root = r"D:\Different".to_string();

        assert!(!transfer_matches_queue_request(&request, &mismatch,));

        let mut mismatch = transfer.clone();

        mismatch.update_existing = false;

        assert!(!transfer_matches_queue_request(&request, &mismatch,));

        let mut mismatch = transfer.clone();

        mismatch.worker_count = 8;

        assert!(!transfer_matches_queue_request(&request, &mismatch,));

        let mut mismatch = transfer.clone();

        mismatch.calibration_mib = 64;

        assert!(!transfer_matches_queue_request(&request, &mismatch,));

        let mut mismatch = transfer.clone();

        mismatch.receiver_agent = "127.0.0.1:7350".parse().unwrap();

        assert!(!transfer_matches_queue_request(&request, &mismatch,));
    }

    #[test]
    fn cancelled_receiver_cleans_up_active_sender() {
        let entry = history_entry(2, ManagementJobOutcome::Cancelled);

        let transfer = entry.transfer.clone();

        let sender_snapshot = active_snapshot(ManagementJobRole::Sender, transfer.sender_job_id);

        let receiver_snapshot = terminal_snapshot(entry.receiver_result);

        let target =
            peer_cleanup_target(&transfer, Some(&sender_snapshot), Some(&receiver_snapshot))
                .unwrap();

        assert_eq!(target.endpoint_role, ManagedEndpointRole::Sender,);

        assert_eq!(target.job_id, transfer.sender_job_id,);

        assert_eq!(target.trigger_outcome, ManagementJobOutcome::Cancelled,);
    }

    #[test]
    fn completed_endpoint_does_not_cancel_finalizing_peer() {
        let entry = history_entry(3, ManagementJobOutcome::Completed);

        let transfer = entry.transfer.clone();

        let sender_snapshot = terminal_snapshot(entry.sender_result);

        let receiver_snapshot =
            active_snapshot(ManagementJobRole::Receiver, transfer.receiver_job_id);

        assert_eq!(
            peer_cleanup_target(&transfer, Some(&sender_snapshot), Some(&receiver_snapshot),),
            None,
        );
    }
}
