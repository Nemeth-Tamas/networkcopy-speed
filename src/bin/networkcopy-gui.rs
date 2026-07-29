#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use networkcopy_speed::gui_session;
use networkcopy_speed::gui_transfer::{
    GuiConnectionMode, GuiTransferControl, GuiTransferProgress, GuiTransferRequest,
    GuiTransferSummary, run_gui_transfer_with_control,
};
use rfd::FileDialog;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "NetworkCopy Speed Edition";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([640.0, 480.0]),

        centered: true,
        renderer: eframe::Renderer::Glow,

        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_creation_context| Ok(Box::new(NetworkCopyGui::new()))),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Language {
    Hungarian,
    English,
}

impl Language {
    const fn initial() -> Self {
        #[cfg(feature = "default-language-en")]
        {
            Self::English
        }

        #[cfg(not(feature = "default-language-en"))]
        {
            Self::Hungarian
        }
    }

    const fn text(self) -> Text {
        match self {
            Self::Hungarian => Text::HUNGARIAN,

            Self::English => Text::ENGLISH,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferMode {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionChoice {
    Direct,
    Address,
}

enum TransferOutcome {
    Completed(GuiTransferSummary),

    Cancelled,

    Failed(String),
}

struct LiveProgress {
    phase: String,
    completed: u64,
    total: u64,
    cancel_requested: bool,
    phase_started: Instant,
    phase_start_completed: u64,
}

impl LiveProgress {
    fn new(snapshot: GuiTransferProgress) -> Self {
        Self {
            phase: snapshot.phase,

            completed: snapshot.completed,

            total: snapshot.total,

            cancel_requested: snapshot.cancel_requested,

            phase_started: Instant::now(),

            phase_start_completed: snapshot.completed,
        }
    }

    fn update(&mut self, snapshot: GuiTransferProgress) {
        let phase_changed = self.phase != snapshot.phase;

        let counter_restarted = snapshot.completed < self.completed;

        if phase_changed || counter_restarted {
            self.phase_started = Instant::now();

            self.phase_start_completed = snapshot.completed;
        }

        self.phase = snapshot.phase;

        self.completed = snapshot.completed;

        self.total = snapshot.total;

        self.cancel_requested = snapshot.cancel_requested;
    }

    fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }

        (self.completed.min(self.total) as f64 / self.total as f64).clamp(0.0, 1.0) as f32
    }

    fn megabytes_per_second(&self) -> f64 {
        let elapsed = self.phase_started.elapsed();

        if elapsed.is_zero() {
            return 0.0;
        }

        let phase_bytes = self.completed.saturating_sub(self.phase_start_completed);

        phase_bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0
    }
}

#[derive(Clone, Copy)]
struct Text {
    subtitle: &'static str,
    language: &'static str,
    send: &'static str,
    receive: &'static str,
    direct_mode: &'static str,
    direct_connection: &'static str,
    ip_connection: &'static str,
    receiver_address: &'static str,
    bind_address: &'static str,
    address_hint: &'static str,
    send_description: &'static str,
    receive_description: &'static str,
    source_folder: &'static str,
    destination_folder: &'static str,
    source_hint: &'static str,
    destination_hint: &'static str,
    browse: &'static str,
    choose_source: &'static str,
    choose_destination: &'static str,
    scanner_workers: &'static str,
    calibration_mib: &'static str,
    start: &'static str,
    cancel: &'static str,
    running: &'static str,
    cancelling: &'static str,
    cancelled: &'static str,
    cancelled_detail: &'static str,
    resume_title: &'static str,
    resume_question: &'static str,
    resume_direction: &'static str,
    resume_folder: &'static str,
    resume_connection: &'static str,
    resume_continue: &'static str,
    resume_discard: &'static str,
    session_save_failed: &'static str,
    session_load_failed: &'static str,
    session_clear_failed: &'static str,
    completed: &'static str,
    failed: &'static str,
    files: &'static str,
    logical_data: &'static str,
    speed: &'static str,
    wire_savings: &'static str,
    streams: &'static str,
    elapsed: &'static str,
    missing_source: &'static str,
    missing_destination: &'static str,
    invalid_address: &'static str,
    worker_start_failed: &'static str,
    worker_disconnected: &'static str,
    development_status: &'static str,
    engine_pending: &'static str,
}

impl Text {
    const HUNGARIAN: Self = Self {
        subtitle: "Gyors fájlmásolás közvetlen hálózati kapcsolaton",

        language: "Nyelv:",

        send: "Küldés",

        receive: "Fogadás",

        direct_mode: "Kapcsolat és átvitel",

        direct_connection: "Közvetlen kábel",

        ip_connection: "IP-cím",

        receiver_address: "Fogadó címe",

        bind_address: "Helyi figyelési cím",

        address_hint: "például 127.0.0.1:7337",

        send_description: "Válassza ki a küldendő mappát. Közvetlen módban a program automatikusan megkeresi a másik számítógépet; IP-cím módban a megadott címhez csatlakozik.",

        receive_description: "Válassza ki a célmappát. Közvetlen módban a program megvárja az automatikusan felderített küldőt; IP-cím módban a megadott helyi címen figyel.",

        source_folder: "Forrásmappa",

        destination_folder: "Célmappa",

        source_hint: "Válassza ki a küldendő mappát…",

        destination_hint: "Válassza ki a fogadási mappát…",

        browse: "Tallózás…",

        choose_source: "Küldendő mappa kiválasztása",

        choose_destination: "Célmappa kiválasztása",

        scanner_workers: "Fájlkereső szálak",

        calibration_mib: "Kalibráció mérete (MiB)",

        start: "Indítás",

        cancel: "Megszakítás",

        running: "Az átvitel folyamatban",

        cancelling: "Megszakítás folyamatban…",

        cancelled: "Az átvitel megszakítva",

        cancelled_detail: "Az átvitel biztonságosan leállt. A fogadóoldalon elkészült nagyfájl-részletek megmaradnak a későbbi folytatáshoz.",

        resume_title: "Megszakadt átvitel",

        resume_question: "Találtam egy korábban félbemaradt átvitelt. Folytatja ugyanazokkal a beállításokkal?",

        resume_direction: "Művelet",

        resume_folder: "Mappa",

        resume_connection: "Kapcsolat",

        resume_continue: "Folytatás",

        resume_discard: "Elvetés",

        session_save_failed: "Nem sikerült elmenteni a folytatáshoz szükséges adatokat",

        session_load_failed: "Nem sikerült betölteni a félbemaradt átvitelt",

        session_clear_failed: "Nem sikerült törölni a befejezett átvitel folytatási adatait",

        completed: "Az átvitel sikeresen befejeződött",

        failed: "Az átvitel sikertelen",

        files: "Fájlok",

        logical_data: "Adatmennyiség",

        speed: "Sebesség",

        wire_savings: "Hálózati megtakarítás",

        streams: "TCP szálak",

        elapsed: "Idő",

        missing_source: "Nincs kiválasztva forrásmappa",

        missing_destination: "Nincs kiválasztva célmappa",

        invalid_address: "Érvénytelen IP-cím vagy port",

        worker_start_failed: "Nem sikerült elindítani az átviteli háttérfolyamatot",

        worker_disconnected: "Az átviteli háttérfolyamat válasz nélkül leállt",

        development_status: "v1.2 fejlesztői felület",

        engine_pending: "A félbemaradt átvitelek beállításait a program automatikusan megőrzi a későbbi folytatáshoz.",
    };

    const ENGLISH: Self = Self {
        subtitle: "High-speed copying over a direct network connection",

        language: "Language:",

        send: "Send",

        receive: "Receive",

        direct_mode: "Connection and transfer",

        direct_connection: "Direct cable",

        ip_connection: "IP address",

        receiver_address: "Receiver address",

        bind_address: "Local listening address",

        address_hint: "for example 127.0.0.1:7337",

        send_description: "Choose the folder to send. In Direct mode, NetworkCopy automatically discovers the other computer; in IP-address mode, it connects to the specified address.",

        receive_description: "Choose the destination folder. In Direct mode, NetworkCopy waits for an automatically discovered sender; in IP-address mode, it listens on the specified local address.",

        source_folder: "Source folder",

        destination_folder: "Destination folder",

        source_hint: "Choose the folder to send…",

        destination_hint: "Choose the receiving folder…",

        browse: "Browse…",

        choose_source: "Choose source folder",

        choose_destination: "Choose destination folder",

        scanner_workers: "Scanner workers",

        calibration_mib: "Calibration size (MiB)",

        start: "Start",

        cancel: "Cancel",

        running: "Transfer in progress",

        cancelling: "Stopping transfer…",

        cancelled: "Transfer stopped",

        cancelled_detail: "The transfer stopped safely. Completed large-file stripes remain available on the receiver for a later resume.",

        resume_title: "Interrupted transfer",

        resume_question: "A previous transfer did not finish. Continue it using the same settings?",

        resume_direction: "Operation",

        resume_folder: "Folder",

        resume_connection: "Connection",

        resume_continue: "Continue",

        resume_discard: "Discard",

        session_save_failed: "Failed to save the information required to resume this transfer",

        session_load_failed: "Failed to load the interrupted transfer",

        session_clear_failed: "Failed to remove the completed transfer's resume information",

        completed: "Transfer completed successfully",

        failed: "Transfer failed",

        files: "Files",

        logical_data: "Logical data",

        speed: "Speed",

        wire_savings: "Network savings",

        streams: "TCP streams",

        elapsed: "Time",

        missing_source: "No source folder has been selected",

        missing_destination: "No destination folder has been selected",

        invalid_address: "Invalid IP address or port",

        worker_start_failed: "Failed to start the transfer worker",

        worker_disconnected: "The transfer worker stopped without returning a result",

        development_status: "v1.2 development interface",

        engine_pending: "Interrupted transfer settings are saved automatically so the operation can be resumed later.",
    };
}

struct NetworkCopyGui {
    language: Language,
    mode: TransferMode,
    connection: ConnectionChoice,
    source_folder: String,
    destination_folder: String,
    receiver_address: String,
    bind_address: String,
    scanner_workers: usize,
    calibration_mib: u64,
    transfer_receiver: Option<Receiver<TransferOutcome>>,

    transfer_control: Option<GuiTransferControl>,

    live_progress: Option<LiveProgress>,

    last_summary: Option<GuiTransferSummary>,

    last_error: Option<String>,

    last_cancelled: bool,

    pending_session: Option<GuiTransferRequest>,

    show_resume_prompt: bool,

    session_warning: Option<String>,
}

impl NetworkCopyGui {
    fn new() -> Self {
        let language = Language::initial();

        let text = language.text();

        let (pending_session, session_warning) = match gui_session::load_latest() {
            Ok(session) => (session, None),

            Err(error) => (
                None,
                Some(format!("{}: {error}", text.session_load_failed,)),
            ),
        };

        let show_resume_prompt = pending_session.is_some();

        Self {
            language,

            mode: TransferMode::Send,

            connection: ConnectionChoice::Direct,

            source_folder: String::new(),

            destination_folder: String::new(),

            receiver_address: "127.0.0.1:7337".to_string(),

            bind_address: "127.0.0.1:7337".to_string(),

            scanner_workers: 4,
            calibration_mib: 64,

            transfer_receiver: None,

            transfer_control: None,

            live_progress: None,

            last_summary: None,

            last_error: None,

            last_cancelled: false,

            pending_session,

            show_resume_prompt,

            session_warning,
        }
    }

    fn mode_selector(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, TransferMode::Send, text.send);

            ui.selectable_value(&mut self.mode, TransferMode::Receive, text.receive);
        });
    }

    fn connection_selector(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.connection,
                ConnectionChoice::Direct,
                text.direct_connection,
            );

            ui.selectable_value(
                &mut self.connection,
                ConnectionChoice::Address,
                text.ip_connection,
            );
        });

        if self.connection != ConnectionChoice::Address {
            return;
        }

        ui.add_space(8.0);

        let (label, address) = match self.mode {
            TransferMode::Send => (text.receiver_address, &mut self.receiver_address),

            TransferMode::Receive => (text.bind_address, &mut self.bind_address),
        };

        ui.label(label);

        ui.add_sized(
            [ui.available_width(), 28.0],
            egui::TextEdit::singleline(address).hint_text(text.address_hint),
        );
    }

    fn send_panel(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.label(text.send_description);

        ui.add_space(8.0);

        folder_picker(
            ui,
            &mut self.source_folder,
            text.source_folder,
            text.source_hint,
            text.browse,
            text.choose_source,
        );

        ui.add_space(16.0);

        egui::Grid::new("send_options")
            .num_columns(2)
            .spacing([24.0, 12.0])
            .show(ui, |ui| {
                ui.label(text.scanner_workers);

                ui.add(egui::Slider::new(&mut self.scanner_workers, 1..=32));

                ui.end_row();

                ui.label(text.calibration_mib);

                ui.add(egui::Slider::new(&mut self.calibration_mib, 1..=1024).logarithmic(true));

                ui.end_row();
            });
    }

    fn receive_panel(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.label(text.receive_description);

        ui.add_space(8.0);

        folder_picker(
            ui,
            &mut self.destination_folder,
            text.destination_folder,
            text.destination_hint,
            text.browse,
            text.choose_destination,
        );
    }

    fn transfer_request(&self, text: Text) -> Result<GuiTransferRequest, String> {
        let connection = match self.connection {
            ConnectionChoice::Direct => GuiConnectionMode::Direct,

            ConnectionChoice::Address => {
                let value = match self.mode {
                    TransferMode::Send => self.receiver_address.trim(),

                    TransferMode::Receive => self.bind_address.trim(),
                };

                let address = value
                    .parse::<SocketAddr>()
                    .map_err(|error| format!("{}: {error}", text.invalid_address,))?;

                GuiConnectionMode::Address(address)
            }
        };

        match self.mode {
            TransferMode::Send => {
                let source = self.source_folder.trim();

                if source.is_empty() {
                    return Err(text.missing_source.to_string());
                }

                Ok(GuiTransferRequest::Send {
                    connection,

                    source_root: PathBuf::from(source),

                    worker_count: self.scanner_workers,

                    calibration_mib: self.calibration_mib,
                })
            }

            TransferMode::Receive => {
                let destination = self.destination_folder.trim();

                if destination.is_empty() {
                    return Err(text.missing_destination.to_string());
                }

                Ok(GuiTransferRequest::Receive {
                    connection,

                    destination_root: PathBuf::from(destination),
                })
            }
        }
    }

    fn apply_request(&mut self, request: &GuiTransferRequest) {
        match request {
            GuiTransferRequest::Send {
                connection,
                source_root,
                worker_count,
                calibration_mib,
            } => {
                self.mode = TransferMode::Send;

                self.source_folder = source_root.display().to_string();

                self.scanner_workers = *worker_count;

                self.calibration_mib = *calibration_mib;

                self.apply_connection(TransferMode::Send, *connection);
            }

            GuiTransferRequest::Receive {
                connection,
                destination_root,
            } => {
                self.mode = TransferMode::Receive;

                self.destination_folder = destination_root.display().to_string();

                self.apply_connection(TransferMode::Receive, *connection);
            }
        }
    }

    fn apply_connection(&mut self, mode: TransferMode, connection: GuiConnectionMode) {
        match connection {
            GuiConnectionMode::Direct => {
                self.connection = ConnectionChoice::Direct;
            }

            GuiConnectionMode::Address(address) => {
                self.connection = ConnectionChoice::Address;

                match mode {
                    TransferMode::Send => {
                        self.receiver_address = address.to_string();
                    }

                    TransferMode::Receive => {
                        self.bind_address = address.to_string();
                    }
                }
            }
        }
    }

    fn resume_pending(&mut self, text: Text) {
        let Some(request) = self.pending_session.clone() else {
            return;
        };

        self.apply_request(&request);

        self.show_resume_prompt = false;

        self.start_transfer(text);
    }

    fn discard_pending(&mut self, text: Text) {
        let Some(request) = self.pending_session.take() else {
            self.show_resume_prompt = false;

            return;
        };

        match gui_session::clear(&request) {
            Ok(()) => match gui_session::load_latest() {
                Ok(next) => {
                    self.pending_session = next;

                    self.show_resume_prompt = self.pending_session.is_some();
                }

                Err(error) => {
                    self.show_resume_prompt = false;

                    self.session_warning = Some(format!("{}: {error}", text.session_load_failed,));
                }
            },

            Err(error) => {
                self.show_resume_prompt = false;

                self.session_warning = Some(format!("{}: {error}", text.session_clear_failed,));
            }
        }
    }

    fn clear_completed_session(&mut self, text: Text) {
        let Some(request) = self.pending_session.take() else {
            return;
        };

        if let Err(error) = gui_session::clear(&request) {
            self.session_warning = Some(format!("{}: {error}", text.session_clear_failed,));
        }
    }

    fn resume_prompt(&mut self, context: &egui::Context, text: Text) {
        if !self.show_resume_prompt || self.transfer_receiver.is_some() {
            return;
        }

        let Some(request) = self.pending_session.clone() else {
            self.show_resume_prompt = false;

            return;
        };

        let mut resume = false;

        let mut discard = false;

        egui::Window::new(text.resume_title)
            .id(egui::Id::new("networkcopy_resume_prompt"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(context, |ui| {
                ui.set_min_width(440.0);

                ui.label(text.resume_question);

                ui.add_space(12.0);

                resume_request_summary(ui, text, &request);

                ui.add_space(16.0);

                ui.horizontal(|ui| {
                    resume = ui.button(text.resume_continue).clicked();

                    discard = ui.button(text.resume_discard).clicked();
                });
            });

        if resume {
            self.resume_pending(text);
        } else if discard {
            self.discard_pending(text);
        }
    }

    fn start_transfer(&mut self, text: Text) {
        if self.transfer_receiver.is_some() {
            return;
        }

        let request = match self.transfer_request(text) {
            Ok(request) => request,

            Err(error) => {
                self.last_summary = None;

                self.last_error = Some(error);

                self.last_cancelled = false;

                return;
            }
        };

        let session_request = request.clone();

        match gui_session::save(&session_request) {
            Ok(_path) => {
                self.session_warning = None;
            }

            Err(error) => {
                self.session_warning = Some(format!("{}: {error}", text.session_save_failed,));
            }
        }

        self.pending_session = Some(session_request);

        self.show_resume_prompt = false;

        let control = GuiTransferControl::new();

        let worker_control = control.clone();

        let (sender, receiver) = mpsc::channel();

        let worker = thread::Builder::new()
            .name("networkcopy-gui-transfer".to_string())
            .spawn(move || {
                let outcome = match run_gui_transfer_with_control(request, worker_control) {
                    Ok(summary) => TransferOutcome::Completed(summary),

                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        TransferOutcome::Cancelled
                    }

                    Err(error) => TransferOutcome::Failed(error.to_string()),
                };

                let _ = sender.send(outcome);
            });

        match worker {
            Ok(_worker) => {
                self.transfer_receiver = Some(receiver);

                self.transfer_control = Some(control);

                self.live_progress = None;

                self.last_summary = None;

                self.last_error = None;

                self.last_cancelled = false;
            }

            Err(error) => {
                self.transfer_control = None;

                self.live_progress = None;

                self.last_summary = None;

                self.last_cancelled = false;

                self.last_error = Some(format!("{}: {error}", text.worker_start_failed,));
            }
        }
    }

    fn poll_transfer(&mut self, context: &egui::Context, text: Text) {
        if let Some(control) = self.transfer_control.as_ref() {
            let snapshot = control.progress();

            match self.live_progress.as_mut() {
                Some(progress) => {
                    progress.update(snapshot);
                }

                None => {
                    self.live_progress = Some(LiveProgress::new(snapshot));
                }
            }

            context.request_repaint_after(Duration::from_millis(100));
        }

        let Some(receiver) = self.transfer_receiver.as_ref() else {
            return;
        };

        let outcome = match receiver.try_recv() {
            Ok(outcome) => Some(outcome),

            Err(TryRecvError::Empty) => None,

            Err(TryRecvError::Disconnected) => Some(TransferOutcome::Failed(
                text.worker_disconnected.to_string(),
            )),
        };

        let Some(outcome) = outcome else {
            return;
        };

        self.transfer_receiver = None;

        self.transfer_control = None;

        self.live_progress = None;

        match outcome {
            TransferOutcome::Completed(summary) => {
                self.clear_completed_session(text);

                self.last_summary = Some(summary);

                self.last_error = None;

                self.last_cancelled = false;
            }

            TransferOutcome::Cancelled => {
                self.last_summary = None;

                self.last_error = None;

                self.last_cancelled = true;
            }

            TransferOutcome::Failed(error) => {
                self.last_summary = None;

                self.last_error = Some(error);

                self.last_cancelled = false;
            }
        }
    }

    fn summary_panel(&self, ui: &mut egui::Ui, text: Text) {
        let Some(summary) = &self.last_summary else {
            return;
        };

        ui.group(|ui| {
            ui.heading(text.completed);

            ui.add_space(6.0);

            egui::Grid::new("transfer_summary")
                .num_columns(2)
                .spacing([24.0, 8.0])
                .show(ui, |ui| {
                    ui.label(text.files);

                    ui.label(summary.files.to_string());

                    ui.end_row();

                    ui.label(text.logical_data);

                    ui.label(format_bytes(summary.logical_bytes));

                    ui.end_row();

                    ui.label(text.speed);

                    ui.label(format!("{:.2} MB/s", summary.logical_megabytes_per_second,));

                    ui.end_row();

                    ui.label(text.wire_savings);

                    ui.label(format!("{:.2}%", summary.wire_savings_percent,));

                    ui.end_row();

                    ui.label(text.streams);

                    ui.label(summary.data_stream_count.to_string());

                    ui.end_row();

                    ui.label(text.elapsed);

                    ui.label(format!("{:.2} s", summary.elapsed.as_secs_f64(),));

                    ui.end_row();
                });
        });
    }

    fn live_progress_panel(&self, ui: &mut egui::Ui, text: Text) {
        let Some(progress) = &self.live_progress else {
            return;
        };

        let phase = localized_phase(self.language, &progress.phase);

        ui.group(|ui| {
            ui.label(egui::RichText::new(text.running).strong());

            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.spinner();

                if progress.cancel_requested {
                    ui.label(text.cancelling);
                } else {
                    ui.label(phase);
                }
            });

            ui.add_space(8.0);

            if progress.total > 0 {
                let amount = format!(
                    "{} / {}",
                    format_bytes(progress.completed.min(progress.total,),),
                    format_bytes(progress.total,),
                );

                ui.add(
                    egui::ProgressBar::new(progress.fraction())
                        .show_percentage()
                        .text(amount),
                );

                ui.add_space(6.0);

                ui.label(format!(
                    "{}: {:.2} MB/s",
                    text.speed,
                    progress.megabytes_per_second(),
                ));
            } else {
                ui.label(format_bytes(progress.completed));
            }
        });
    }

    fn action_buttons(&mut self, ui: &mut egui::Ui, text: Text) {
        let running = self.transfer_receiver.is_some();

        let cancel_requested = self
            .live_progress
            .as_ref()
            .is_some_and(|progress| progress.cancel_requested);

        ui.horizontal(|ui| {
            let start = ui.add_enabled(
                !running,
                egui::Button::new(text.start).min_size(egui::vec2(120.0, 34.0)),
            );

            if start.clicked() {
                self.start_transfer(text);

                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }

            let cancel = ui.add_enabled(
                running && !cancel_requested,
                egui::Button::new(text.cancel).min_size(egui::vec2(120.0, 34.0)),
            );

            if cancel.clicked() {
                if let Some(control) = self.transfer_control.as_ref() {
                    control.cancel();
                }

                if let Some(progress) = self.live_progress.as_mut() {
                    progress.cancel_requested = true;
                }

                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
        });

        if running {
            ui.add_space(12.0);

            self.live_progress_panel(ui, text);
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui, text: Text) {
        if let Some(warning) = &self.session_warning {
            ui.label(egui::RichText::new(warning).italics());

            ui.add_space(8.0);
        }

        let can_resume = self.pending_session.is_some() && self.transfer_receiver.is_none();

        if let Some(error) = self.last_error.clone() {
            let mut resume = false;

            ui.group(|ui| {
                ui.label(egui::RichText::new(text.failed).strong());

                ui.label(error);

                if can_resume {
                    ui.add_space(8.0);

                    resume = ui.button(text.resume_continue).clicked();
                }
            });

            if resume {
                self.resume_pending(text);
            }
        } else if self.last_cancelled {
            let mut resume = false;

            ui.group(|ui| {
                ui.label(egui::RichText::new(text.cancelled).strong());

                ui.label(text.cancelled_detail);

                if can_resume {
                    ui.add_space(8.0);

                    resume = ui.button(text.resume_continue).clicked();
                }
            });

            if resume {
                self.resume_pending(text);
            }
        } else if self.last_summary.is_some() {
            self.summary_panel(ui, text);
        } else {
            ui.label(egui::RichText::new(text.development_status).strong());

            ui.label(text.engine_pending);
        }
    }
}

impl eframe::App for NetworkCopyGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let text = self.language.text();

        self.poll_transfer(ui.ctx(), text);

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading(APP_NAME);

                    ui.label(text.subtitle);
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.selectable_value(&mut self.language, Language::English, "English");

                    ui.selectable_value(&mut self.language, Language::Hungarian, "Magyar");

                    ui.label(text.language);
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            self.mode_selector(ui, text);

            ui.add_space(12.0);

            ui.group(|ui| {
                ui.heading(text.direct_mode);

                ui.add_space(6.0);

                self.connection_selector(ui, text);

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                match self.mode {
                    TransferMode::Send => {
                        self.send_panel(ui, text);
                    }

                    TransferMode::Receive => {
                        self.receive_panel(ui, text);
                    }
                }
            });

            ui.add_space(18.0);

            self.action_buttons(ui, text);

            ui.add_space(18.0);
            ui.separator();
            ui.add_space(10.0);

            self.status_panel(ui, text);
        });

        self.resume_prompt(ui.ctx(), text);
    }
}

