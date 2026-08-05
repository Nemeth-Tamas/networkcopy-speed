#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use networkcopy_speed::destination_layout::DestinationLayout;
use networkcopy_speed::gui_session;
use networkcopy_speed::gui_transfer::{
    GuiConnectionMode, GuiTransferControl, GuiTransferDiagnostic, GuiTransferProgress,
    GuiTransferRequest, GuiTransferSummary, run_gui_transfer_with_control,
};
use networkcopy_speed::windows_elevation;
use rfd::FileDialog;
use std::env;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

const APP_NAME: &str = "NetworkCopy Speed Edition";

const AUTO_RESUME_RECEIVE_ARGUMENT: &str = "--resume-receive";

fn main() -> eframe::Result {
    let auto_resume_receive =
        env::args_os().any(|argument| argument == OsStr::new(AUTO_RESUME_RECEIVE_ARGUMENT));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 720.0])
            .with_min_inner_size([680.0, 520.0]),

        centered: true,
        renderer: eframe::Renderer::Wgpu,

        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation_context| {
            configure_style(&creation_context.egui_ctx);

            Ok(Box::new(NetworkCopyGui::new(auto_resume_receive)))
        }),
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
    Completed(Box<GuiTransferSummary>),

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

    destination_layout: &'static str,

    exact_destination: &'static str,

    destination_root_layout: &'static str,

    destination_layout_hint: &'static str,

    update_existing: &'static str,
    update_existing_hint: &'static str,
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
    resume_update_mode: &'static str,

    resume_destination_layout: &'static str,

    enabled: &'static str,
    disabled: &'static str,
    resume_continue: &'static str,
    resume_discard: &'static str,
    session_save_failed: &'static str,
    session_load_failed: &'static str,
    session_clear_failed: &'static str,
    completed: &'static str,
    failed: &'static str,
    files: &'static str,
    skipped_files: &'static str,
    skipped_data: &'static str,
    logical_data: &'static str,
    speed: &'static str,
    wire_savings: &'static str,

    exact_reuse_files: &'static str,
    exact_reuse_data: &'static str,
    exact_reuse_wire: &'static str,
    exact_reuse_savings: &'static str,

    cdc_updates: &'static str,
    cdc_fallbacks: &'static str,
    cdc_data: &'static str,
    cdc_wire: &'static str,
    cdc_savings: &'static str,

    tiny_packs: &'static str,
    compressed_tiny_packs: &'static str,
    raw_tiny_packs: &'static str,
    packed_tiny_files: &'static str,
    tiny_pack_data: &'static str,
    tiny_pack_savings: &'static str,
    streams: &'static str,
    tiny_write_workers: &'static str,
    elapsed: &'static str,
    missing_source: &'static str,
    missing_destination: &'static str,
    invalid_address: &'static str,
    worker_start_failed: &'static str,
    worker_disconnected: &'static str,
    elevation_failed: &'static str,
    development_status: &'static str,
    engine_pending: &'static str,
    compression_strategy: &'static str,
    compression_strategy_adaptive: &'static str,
    transfer_diagnostic: &'static str,
    diagnostic_all_skipped: &'static str,
    diagnostic_tiny_files: &'static str,
    diagnostic_exact_reuse: &'static str,
    diagnostic_cdc_effective: &'static str,
    diagnostic_compression_effective: &'static str,
    diagnostic_compression_bypassed: &'static str,
    diagnostic_balanced: &'static str,
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

        destination_layout: "Fogadási elrendezés",

        exact_destination: "Pontos célmappa",

        destination_root_layout: "Célgyökér",

        destination_layout_hint: "Pontos célmappánál a fájlok közvetlenül a kiválasztott mappába kerülnek. Célgyökér módban a küldött mappa neve automatikusan hozzáadódik, például D:\\Mentés\\Desktop. Sikeres átvitel után a fogadó automatikusan várja a következő mappát, amíg meg nem szakítja.",

        update_existing: "Meglévő célmappa frissítése",

        update_existing_hint: "Új célmappánál az azonos közepes fájlokat egyszer küldi át, majd helyben újra felhasználja. Frissítéskor a módosult közepes és nagy fájlok meglévő tartalmából csak a szükséges eltéréseket küldi át. A célfájlt csak sikeres ellenőrzés után cseréli le.",

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

        resume_update_mode: "Meglévő mappa frissítése",

        resume_destination_layout: "Fogadási elrendezés",

        enabled: "Bekapcsolva",

        disabled: "Kikapcsolva",

        resume_continue: "Folytatás",

        resume_discard: "Elvetés",

        session_save_failed: "Nem sikerült elmenteni a folytatáshoz szükséges adatokat",

        session_load_failed: "Nem sikerült betölteni a félbemaradt átvitelt",

        session_clear_failed: "Nem sikerült törölni a befejezett átvitel folytatási adatait",

        completed: "Az átvitel sikeresen befejeződött",

        failed: "Az átvitel sikertelen",

        files: "Fájlok",

        skipped_files: "Kihagyott változatlan fájlok",

        skipped_data: "Kihagyott adatmennyiség",

        logical_data: "Adatmennyiség",

        speed: "Sebesség",

        wire_savings: "Hálózati megtakarítás",

        exact_reuse_files: "Helyben újrafelhasznált azonos fájlok",

        exact_reuse_data: "Azonos fájlok újrafelhasznált adata",

        exact_reuse_wire: "Azonosfájl-terv hálózati mérete",

        exact_reuse_savings: "Azonosfájl-megtakarítás",

        cdc_updates: "CDC-frissítések (kész / felkínált)",

        cdc_fallbacks: "Teljes fájlra visszaváltás",

        cdc_data: "CDC-adat (logikai / újrafelhasznált / új)",

        cdc_wire: "CDC-hálózat (index / terv)",

        cdc_savings: "CDC-megtakarítás",

        tiny_packs: "Aprófájl-csomagok",

        compressed_tiny_packs: "Tömörített csomagok",

        raw_tiny_packs: "Nyers csomagok",

        packed_tiny_files: "Csomagolt aprófájlok",

        tiny_pack_data: "Aprófájl-adat (logikai / hálózati)",

        tiny_pack_savings: "Aprófájl-megtakarítás",

        streams: "TCP szálak",

        tiny_write_workers: "Aprófájl-író szálak",

        elapsed: "Idő",

        missing_source: "Nincs kiválasztva forrásmappa",

        missing_destination: "Nincs kiválasztva célmappa",

        invalid_address: "Érvénytelen IP-cím vagy port",

        worker_start_failed: "Nem sikerült elindítani az átviteli háttérfolyamatot",

        worker_disconnected: "Az átviteli háttérfolyamat válasz nélkül leállt",

        elevation_failed: "A fogadás rendszergazdai indítása nem sikerült",

        development_status: "Készen áll az átvitelre",

        engine_pending: "Válassza ki a kapcsolatot és a mappát, majd indítsa el az átvitelt. A félbemaradt műveletek később folytathatók.",

        compression_strategy: "Tömörítési stratégia",

        compression_strategy_adaptive: "Automatikus, rekordonkénti próba",

        transfer_diagnostic: "Átviteli diagnosztika",

        diagnostic_all_skipped: "Nincs átküldendő adat; minden fájl már naprakész volt.",

        diagnostic_tiny_files: "Valószínű korlát: fájlonkénti többletmunka a sok apró fájl miatt.",

        diagnostic_exact_reuse: "Az azonos fájlok helyi újrafelhasználása elkerülte ugyanazon tartalom ismételt átküldését.",

        diagnostic_cdc_effective: "A tartalomalapú újrafelhasználás jelentősen csökkentette a hálózati forgalmat.",

        diagnostic_compression_effective: "A tömörítés érdemben csökkentette a hálózati forgalmat.",

        diagnostic_compression_bypassed: "Az adat nagyrészt nem tömöríthető; a nyers átvitel volt célszerűbb.",

        diagnostic_balanced: "Az átviteli adatok alapján nem látszik egyetlen domináns korlát.",
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

        destination_layout: "Receiver layout",

        exact_destination: "Exact destination",

        destination_root_layout: "Destination root",

        destination_layout_hint: "Exact destination places the files directly in the selected folder. Destination root automatically appends the sender's folder name, for example D:\\Backup\\Desktop. After each successful transfer, the receiver automatically waits for the next folder until cancelled.",

        update_existing: "Update existing destination",

        update_existing_hint: "For a fresh destination, identical medium files are transferred once and then reused locally. During updates, existing content from changed medium and large files is reused so only the necessary differences cross the network. Destination files are replaced only after verification succeeds.",

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

        resume_update_mode: "Update existing destination",

        resume_destination_layout: "Receiver layout",

        enabled: "Enabled",

        disabled: "Disabled",

        resume_continue: "Continue",

        resume_discard: "Discard",

        session_save_failed: "Failed to save the information required to resume this transfer",

        session_load_failed: "Failed to load the interrupted transfer",

        session_clear_failed: "Failed to remove the completed transfer's resume information",

        completed: "Transfer completed successfully",

        failed: "Transfer failed",

        files: "Files",

        skipped_files: "Skipped unchanged files",

        skipped_data: "Skipped data",

        logical_data: "Logical data",

        speed: "Speed",

        wire_savings: "Network savings",

        exact_reuse_files: "Exact files reused locally",

        exact_reuse_data: "Exact data reused locally",

        exact_reuse_wire: "Exact-reuse plan wire size",

        exact_reuse_savings: "Exact-reuse savings",

        cdc_updates: "CDC updates (completed / offered)",

        cdc_fallbacks: "Whole-file fallbacks",

        cdc_data: "CDC data (logical / reused / literal)",

        cdc_wire: "CDC wire (index / plan)",

        cdc_savings: "CDC savings",

        tiny_packs: "Tiny-file packs",

        compressed_tiny_packs: "Compressed packs",

        raw_tiny_packs: "Raw packs",

        packed_tiny_files: "Packed tiny files",

        tiny_pack_data: "Tiny-pack data (logical / wire)",

        tiny_pack_savings: "Tiny-pack savings",

        streams: "TCP streams",

        tiny_write_workers: "Tiny write workers",

        elapsed: "Time",

        missing_source: "No source folder has been selected",

        missing_destination: "No destination folder has been selected",

        invalid_address: "Invalid IP address or port",

        worker_start_failed: "Failed to start the transfer worker",

        worker_disconnected: "The transfer worker stopped without returning a result",

        elevation_failed: "Failed to start the receiver with administrator privileges",

        development_status: "Ready to transfer",

        engine_pending: "Choose the connection and folder, then start the transfer. Interrupted operations can be resumed later.",

        compression_strategy: "Compression strategy",

        compression_strategy_adaptive: "Automatic per-record probing",

        transfer_diagnostic: "Transfer diagnostic",

        diagnostic_all_skipped: "No payload was required; every file was already current.",

        diagnostic_tiny_files: "Likely limiter: per-file overhead from a large number of tiny files.",

        diagnostic_exact_reuse: "Exact-file reuse avoided retransmitting duplicate content.",

        diagnostic_cdc_effective: "Content-defined reuse avoided retransmitting most of the changed-file data.",

        diagnostic_compression_effective: "Compression meaningfully reduced network traffic.",

        diagnostic_compression_bypassed: "The data was mostly incompressible, so raw transfer was the better choice.",

        diagnostic_balanced: "No single dominant limiter is visible from the transfer telemetry.",
    };
}

