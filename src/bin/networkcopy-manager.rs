#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use networkcopy_speed::management_control;
use networkcopy_speed::management_discovery::{self, DiscoveredAgent};
use networkcopy_speed::management_orchestration::{
    self, ManagedTransferRecord, ManagedTransferRequest,
};
use networkcopy_speed::management_snapshot::ManagementAgentSnapshot;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "NetworkCopy Manager";

const POLL_INTERVAL: Duration = Duration::from_millis(500);

const REPAINT_INTERVAL: Duration = Duration::from_millis(100);

type DiscoveryResult = Result<Vec<DiscoveredAgent>, String>;

type StartResult = Result<ManagedTransferRecord, String>;

struct PollResponse {
    sender: Result<ManagementAgentSnapshot, String>,

    receiver: Result<ManagementAgentSnapshot, String>,
}

struct CancelResponse {
    sender: Result<u64, String>,

    receiver: Result<u64, String>,
}

struct NetworkCopyManager {
    agents: Vec<DiscoveredAgent>,

    sender_agent: String,

    receiver_agent: String,

    source_root: String,

    destination_root: String,

    worker_count: usize,

    calibration_mib: u64,

    update_existing: bool,

    discovery_receiver: Option<Receiver<DiscoveryResult>>,

    start_receiver: Option<Receiver<StartResult>>,

    poll_receiver: Option<Receiver<PollResponse>>,

    cancel_receiver: Option<Receiver<CancelResponse>>,

    transfer: Option<ManagedTransferRecord>,

    sender_snapshot: Option<ManagementAgentSnapshot>,

    receiver_snapshot: Option<ManagementAgentSnapshot>,

    monitoring_complete: bool,

    last_poll: Instant,

    notice: String,

    error: String,
}

