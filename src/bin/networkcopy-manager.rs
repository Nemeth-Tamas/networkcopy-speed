#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use networkcopy_speed::management_control;
use networkcopy_speed::management_directory::{ManagementDirectoryEntry, ManagementEntryKind};
use networkcopy_speed::management_discovery::{self, DiscoveredAgent};
use networkcopy_speed::management_orchestration::{
    self, ManagedTransferRecord, ManagedTransferRequest,
};
use networkcopy_speed::management_persistence::{self, ManagerHistoryEntry, ManagerPersistedState};
use networkcopy_speed::management_reconnect;
use networkcopy_speed::management_snapshot::{
    ManagementAgentSnapshot, ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
};
use std::collections::VecDeque;
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

type StartResult = Result<ManagedTransferRecord, String>;

type AttachResult = Result<
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

    sender_agent: String,

    receiver_agent: String,

    source_root: String,

    destination_root: String,

    sender_browser: RemoteBrowserPane,

    receiver_browser: RemoteBrowserPane,

    worker_count: usize,

    calibration_mib: u64,

    update_existing: bool,

    show_agents: bool,

    show_setup: bool,

    show_browsers: bool,

    show_history: bool,

    discovery_receiver: Option<Receiver<DiscoveryResult>>,

    start_receiver: Option<Receiver<StartResult>>,

    attach_receiver: Option<Receiver<AttachResult>>,

    poll_receiver: Option<Receiver<PollResponse>>,

    cancel_receiver: Option<Receiver<CancelResponse>>,

    peer_cleanup_receiver: Option<Receiver<PeerCleanupResponse>>,

    peer_cleanup_attempted: bool,

    transfer: Option<ManagedTransferRecord>,

    sender_snapshot: Option<ManagementAgentSnapshot>,

    receiver_snapshot: Option<ManagementAgentSnapshot>,

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

            sender_agent: String::new(),

            receiver_agent: String::new(),

            source_root: String::new(),

            destination_root: String::new(),

            sender_browser: RemoteBrowserPane::new(),

            receiver_browser: RemoteBrowserPane::new(),

            worker_count: 4,

            calibration_mib: 8,

            update_existing: false,

            show_agents: true,

            show_setup: true,

            show_browsers: false,

            show_history: false,

            discovery_receiver: None,

            start_receiver: None,

            attach_receiver: None,

            poll_receiver: None,

            cancel_receiver: None,

            peer_cleanup_receiver: None,

            peer_cleanup_attempted: false,

            transfer: None,

            sender_snapshot: None,

            receiver_snapshot: None,

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
                self.apply_persisted_state(state.clone());

                self.last_saved_state = Some(state);

                self.notice =
                    "Restored saved manager configuration and transfer history.".to_string();
            }

            Ok(None) => {}

            Err(error) => {
                self.persistence_error = format!("Failed to load {}: {error}", path.display(),);
            }
        }
    }

    fn apply_persisted_state(&mut self, state: ManagerPersistedState) {
        self.sender_agent = state.sender_agent;

        self.receiver_agent = state.receiver_agent;

        self.source_root = state.source_root;

        self.destination_root = state.destination_root;

        self.worker_count = state.worker_count;

        self.calibration_mib = state.calibration_mib;

        self.update_existing = state.update_existing;

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

    fn begin_transfer(&mut self) {
        if self.start_receiver.is_some() {
            return;
        }

        self.error.clear();

        let sender_agent = match parse_endpoint(&self.sender_agent, "sender management agent") {
            Ok(endpoint) => endpoint,

            Err(error) => {
                self.error = error;
                return;
            }
        };

        let receiver_agent = match parse_endpoint(&self.receiver_agent, "receiver management agent")
        {
            Ok(endpoint) => endpoint,

            Err(error) => {
                self.error = error;
                return;
            }
        };

        if sender_agent == receiver_agent {
            self.error = "Sender and receiver must be different management agents.".to_string();

            return;
        }

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
            let result = management_orchestration::start_transfer(request)
                .map_err(|error| format!("Managed transfer startup failed: {error}"));

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
                .map_err(|error| format!("Managed transfer resume failed: {error}"));

            let _ = sender.send(result);
        });
    }

    fn begin_attach(&mut self) {
        if self.attach_receiver.is_some() {
            return;
        }

        self.error.clear();

        let sender_agent = match parse_endpoint(&self.sender_agent, "sender management agent") {
            Ok(endpoint) => endpoint,

            Err(error) => {
                self.error = error;
                return;
            }
        };

        let receiver_agent = match parse_endpoint(&self.receiver_agent, "receiver management agent")
        {
            Ok(endpoint) => endpoint,

            Err(error) => {
                self.error = error;
                return;
            }
        };

        if sender_agent == receiver_agent {
            self.error = "Sender and receiver must be different management agents.".to_string();

            return;
        }

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

        self.process_start_message();

        self.process_attach_message();

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

                self.notice = "Both endpoint jobs were accepted. The manager is now polling them."
                    .to_string();

                self.transfer = Some(transfer);

                self.peer_cleanup_receiver = None;

                self.peer_cleanup_attempted = false;

                self.last_poll = Instant::now();

                self.monitoring_complete = false;

                self.show_setup = false;
            }

            Some(Ok(Err(error))) => {
                self.start_receiver = None;

                self.error = error;

                self.notice.clear();
            }

            Some(Err(TryRecvError::Disconnected)) => {
                self.start_receiver = None;

                self.error = "Transfer startup worker disconnected.".to_string();

                self.notice.clear();
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

                    self.notice = "Both endpoint jobs reached a terminal state.".to_string();
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
            || self.start_receiver.is_some()
            || self.attach_receiver.is_some()
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
        } else if self.transfer.is_some() && !self.monitoring_complete {
            ("Transfer active", egui::Color32::from_rgb(126, 230, 64))
        } else if self.monitoring_complete {
            ("Transfer finished", egui::Color32::from_rgb(95, 194, 255))
        } else if self.discovery_receiver.is_some() {
            ("Discovering agents", egui::Color32::from_rgb(95, 194, 255))
        } else {
            ("Ready", egui::Color32::from_rgb(126, 230, 64))
        }
    }

    fn render_app_header(&self, ui: &mut egui::Ui) {
        let (status, status_color) = self.manager_status();

        ui.horizontal_wrapped(|ui| {
            ui.heading(APP_NAME);

            ui.separator();

            status_label(ui, status, status_color);

            ui.separator();

            ui.label(format!("v{}", env!("CARGO_PKG_VERSION"),));
        });

        ui.label("Remote LAN orchestration with direct sender-to-receiver payload transfer.");

        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} agent(s) discovered", self.agents.len(),));

            ui.separator();

            ui.label(format!("{} retained transfer(s)", self.history.len(),));

            ui.separator();

            ui.label("Trusted LAN · management traffic is not yet encrypted");
        });
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
        ui.horizontal_wrapped(|ui| {
            let discovering = self.discovery_receiver.is_some();

            if ui
                .add_enabled(!discovering, egui::Button::new("Refresh discovery"))
                .clicked()
            {
                self.begin_discovery();
            }

            if discovering {
                ui.spinner();

                ui.label("Searching LAN...");
            }

            ui.label("Run networkcopy-speed.exe management-agent on each endpoint.");
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
                    if ui
                        .add_enabled(
                            sender_enabled && !sender_selected,
                            egui::Button::new(if sender_selected {
                                "Sender selected"
                            } else {
                                "Use as sender"
                            }),
                        )
                        .clicked()
                    {
                        self.sender_agent = endpoint_text.clone();
                    }

                    if ui
                        .add_enabled(
                            receiver_enabled && !receiver_selected,
                            egui::Button::new(if receiver_selected {
                                "Receiver selected"
                            } else {
                                "Use as receiver"
                            }),
                        )
                        .clicked()
                    {
                        self.receiver_agent = endpoint_text.clone();
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

    fn render_configuration(&mut self, ui: &mut egui::Ui) {
        ui.label("Paths are evaluated on the selected remote endpoint machines.");

        ui.add_space(8.0);

        ui.columns(2, |columns| {
            let (sender_column, receiver_column) = columns.split_at_mut(1);

            sender_column[0].group(|ui| {
                ui.set_min_width(ui.available_width());

                status_label(ui, "Sender endpoint", egui::Color32::from_rgb(95, 194, 255));

                ui.add_space(4.0);

                ui.label("Management agent");

                let width = ui.available_width();

                ui.add(egui::TextEdit::singleline(&mut self.sender_agent).desired_width(width));

                ui.label("Source folder");

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

                ui.add(egui::TextEdit::singleline(&mut self.receiver_agent).desired_width(width));

                ui.label("Destination folder");

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

                render_remote_browser(
                    &mut left[0],
                    "Sender folders",
                    &sender_agent,
                    &mut self.source_root,
                    &mut self.sender_browser,
                );

                render_remote_browser(
                    &mut right[0],
                    "Receiver folders",
                    &receiver_agent,
                    &mut self.destination_root,
                    &mut self.receiver_browser,
                );
            });
        }

        if !self.sender_agent.is_empty() && self.sender_agent == self.receiver_agent {
            ui.label("Sender and receiver cannot be the same agent.");
        }

        ui.add_space(8.0);

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let can_start = self.start_receiver.is_none()
            && self.attach_receiver.is_none()
            && !transfer_active
            && !self.sender_agent.trim().is_empty()
            && !self.receiver_agent.trim().is_empty()
            && !self.source_root.trim().is_empty()
            && !self.destination_root.trim().is_empty()
            && self.sender_agent != self.receiver_agent;

        let can_attach = self.attach_receiver.is_none()
            && self.start_receiver.is_none()
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
                self.transfer = None;

                self.peer_cleanup_receiver = None;

                self.peer_cleanup_attempted = false;

                self.sender_snapshot = None;

                self.receiver_snapshot = None;

                self.monitoring_complete = false;
            }
        });
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

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let can_resume = self.start_receiver.is_none()
            && self.attach_receiver.is_none()
            && self.cancel_receiver.is_none()
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
                } else if outcome != ManagementJobOutcome::Completed {
                    ui.label("Receiver resume journal unavailable.");
                }
            });

            ui.add_space(8.0);
        }

        if let Some(entry) = resume {
            self.begin_resumed_transfer(entry);
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
                        "Endpoints and paths selected".to_string()
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

fn main() -> eframe::Result {
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
        Box::new(|creation_context| {
            configure_style(&creation_context.egui_ctx);

            Ok(Box::new(NetworkCopyManager::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_TRANSFER_HISTORY, ManagedEndpointRole, PairedTransferHistoryEntry, join_remote_path,
        paired_outcome, parent_remote_path, peer_cleanup_target, remember_history,
    };
    use networkcopy_speed::management_orchestration::ManagedTransferRecord;
    use networkcopy_speed::management_snapshot::{
        ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
        ManagementJobOutcome, ManagementJobResult, ManagementJobRole,
    };
    use std::collections::VecDeque;

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