struct NetworkCopyGui {
    language: Language,
    mode: TransferMode,
    connection: ConnectionChoice,
    source_folder: String,
    destination_folder: String,

    destination_layout: DestinationLayout,

    update_existing: bool,
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

    auto_start_pending: bool,

    close_requested: bool,
}

impl NetworkCopyGui {
    fn new(auto_resume_receive: bool) -> Self {
        let language = Language::initial();

        let text = language.text();

        let load_result = if auto_resume_receive {
            gui_session::load_receive()
        } else {
            gui_session::load_latest()
        };

        let (pending_session, session_warning) = match load_result {
            Ok(session) => (session, None),

            Err(error) => (
                None,
                Some(format!("{}: {error}", text.session_load_failed,)),
            ),
        };

        let has_pending_session = pending_session.is_some();

        let show_resume_prompt = has_pending_session && !auto_resume_receive;

        let auto_start_pending = has_pending_session && auto_resume_receive;

        Self {
            language,

            mode: TransferMode::Send,

            connection: ConnectionChoice::Direct,

            source_folder: String::new(),

            destination_folder: String::new(),

            destination_layout: DestinationLayout::Exact,

            update_existing: false,

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

            auto_start_pending,

            close_requested: false,
        }
    }

    fn mode_selector(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(20.0, 10.0);

            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.mode,
                    TransferMode::Send,
                    egui::RichText::new(text.send).strong(),
                );