fn resume_request_summary(ui: &mut egui::Ui, text: Text, request: &GuiTransferRequest) {
    let (direction, folder, connection, send_options) = match request {
        GuiTransferRequest::Send {
            connection,
            source_root,
            worker_count,
            calibration_mib,
        } => (
            text.send,
            source_root,
            *connection,
            Some((*worker_count, *calibration_mib)),
        ),

        GuiTransferRequest::Receive {
            connection,
            destination_root,
        } => (text.receive, destination_root, *connection, None),
    };

    let connection = match connection {
        GuiConnectionMode::Direct => text.direct_connection.to_string(),

        GuiConnectionMode::Address(address) => {
            format!("{} — {address}", text.ip_connection,)
        }
    };

    egui::Grid::new("resume_request_summary")
        .num_columns(2)
        .spacing([24.0, 8.0])
        .show(ui, |ui| {
            ui.label(text.resume_direction);

            ui.label(direction);

            ui.end_row();

            ui.label(text.resume_folder);

            ui.label(folder.display().to_string());

            ui.end_row();

            ui.label(text.resume_connection);

            ui.label(connection);

            ui.end_row();

            if let Some((worker_count, calibration_mib)) = send_options {
                ui.label(text.scanner_workers);

                ui.label(worker_count.to_string());

                ui.end_row();

                ui.label(text.calibration_mib);

                ui.label(calibration_mib.to_string());

                ui.end_row();
            }
        });
}

