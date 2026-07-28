#![cfg_attr(
    all(target_os = "windows", not(debug_assertions),),
    windows_subsystem = "windows"
)]

use eframe::egui;
use rfd::FileDialog;

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

#[derive(Clone, Copy)]
struct Text {
    subtitle: &'static str,
    language: &'static str,
    send: &'static str,
    receive: &'static str,
    direct_mode: &'static str,
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
    development_status: &'static str,
    engine_pending: &'static str,
}

impl Text {
    const HUNGARIAN: Self = Self {
        subtitle: "Gyors fájlmásolás közvetlen hálózati kapcsolaton",

        language: "Nyelv:",

        send: "Küldés",

        receive: "Fogadás",

        direct_mode: "Automatikus közvetlen kapcsolat",

        send_description: "Válassza ki a küldendő mappát. A program automatikusan megkeresi a másik számítógépet, megméri a kapcsolat sebességét, majd elindítja a másolást.",

        receive_description: "Válassza ki a célmappát, majd indítsa el a fogadást. A program automatikusan beállítja a Windows tűzfalat és megvárja a küldő számítógépet.",

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

        development_status: "v1.2 fejlesztői felület",

        engine_pending: "A mappaválasztás és a nyelvváltás már működik. A másolómotor bekötése a következő lépés.",
    };

    const ENGLISH: Self = Self {
        subtitle: "High-speed copying over a direct network connection",

        language: "Language:",

        send: "Send",

        receive: "Receive",

        direct_mode: "Automatic direct connection",

        send_description: "Choose the folder to send. NetworkCopy will automatically find the other computer, measure the connection speed, and start the transfer.",

        receive_description: "Choose the destination folder and start receiving. NetworkCopy will configure Windows Firewall automatically and wait for the sending computer.",

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

        development_status: "v1.2 development interface",

        engine_pending: "Folder selection and language switching are ready. Connecting the transfer engine is the next step.",
    };
}

struct NetworkCopyGui {
    language: Language,
    mode: TransferMode,
    source_folder: String,
    destination_folder: String,
    scanner_workers: usize,
    calibration_mib: u64,
}

impl NetworkCopyGui {
    fn new() -> Self {
        Self {
            language: Language::initial(),

            mode: TransferMode::Send,

            source_folder: String::new(),

            destination_folder: String::new(),

            scanner_workers: 4,
            calibration_mib: 64,
        }
    }

    fn mode_selector(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, TransferMode::Send, text.send);

            ui.selectable_value(&mut self.mode, TransferMode::Receive, text.receive);
        });
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

    fn action_buttons(&mut self, ui: &mut egui::Ui, text: Text) {
        ui.horizontal(|ui| {
            ui.add_enabled(
                false,
                egui::Button::new(text.start).min_size(egui::vec2(120.0, 34.0)),
            );

            ui.add_enabled(
                false,
                egui::Button::new(text.cancel).min_size(egui::vec2(120.0, 34.0)),
            );
        });
    }
}

impl eframe::App for NetworkCopyGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let text = self.language.text();

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

            ui.label(egui::RichText::new(text.development_status).strong());

            ui.label(text.engine_pending);
        });
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

#[cfg(test)]
mod tests {
    use super::{Language, Text};

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
}