                ui.selectable_value(
                    &mut self.mode,
                    TransferMode::Receive,
                    egui::RichText::new(text.receive).strong(),
                );
            });
        });
    }

    fn connection_selector(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(18.0, 8.0);

            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.connection,
                    ConnectionChoice::Direct,
                    egui::RichText::new(text.direct_connection).strong(),
                );

                ui.selectable_value(
                    &mut self.connection,
                    ConnectionChoice::Address,
                    egui::RichText::new(text.ip_connection).strong(),
                );
            });
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

        ui.add_space(12.0);

        ui.label(text.destination_layout);

        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.destination_layout,
                DestinationLayout::Exact,
                text.exact_destination,
            );

            ui.selectable_value(
                &mut self.destination_layout,
                DestinationLayout::SourceNameUnderRoot,
                text.destination_root_layout,
            );
        });

        ui.label(
            egui::RichText::new(text.destination_layout_hint)
                .small()
                .color(muted_text()),
        );

        ui.add_space(14.0);

        ui.checkbox(&mut self.update_existing, text.update_existing)
            .on_hover_text(text.update_existing_hint);

        ui.label(
            egui::RichText::new(text.update_existing_hint)
                .small()
                .color(muted_text()),
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

                    destination_layout: self.destination_layout,

                    update_existing: self.update_existing,
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
                destination_layout,
                update_existing,
            } => {
                self.mode = TransferMode::Receive;

                self.destination_folder = destination_root.display().to_string();

                self.destination_layout = *destination_layout;

                self.update_existing = *update_existing;

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

        self.start_request(request, text, true, false);
    }

    fn start_request(
        &mut self,
        request: GuiTransferRequest,
        text: Text,
        save_session: bool,
        preserve_summary: bool,
    ) {
        if self.transfer_receiver.is_some() {
            return;
        }

        if save_session {
            match gui_session::save(&request) {
                Ok(_path) => {
                    self.session_warning = None;
                }

                Err(error) => {
                    self.session_warning = Some(format!("{}: {error}", text.session_save_failed,));
                }
            }

            self.pending_session = Some(request.clone());
        }

        self.show_resume_prompt = false;

        let is_receive = matches!(&request, GuiTransferRequest::Receive { .. },);

        if is_receive {
            let elevated = match windows_elevation::is_elevated() {
                Ok(elevated) => elevated,

                Err(error) => {
                    if !preserve_summary {
                        self.last_summary = None;
                    }

                    self.last_cancelled = false;

                    self.last_error = Some(format!("{}: {error}", text.elevation_failed,));

                    return;
                }
            };

            if !elevated {
                match windows_elevation::relaunch_elevated(OsStr::new(AUTO_RESUME_RECEIVE_ARGUMENT))
                {
                    Ok(()) => {
                        self.close_requested = true;
                    }

                    Err(error) => {
                        if !preserve_summary {
                            self.last_summary = None;
                        }

                        self.last_cancelled = false;

                        self.last_error = Some(format!("{}: {error}", text.elevation_failed,));
                    }
                }

                return;
            }
        }

        let control = GuiTransferControl::new();

        let worker_control = control.clone();

        let (sender, receiver) = mpsc::channel();

        let worker = thread::Builder::new()
            .name("networkcopy-gui-transfer".to_string())
            .spawn(move || {
                let outcome = match run_gui_transfer_with_control(request, worker_control) {
                    Ok(summary) => TransferOutcome::Completed(Box::new(summary)),

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

                if !preserve_summary {
                    self.last_summary = None;
                }

                self.last_error = None;

                self.last_cancelled = false;
            }

            Err(error) => {
                self.transfer_control = None;

                self.live_progress = None;

                if !preserve_summary {
                    self.last_summary = None;
                }

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

        let rearm_request = if matches!(&outcome, TransferOutcome::Completed(_),) {
            self.pending_session
                .as_ref()
                .filter(|request| should_rearm_root_receiver(request))
                .cloned()
        } else {
            None
        };

        self.transfer_receiver = None;

        self.transfer_control = None;

        self.live_progress = None;

        match outcome {
            TransferOutcome::Completed(summary) => {
                self.last_summary = Some(*summary);

                self.last_error = None;

                self.last_cancelled = false;

                if let Some(request) = rearm_request {
                    self.start_request(request, text, false, true);
                } else {
                    self.clear_completed_session(text);
                }
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

        card(ui, brand_green(), |ui| {
            ui.heading(egui::RichText::new(text.completed).color(brand_green()));

            ui.add_space(10.0);

            egui::Grid::new("transfer_summary")
                .num_columns(2)
                .spacing([32.0, 10.0])
                .show(ui, |ui| {
                    ui.label(text.files);

                    ui.strong(summary.files.to_string());

                    ui.end_row();

                    if summary.skipped_files > 0 {
                        ui.label(text.skipped_files);

                        ui.strong(summary.skipped_files.to_string());

                        ui.end_row();

                        ui.label(text.skipped_data);

                        ui.strong(format_bytes(summary.skipped_bytes));

                        ui.end_row();
                    }

                    ui.label(text.logical_data);

                    ui.strong(format_bytes(summary.logical_bytes));

                    ui.end_row();

                    ui.label(text.speed);

                    ui.strong(format!("{:.2} MB/s", summary.logical_megabytes_per_second,));

                    ui.end_row();

                    ui.label(text.wire_savings);

                    ui.strong(format!("{:.2}%", summary.wire_savings_percent,));

                    ui.end_row();

                    if summary.exact_reused_files > 0 {
                        ui.label(text.exact_reuse_files);

                        ui.strong(summary.exact_reused_files.to_string());

                        ui.end_row();

                        ui.label(text.exact_reuse_data);

                        ui.strong(format_bytes(summary.exact_reused_bytes));

                        ui.end_row();

                        ui.label(text.exact_reuse_wire);

                        ui.strong(format_bytes(summary.exact_reuse_plan_wire_bytes));

                        ui.end_row();

                        ui.label(text.exact_reuse_savings);

                        ui.strong(format!("{:.2}%", summary.exact_reuse_wire_savings_percent,));

                        ui.end_row();
                    }

                    if summary.cdc_offered_files > 0 {
                        ui.label(text.cdc_updates);

                        ui.strong(format!(
                            "{} / {}",
                            summary.cdc_files, summary.cdc_offered_files,
                        ));

                        ui.end_row();

                        ui.label(text.cdc_fallbacks);

                        ui.strong(summary.cdc_fallback_files.to_string());

                        ui.end_row();

                        ui.label(text.cdc_data);

                        ui.strong(format!(
                            "{} / {} / {}",
                            format_bytes(summary.cdc_logical_bytes,),
                            format_bytes(summary.cdc_reused_bytes,),
                            format_bytes(summary.cdc_literal_bytes,),
                        ));

                        ui.end_row();

                        ui.label(text.cdc_wire);

                        ui.strong(format!(
                            "{} / {}",
                            format_bytes(summary.cdc_index_wire_bytes,),
                            format_bytes(summary.cdc_plan_wire_bytes,),
                        ));

                        ui.end_row();

                        ui.label(text.cdc_savings);

                        ui.strong(format!("{:.2}%", summary.cdc_wire_savings_percent,));

                        ui.end_row();
                    }

                    ui.label(text.compression_strategy);

                    ui.strong(text.compression_strategy_adaptive);

                    ui.end_row();

                    ui.label(text.transfer_diagnostic);

                    let diagnostic = match summary.diagnostic() {
                        GuiTransferDiagnostic::AllFilesSkipped => text.diagnostic_all_skipped,

                        GuiTransferDiagnostic::TinyFileHeavy => text.diagnostic_tiny_files,

                        GuiTransferDiagnostic::ExactReuseEffective => text.diagnostic_exact_reuse,

                        GuiTransferDiagnostic::CdcEffective => text.diagnostic_cdc_effective,

                        GuiTransferDiagnostic::CompressionEffective => {
                            text.diagnostic_compression_effective
                        }

                        GuiTransferDiagnostic::CompressionBypassed => {
                            text.diagnostic_compression_bypassed
                        }

                        GuiTransferDiagnostic::Balanced => text.diagnostic_balanced,
                    };

                    ui.strong(diagnostic);

                    ui.end_row();

                    if summary.tiny_pack_count > 0 {
                        ui.label(text.tiny_packs);

                        ui.strong(summary.tiny_pack_count.to_string());

                        ui.end_row();

                        ui.label(text.compressed_tiny_packs);

                        ui.strong(summary.compressed_tiny_pack_count.to_string());

                        ui.end_row();

                        ui.label(text.raw_tiny_packs);

                        ui.strong(summary.raw_tiny_pack_count.to_string());

                        ui.end_row();

                        ui.label(text.packed_tiny_files);

                        ui.strong(summary.tiny_files_packed.to_string());

                        ui.end_row();

                        ui.label(text.tiny_write_workers);

                        ui.strong(summary.tiny_materialization_workers.to_string());

                        ui.end_row();

                        ui.label(text.tiny_pack_data);

                        ui.strong(format!(
                            "{} / {}",
                            format_bytes(summary.tiny_bytes_packed),
                            format_bytes(summary.tiny_pack_wire_bytes),
                        ));

                        ui.end_row();

                        ui.label(text.tiny_pack_savings);

                        ui.strong(format!("{:.2}%", summary.tiny_pack_wire_savings_percent,));

                        ui.end_row();
                    }

                    ui.label(text.streams);

                    ui.strong(summary.data_stream_count.to_string());

                    ui.end_row();

                    ui.label(text.elapsed);

                    ui.strong(format!("{:.2} s", summary.elapsed.as_secs_f64(),));

                    ui.end_row();
                });
        });
    }

    fn live_progress_panel(&self, ui: &mut egui::Ui, text: Text) {
        let Some(progress) = &self.live_progress else {
            return;
        };

        let phase = localized_phase(self.language, &progress.phase);

        card(ui, brand_blue(), |ui| {
            ui.horizontal(|ui| {
                ui.spinner();

                ui.heading(if progress.cancel_requested {
                    text.cancelling
                } else {
                    text.running
                });
            });

            ui.add_space(8.0);

            if !progress.cancel_requested {
                ui.label(egui::RichText::new(phase).color(muted_text()));
            }

            ui.add_space(10.0);

            if progress.total > 0 {
                let amount = format!(
                    "{} / {}",
                    format_bytes(progress.completed.min(progress.total,),),
                    format_bytes(progress.total,),
                );

                ui.add(
                    egui::ProgressBar::new(progress.fraction())
                        .show_percentage()
                        .text(amount)
                        .animate(true),
                );

                ui.add_space(8.0);

                ui.label(
                    egui::RichText::new(format!(
                        "{}: {:.2} MB/s",
                        text.speed,
                        progress.megabytes_per_second(),
                    ))
                    .strong(),
                );
            } else if progress.completed > 0 {
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

        ui.horizontal_wrapped(|ui| {
            let start_button = egui::Button::new(
                egui::RichText::new(text.start)
                    .strong()
                    .size(17.0)
                    .color(egui::Color32::WHITE),
            )
            .fill(brand_blue())
            .stroke(egui::Stroke::new(1.0, brand_blue_light()))
            .corner_radius(egui::CornerRadius::same(8))
            .min_size(egui::vec2(160.0, 44.0));

            let start = ui.add_enabled(!running, start_button);

            if start.clicked() {
                self.start_transfer(text);

                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }

            let cancel_button =
                egui::Button::new(egui::RichText::new(text.cancel).strong().size(16.0))
                    .fill(if running {
                        danger_fill()
                    } else {
                        inactive_fill()
                    })
                    .corner_radius(egui::CornerRadius::same(8))
                    .min_size(egui::vec2(150.0, 44.0));

            let cancel = ui.add_enabled(running && !cancel_requested, cancel_button);

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
            ui.add_space(14.0);

            self.live_progress_panel(ui, text);
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui, text: Text) {
        if let Some(warning) = &self.session_warning {
            ui.label(egui::RichText::new(warning).italics().color(warning_text()));

            ui.add_space(10.0);
        }

        let can_resume = self.pending_session.is_some() && self.transfer_receiver.is_none();

        if let Some(error) = self.last_error.clone() {
            let mut resume = false;

            card(ui, danger_text(), |ui| {
                ui.heading(egui::RichText::new(text.failed).color(danger_text()));

                ui.add_space(6.0);

                ui.label(error);

                if can_resume {
                    ui.add_space(12.0);

                    resume = ui.add(resume_button(text.resume_continue)).clicked();
                }
            });

            if resume {
                self.resume_pending(text);
            }
        } else if self.last_cancelled {
            let mut resume = false;

            card(ui, warning_text(), |ui| {
                ui.heading(egui::RichText::new(text.cancelled).color(warning_text()));

                ui.add_space(6.0);

                ui.label(text.cancelled_detail);

                if can_resume {
                    ui.add_space(12.0);

                    resume = ui.add(resume_button(text.resume_continue)).clicked();
                }
            });

            if resume {
                self.resume_pending(text);
            }
        } else if self.last_summary.is_some() {
            self.summary_panel(ui, text);
        } else {
            card(ui, brand_blue(), |ui| {
                ui.label(
                    egui::RichText::new(text.development_status)
                        .strong()
                        .color(brand_blue_light()),
                );

                ui.add_space(4.0);

                ui.label(egui::RichText::new(text.engine_pending).color(muted_text()));
            });
        }
    }
}

impl eframe::App for NetworkCopyGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let text = self.language.text();

        if self.auto_start_pending && self.transfer_receiver.is_none() {
            self.auto_start_pending = false;

            self.resume_pending(text);
        }

        self.poll_transfer(ui.ctx(), text);

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());

                    ui.horizontal(|ui| {
                        brand_mark(ui, 58.0);

                        ui.add_space(8.0);

                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(APP_NAME).strong().size(27.0));

                            ui.label(egui::RichText::new(text.subtitle).color(muted_text()));
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.selectable_value(&mut self.language, Language::English, "English");

                            ui.selectable_value(&mut self.language, Language::Hungarian, "Magyar");

                            ui.label(text.language);
                        });
                    });

                    ui.add_space(16.0);

                    ui.separator();

                    ui.add_space(14.0);

                    self.mode_selector(ui, text);

                    ui.add_space(14.0);

                    card(ui, brand_blue(), |ui| {
                        ui.heading(text.direct_mode);

                        ui.add_space(8.0);

                        self.connection_selector(ui, text);

                        ui.add_space(12.0);

                        ui.separator();

                        ui.add_space(12.0);

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

                    self.status_panel(ui, text);

                    ui.add_space(16.0);
                });
        });

        if self.close_requested {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);

            return;
        }

        self.resume_prompt(ui.ctx(), text);
    }
}