impl NetworkCopyManager {
    fn new() -> Self {
        let mut manager = Self {
            agents: Vec::new(),

            sender_agent: String::new(),

            receiver_agent: String::new(),

            source_root: String::new(),

            destination_root: String::new(),

            worker_count: 4,

            calibration_mib: 8,

            update_existing: false,

            discovery_receiver: None,

            start_receiver: None,

            poll_receiver: None,

            cancel_receiver: None,

            transfer: None,

            sender_snapshot: None,

            receiver_snapshot: None,

            monitoring_complete: false,

            last_poll: Instant::now(),

            notice: String::new(),

            error: String::new(),
        };

        manager.begin_discovery();

        manager
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

    fn begin_cancel(&mut self) {
        if self.cancel_receiver.is_some() {
            return;
        }

        let Some(transfer) = self.transfer.clone() else {
            return;
        };

        self.error.clear();

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

        self.process_poll_message();

        self.process_cancel_message();
    }

    fn process_discovery_message(&mut self) {
        let message = match &self.discovery_receiver {
            Some(receiver) => Some(receiver.try_recv()),

            None => None,
        };

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
        let message = match &self.start_receiver {
            Some(receiver) => Some(receiver.try_recv()),

            None => None,
        };

        match message {
            Some(Ok(Ok(transfer))) => {
                self.start_receiver = None;

                self.sender_agent = transfer.sender_agent.to_string();

                self.receiver_agent = transfer.receiver_agent.to_string();

                self.notice = "Both endpoint jobs were accepted. The manager is now polling them."
                    .to_string();

                self.transfer = Some(transfer);

                self.last_poll = Instant::now();

                self.monitoring_complete = false;
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

    fn process_poll_message(&mut self) {
        let message = match &self.poll_receiver {
            Some(receiver) => Some(receiver.try_recv()),

            None => None,
        };

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

                if let Some(transfer) = &self.transfer {
                    let sender_complete =
                        snapshot_is_terminal(self.sender_snapshot.as_ref(), transfer.sender_job_id);

                    let receiver_complete = snapshot_is_terminal(
                        self.receiver_snapshot.as_ref(),
                        transfer.receiver_job_id,
                    );

                    self.monitoring_complete = sender_complete && receiver_complete;

                    if self.monitoring_complete {
                        self.notice = "Both endpoint jobs reached a terminal state.".to_string();
                    }
                }
            }

            Err(TryRecvError::Disconnected) => {
                self.poll_receiver = None;

                self.error = "Snapshot polling worker disconnected.".to_string();
            }

            Err(TryRecvError::Empty) => {}
        }
    }

    fn process_cancel_message(&mut self) {
        let message = match &self.cancel_receiver {
            Some(receiver) => Some(receiver.try_recv()),

            None => None,
        };

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

    fn has_background_work(&self) -> bool {
        self.discovery_receiver.is_some()
            || self.start_receiver.is_some()
            || self.poll_receiver.is_some()
            || self.cancel_receiver.is_some()
            || (self.transfer.is_some() && !self.monitoring_complete)
    }

    fn render_discovery(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("LAN agents");

            let discovering = self.discovery_receiver.is_some();

            if ui
                .add_enabled(!discovering, egui::Button::new("Refresh discovery"))
                .clicked()
            {
                self.begin_discovery();
            }

            if discovering {
                ui.spinner();

                ui.label("Searching...");
            }
        });

        ui.label("Run networkcopy-speed.exe management-agent on each endpoint machine.");

        ui.add_space(6.0);

        if self.agents.is_empty() {
            ui.label("No agents discovered yet. Manual IP addresses can still be entered below.");

            return;
        }

        let agents = self.agents.clone();

        for agent in agents {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(&agent.hostname);

                    ui.label(agent.endpoint.to_string());

                    ui.label(agent.state.label());
                });

                ui.horizontal(|ui| {
                    let sender_enabled = agent.capabilities.can_send();

                    let receiver_enabled = agent.capabilities.can_receive();

                    if ui
                        .add_enabled(sender_enabled, egui::Button::new("Use as sender"))
                        .clicked()
                    {
                        self.sender_agent = agent.endpoint.to_string();
                    }

                    if ui
                        .add_enabled(receiver_enabled, egui::Button::new("Use as receiver"))
                        .clicked()
                    {
                        self.receiver_agent = agent.endpoint.to_string();
                    }

                    ui.label(format!("Protocol {}", agent.protocol_version,));
                });
            });
        }
    }

    fn render_configuration(&mut self, ui: &mut egui::Ui) {
        ui.heading("Transfer setup");

        ui.label("The following paths are evaluated on the selected remote machines.");

        ui.add_space(6.0);

        egui::Grid::new("manager-transfer-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Sender management agent");

                ui.text_edit_singleline(&mut self.sender_agent);

                ui.end_row();

                ui.label("Source path on sender");

                ui.text_edit_singleline(&mut self.source_root);

                ui.end_row();

                ui.label("Receiver management agent");

                ui.text_edit_singleline(&mut self.receiver_agent);

                ui.end_row();

                ui.label("Destination path on receiver");

                ui.text_edit_singleline(&mut self.destination_root);

                ui.end_row();

                ui.label("Scanner workers");

                ui.add(egui::DragValue::new(&mut self.worker_count).range(1..=64));

                ui.end_row();

                ui.label("Calibration size (MiB)");

                ui.add(egui::DragValue::new(&mut self.calibration_mib).range(1..=4096));

                ui.end_row();
            });

        ui.checkbox(
            &mut self.update_existing,
            "Update and verify an existing destination",
        );

        if !self.sender_agent.is_empty() && self.sender_agent == self.receiver_agent {
            ui.label("Sender and receiver cannot be the same agent.");
        }

        ui.add_space(8.0);

        let transfer_active = self.transfer.is_some() && !self.monitoring_complete;

        let can_start = self.start_receiver.is_none()
            && !transfer_active
            && !self.sender_agent.trim().is_empty()
            && !self.receiver_agent.trim().is_empty()
            && !self.source_root.trim().is_empty()
            && !self.destination_root.trim().is_empty()
            && self.sender_agent != self.receiver_agent;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_start, egui::Button::new("Start managed transfer"))
                .clicked()
            {
                self.begin_transfer();
            }

            if self.start_receiver.is_some() {
                ui.spinner();

                ui.label("Starting endpoints...");
            }
        });
    }

    fn render_transfer(&mut self, ui: &mut egui::Ui) {
        ui.heading("Current transfer");

        let Some(transfer) = self.transfer.clone() else {
            ui.label("No managed transfer has been started from this window.");

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
            let can_cancel = !self.monitoring_complete && self.cancel_receiver.is_none();

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

            if self.monitoring_complete && ui.button("Clear transfer card").clicked() {
                self.transfer = None;

                self.sender_snapshot = None;

                self.receiver_snapshot = None;

                self.monitoring_complete = false;
            }
        });
    }
}