fn localized_phase(language: Language, phase: &str) -> String {
    if language == Language::English {
        return phase.to_string();
    }

    match phase {
        "Preparing transfer" => "Átvitel előkészítése".to_string(),

        "Discovering direct receiver" => "Közvetlen fogadó keresése".to_string(),

        "Waiting for direct sender" => "Várakozás a közvetlen küldőre".to_string(),

        "Connecting calibration" => "Kapcsolódás a sebességméréshez".to_string(),

        "Waiting for calibration" => "Várakozás a sebességmérésre".to_string(),

        "Scanning source" => "Forrásmappa vizsgálata".to_string(),

        "Waiting for transfer" => "Várakozás az átvitelre".to_string(),

        "Transfer send" => "Fájlok küldése".to_string(),

        "Transfer receive" => "Fájlok fogadása".to_string(),

        "Complete" => "Kész".to_string(),

        _ => {
            if let Some(streams) = phase
                .strip_prefix("Calibration send - ")
                .and_then(|value| value.strip_suffix(" streams"))
            {
                return format!("Sebességmérés küldése — {streams} TCP szál",);
            }

            if let Some(streams) = phase
                .strip_prefix("Calibration receive - ")
                .and_then(|value| value.strip_suffix(" streams"))
            {
                return format!("Sebességmérés fogadása — {streams} TCP szál",);
            }

            phase.to_string()
        }
    }
}