fn configure_style(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);

    context.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);

        style.spacing.button_padding = egui::vec2(14.0, 8.0);

        style.spacing.slider_width = 180.0;

        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(23.0));

        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));

        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
    });

    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = egui::Color32::from_rgb(10, 17, 27);

    visuals.window_fill = card_fill();

    visuals.extreme_bg_color = egui::Color32::from_rgb(5, 10, 17);

    visuals.selection.bg_fill = brand_blue();

    visuals.hyperlink_color = brand_blue_light();

    context.set_visuals_of(egui::Theme::Dark, visuals);
}

fn card(ui: &mut egui::Ui, accent: egui::Color32, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(card_fill())
        .stroke(egui::Stroke::new(1.0, accent))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            add_contents(ui);
        });
}

fn brand_mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());

    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, egui::CornerRadius::same(13), card_fill());

    let center = rect.center();

    let left = egui::pos2(rect.left() + 14.0, center.y);

    let right = egui::pos2(rect.right() - 14.0, center.y);

    painter.circle_filled(left, 5.0, brand_blue_light());

    painter.circle_filled(right, 5.0, brand_green());

    painter.arrow(
        egui::pos2(left.x + 5.0, center.y - 8.0),
        egui::vec2(right.x - left.x - 10.0, 0.0),
        egui::Stroke::new(4.0, brand_green()),
    );

    painter.arrow(
        egui::pos2(right.x - 5.0, center.y + 8.0),
        egui::vec2(left.x - right.x + 10.0, 0.0),
        egui::Stroke::new(4.0, brand_blue_light()),
    );
}