impl eframe::App for NetworkCopyManager {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _frame: &mut eframe::Frame,
    ) {
        self.process_messages();

        self.begin_poll();

        if self.has_background_work() {
            ui.ctx().request_repaint_after(
                REPAINT_INTERVAL,
            );
        }

        egui::CentralPanel::default()
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([
                        false,
                        false,
                    ])
                    .show(ui, |ui| {
                        ui.set_width(
                            ui.available_width(),
                        );

                        ui.horizontal(|ui| {
                            ui.heading(APP_NAME);

                            ui.separator();

                            ui.label(format!(
                                "NetworkCopy Speed Edition {}",
                                env!(
                                    "CARGO_PKG_VERSION"
                                ),
                            ));
                        });

                        ui.label(
                            "Trusted-LAN development mode — management traffic is currently unauthenticated.",
                        );

                        ui.add_space(12.0);

                        ui.separator();

                        ui.add_space(12.0);

                        self.render_discovery(ui);

                        ui.add_space(12.0);

                        ui.separator();

                        ui.add_space(12.0);

                        self.render_configuration(
                            ui,
                        );

                        ui.add_space(12.0);

                        ui.separator();

                        ui.add_space(12.0);

                        self.render_transfer(ui);

                        if !self.notice.is_empty() {
                            ui.add_space(12.0);

                            ui.separator();

                            ui.add_space(8.0);

                            ui.label(
                                &self.notice,
                            );
                        }

                        if !self.error.is_empty() {
                            ui.add_space(12.0);

                            ui.separator();

                            ui.add_space(8.0);

                            ui.strong("Error");

                            ui.label(
                                &self.error,
                            );
                        }

                        ui.add_space(16.0);
                    });
            });
    }
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

        if let Some(active) = &snapshot.active {
            if active.job_id == expected_job_id {
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
        }

        let result = snapshot
            .latest_result
            .as_ref()
            .filter(|result| result.job_id == expected_job_id);

        if let Some(result) = result {
            ui.separator();

            ui.strong(format!("Result: {}", result.outcome.label(),));

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

fn configure_style(
    context: &egui::Context,
) {
    context.set_theme(
        egui::Theme::Dark,
    );

    context.style_mut_of(
        egui::Theme::Dark,
        |style| {
            style.spacing.item_spacing =
                egui::vec2(10.0, 8.0);

            style.spacing.button_padding =
                egui::vec2(12.0, 7.0);

            style
                .text_styles
                .insert(
                    egui::TextStyle::Heading,
                    egui::FontId::
                        proportional(22.0),
                );

            style
                .text_styles
                .insert(
                    egui::TextStyle::Body,
                    egui::FontId::
                        proportional(15.0),
                );

            style
                .text_styles
                .insert(
                    egui::TextStyle::Button,
                    egui::FontId::
                        proportional(15.0),
                );
        },
    );

    let mut visuals =
        egui::Visuals::dark();

    visuals.panel_fill =
        egui::Color32::from_rgb(
            10,
            17,
            27,
        );

    visuals.window_fill =
        egui::Color32::from_rgb(
            16,
            27,
            42,
        );

    visuals.extreme_bg_color =
        egui::Color32::from_rgb(
            5,
            10,
            17,
        );

    visuals.selection.bg_fill =
        egui::Color32::from_rgb(
            0,
            128,
            194,
        );

    context.set_visuals_of(
        egui::Theme::Dark,
        visuals,
    );
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 820.0])
            .with_min_inner_size([760.0, 600.0]),

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