fn folder_picker(
    ui: &mut egui::Ui,
    value: &mut String,
    label: &str,
    hint: &str,
    browse: &str,
    dialog_title: &str,
) {
    ui.label(label);

    ui.horizontal(|ui| {
        let button_width = 110.0;

        let spacing = ui.spacing().item_spacing.x;

        let edit_width = (ui.available_width() - button_width - spacing).max(160.0);

        ui.add_sized(
            [edit_width, 28.0],
            egui::TextEdit::singleline(value).hint_text(hint),
        );

        let clicked = ui
            .add_sized([button_width, 28.0], egui::Button::new(browse))
            .clicked();

        if clicked && let Some(path) = FileDialog::new().set_title(dialog_title).pick_folder() {
            *value = path.display().to_string();
        }
    });
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;

    const MIB: f64 = 1024.0 * KIB;

    const GIB: f64 = 1024.0 * MIB;

    let bytes = bytes as f64;

    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB,)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB,)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB,)
    } else {
        format!("{bytes:.0} B",)
    }
}

#[cfg(test)]
mod tests {
    use super::{Language, Text, localized_phase};

    #[test]
    fn both_languages_are_embedded() {
        assert_eq!(Text::HUNGARIAN.send, "Küldés",);

        assert_eq!(Text::ENGLISH.send, "Send",);
    }

    #[test]
    fn initial_language_matches_feature() {
        let expected = if cfg!(feature = "default-language-en") {
            Language::English
        } else {
            Language::Hungarian
        };

        assert_eq!(Language::initial(), expected,);
    }

    #[test]
    fn calibration_phase_is_localized() {
        assert_eq!(
            localized_phase(Language::Hungarian, "Calibration send - 4 streams",),
            "Sebességmérés küldése — 4 TCP szál",
        );
    }
}