fn resume_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(text)
            .strong()
            .color(egui::Color32::WHITE),
    )
    .fill(brand_green_dark())
    .stroke(egui::Stroke::new(1.0, brand_green()))
    .corner_radius(egui::CornerRadius::same(8))
    .min_size(egui::vec2(150.0, 40.0))
}

fn card_fill() -> egui::Color32 {
    egui::Color32::from_rgb(16, 27, 42)
}

fn brand_blue() -> egui::Color32 {
    egui::Color32::from_rgb(0, 128, 194)
}

fn brand_blue_light() -> egui::Color32 {
    egui::Color32::from_rgb(0, 198, 255)
}

fn brand_green() -> egui::Color32 {
    egui::Color32::from_rgb(126, 230, 64)
}

fn brand_green_dark() -> egui::Color32 {
    egui::Color32::from_rgb(55, 142, 44)
}

fn muted_text() -> egui::Color32 {
    egui::Color32::from_rgb(165, 181, 196)
}

fn inactive_fill() -> egui::Color32 {
    egui::Color32::from_rgb(47, 54, 63)
}

fn danger_fill() -> egui::Color32 {
    egui::Color32::from_rgb(120, 48, 53)
}

fn danger_text() -> egui::Color32 {
    egui::Color32::from_rgb(255, 112, 120)
}

fn warning_text() -> egui::Color32 {
    egui::Color32::from_rgb(255, 190, 82)
}

fn should_rearm_root_receiver(request: &GuiTransferRequest) -> bool {
    matches!(
        request,
        GuiTransferRequest::Receive {
            destination_layout: DestinationLayout::SourceNameUnderRoot,
            ..
        }
    )
}

fn resume_request_summary(ui: &mut egui::Ui, text: Text, request: &GuiTransferRequest) {
    let (direction, folder, connection, send_options, update_existing, destination_layout) =
        match request {
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
                None,
                None,
            ),

            GuiTransferRequest::Receive {
                connection,
                destination_root,
                destination_layout,
                update_existing,
            } => (
                text.receive,
                destination_root,
                *connection,
                None,
                Some(*update_existing),
                Some(*destination_layout),
            ),
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

            if let Some(layout) = destination_layout {
                ui.label(text.resume_destination_layout);

                ui.label(match layout {
                    DestinationLayout::Exact => text.exact_destination,

                    DestinationLayout::SourceNameUnderRoot => text.destination_root_layout,
                });

                ui.end_row();
            }

            if let Some(update_existing) = update_existing {
                ui.label(text.resume_update_mode);

                ui.label(if update_existing {
                    text.enabled
                } else {
                    text.disabled
                });

                ui.end_row();
            }

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

        "Finding exact duplicates" => "Azonos fájlok keresése".to_string(),

        "Waiting for transfer" => "Várakozás az átvitelre".to_string(),

        "Transfer send" => "Fájlok küldése".to_string(),

        "Transfer receive" => "Fájlok fogadása".to_string(),

        "Complete" => "Kész".to_string(),

        "Waiting for receiver finalization" => "Várakozás a fogadó lezárására".to_string(),

        "Finalizing destination" => "Célmappa véglegesítése".to_string(),

        _ => {
            if let Some(source_name) = phase.strip_prefix("Receiving source folder ") {
                return format!("Forrásmappa fogadása — {source_name}",);
            }

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
    use super::{Language, Text, localized_phase, should_rearm_root_receiver};
    use networkcopy_speed::destination_layout::DestinationLayout;
    use networkcopy_speed::gui_transfer::{GuiConnectionMode, GuiTransferRequest};
    use std::path::PathBuf;

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

    #[test]
    fn root_receive_request_rearms() {
        let request = GuiTransferRequest::Receive {
            connection: GuiConnectionMode::Direct,

            destination_root: PathBuf::from(r"D:\Backup"),

            destination_layout: DestinationLayout::SourceNameUnderRoot,

            update_existing: true,
        };

        assert!(should_rearm_root_receiver(&request,),);
    }

    #[test]
    fn exact_receive_request_is_one_shot() {
        let request = GuiTransferRequest::Receive {
            connection: GuiConnectionMode::Direct,

            destination_root: PathBuf::from(r"D:\Exact"),

            destination_layout: DestinationLayout::Exact,

            update_existing: false,
        };

        assert!(!should_rearm_root_receiver(&request,),);
    }

    #[test]
    fn send_request_never_rearms_receiver() {
        let request = GuiTransferRequest::Send {
            connection: GuiConnectionMode::Direct,

            source_root: PathBuf::from(r"C:\Desktop"),

            worker_count: 4,

            calibration_mib: 64,
        };

        assert!(!should_rearm_root_receiver(&request,),);
    }
}
