use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use sp404sx_wave_converter::Card;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub fn run(initial_root: Option<String>) -> Result<(), String> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
        .map_err(|error| format!("could not decode application icon: {error}"))?;
    let skin = skin_image()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("sp404sx-wave-converter")
            .with_icon(icon)
            .with_inner_size([310.0, 370.0])
            .with_min_inner_size([310.0, 370.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SP-404SX Wave Converter",
        options,
        Box::new(|cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.extreme_bg_color = Color32::TRANSPARENT;
            for widget in [
                &mut visuals.widgets.noninteractive,
                &mut visuals.widgets.inactive,
                &mut visuals.widgets.hovered,
                &mut visuals.widgets.active,
                &mut visuals.widgets.open,
            ] {
                widget.bg_fill = Color32::TRANSPARENT;
                widget.weak_bg_fill = Color32::TRANSPARENT;
            }
            cc.egui_ctx.set_visuals(visuals);
            cc.egui_ctx.style_mut(|style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, FontId::proportional(12.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, FontId::proportional(12.0));
            });
            let skin = cc
                .egui_ctx
                .load_texture("background skin", skin, skin_texture_options());
            Ok(Box::new(App::new(initial_root, skin, settings_path())))
        }),
    )
    .map_err(|error| format!("could not start GUI: {error}"))
}

fn skin_image() -> Result<egui::ColorImage, String> {
    let image = eframe::icon_data::from_png_bytes(include_bytes!("../bg.png"))
        .map_err(|error| format!("could not decode background skin: {error}"))?;
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [image.width as usize, image.height as usize],
        &image.rgba,
    ))
}

fn skin_texture_options() -> egui::TextureOptions {
    egui::TextureOptions {
        mipmap_mode: Some(egui::TextureFilter::Linear),
        ..egui::TextureOptions::LINEAR
    }
}

fn skin_rect(window: Rect, image_size: Vec2) -> Rect {
    let scale = (window.width() / image_size.x).min(window.height() / image_size.y);
    let size = image_size * scale;
    Rect::from_min_size(
        Pos2::new(window.center().x - size.x / 2.0, window.top()),
        size,
    )
}

fn skin_fill_rect(window: Rect, image_size: Vec2) -> Rect {
    let scale = (window.width() / image_size.x).max(window.height() / image_size.y);
    Rect::from_center_size(window.center(), image_size * scale)
}

fn header_bottom(skin: Rect) -> f32 {
    skin.top() + skin.height() * (250.0 / 1367.0)
}

fn settings_path() -> Result<PathBuf, String> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or("No absolute XDG_CONFIG_HOME or HOME directory is available")?;
    Ok(config_dir
        .join("sp404sx-wave-converter")
        .join("last-card-folder"))
}

fn read_last_card(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(root) if root.is_empty() => Err("The remembered card folder is empty".into()),
        Ok(root) => Ok(Some(root)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read remembered card folder: {error}")),
    }
}

fn save_last_card(path: &Path, root: &str) -> Result<(), String> {
    let root = std::path::absolute(root)
        .map_err(|error| format!("Could not resolve card folder: {error}"))?;
    let root = root
        .to_str()
        .ok_or("The absolute card folder path is not valid UTF-8")?;
    let dir = path.parent().ok_or("Invalid settings path")?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("Could not create settings directory: {error}"))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .map_err(|error| format!("Could not create settings file: {error}"))?;
    file.write_all(root.as_bytes())
        .map_err(|error| format!("Could not write settings file: {error}"))?;
    file.persist(path)
        .map_err(|error| format!("Could not save settings file: {}", error.error))?;
    Ok(())
}

struct App {
    root: String,
    card: Option<Card>,
    bank: char,
    selected: Option<u8>,
    player: Option<Child>,
    playing: Option<(char, u8)>,
    message: String,
    skin: egui::TextureHandle,
    settings_path: Result<PathBuf, String>,
}

impl App {
    fn new(
        initial_root: Option<String>,
        skin: egui::TextureHandle,
        settings_path: Result<PathBuf, String>,
    ) -> Self {
        let explicit_root = initial_root.is_some();
        let (root, message) = if let Some(root) = initial_root {
            (root, String::new())
        } else {
            match settings_path.as_ref() {
                Ok(path) => match read_last_card(path) {
                    Ok(root) => (root.unwrap_or_default(), String::new()),
                    Err(error) => (String::new(), error),
                },
                Err(error) => (String::new(), error.clone()),
            }
        };
        let mut app = Self {
            root,
            card: None,
            bank: 'A',
            selected: None,
            player: None,
            playing: None,
            message,
            skin,
            settings_path,
        };
        if !app.root.is_empty() {
            app.open_card(explicit_root);
        }
        app
    }

    fn open_card(&mut self, remember: bool) {
        self.selected = None;
        match Card::open(&self.root) {
            Ok(card) => {
                self.card = Some(card);
                self.message = "Card opened.".into();
                if remember {
                    let result = match &self.settings_path {
                        Ok(path) => save_last_card(path, &self.root),
                        Err(error) => Err(error.clone()),
                    };
                    if let Err(error) = result {
                        self.message
                            .push_str(&format!(" Could not remember card folder: {error}"));
                    }
                }
            }
            Err(error) => {
                self.card = None;
                self.message = error.to_string();
            }
        }
    }

    fn open_selected_card(&mut self, path: &Path) {
        let Some(path) = path.to_str() else {
            self.message = "The selected folder path is not valid UTF-8.".into();
            return;
        };
        self.root = path.to_owned();
        self.stop();
        self.open_card(true);
    }

    fn browse_card(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Select SP-404SX card folder");
        if Path::new(&self.root).is_dir() {
            dialog = dialog.set_directory(&self.root);
        }
        if let Some(path) = dialog.pick_folder() {
            self.open_selected_card(&path);
        }
    }

    fn stop(&mut self) -> bool {
        if let Some(mut player) = self.player.take() {
            match player.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    self.message = format!("Playback failed: {status}");
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = player.kill().and_then(|_| player.wait().map(|_| ())) {
                        self.message = format!("Could not stop playback: {error}");
                        self.player = Some(player);
                        return false;
                    }
                }
                Err(error) => {
                    self.message = format!("Could not check playback: {error}");
                    self.player = Some(player);
                    return false;
                }
            }
        }
        self.playing = None;
        true
    }

    fn play(&mut self, number: u8) {
        if !self.stop() {
            return;
        }
        let Some(pad) = self
            .card
            .as_ref()
            .and_then(|card| card.pad(self.bank, number))
        else {
            return;
        };
        let Some(file) = pad.file.as_ref() else {
            return;
        };
        let Some(size) = pad.file_size else {
            return;
        };
        let alignment = u32::from(pad.channels) * 2;
        if !matches!(pad.channels, 1 | 2)
            || pad.start < 512
            || pad.end <= pad.start
            || u64::from(pad.end) > size
            || (pad.start - 512) % alignment != 0
            || (pad.end - 512) % alignment != 0
        {
            self.message = "Invalid sample trim or channel settings.".into();
            return;
        }
        let bytes_per_second = 44100.0 * f64::from(alignment);
        let start = format!("{:.9}", f64::from(pad.start - 512) / bytes_per_second);
        let duration = format!("{:.9}", f64::from(pad.end - pad.start) / bytes_per_second);
        match Command::new("ffplay")
            .args([
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "error",
                "-ss",
                &start,
                "-t",
                &duration,
            ])
            .arg(file)
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.player = Some(child);
                self.playing = Some((self.bank, number));
                self.message = format!("Previewing {}{number}.", self.bank);
            }
            Err(error) => self.message = format!("Could not start ffplay: {error}"),
        }
    }

    fn confirm(title: &str, description: &str) -> bool {
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(description)
            .set_buttons(rfd::MessageButtons::YesNo)
            .set_level(rfd::MessageLevel::Warning)
            .show()
            == rfd::MessageDialogResult::Yes
    }

    fn import(&mut self, number: u8) {
        let Some(card) = self.card.as_ref() else {
            self.message = "Open a formatted card first.".into();
            return;
        };
        let assigned = card
            .pad(self.bank, number)
            .is_some_and(|pad| pad.file.is_some());
        let Some(path) = rfd::FileDialog::new()
            .set_title("Select a WAV or AIFF sample")
            .add_filter("WAV and AIFF audio", &["wav", "aif", "aiff"])
            .pick_file()
        else {
            return;
        };
        if assigned
            && !Self::confirm(
                "Replace sample?",
                &format!("Replace the sample on {}{number}?", self.bank),
            )
        {
            return;
        }
        if !self.stop() {
            return;
        }
        let Some(card) = self.card.as_mut() else {
            return;
        };
        self.message = match card.import_sample(self.bank, number, &path) {
            Ok(()) => format!("Imported sample to {}{number}.", self.bank),
            Err(error) => format!("Import failed: {error}"),
        };
        self.refresh_after_change();
    }

    fn delete(&mut self, number: u8) {
        if !Self::confirm(
            "Delete sample?",
            &format!("Permanently delete the sample on {}{number}?", self.bank),
        ) {
            return;
        }
        if !self.stop() {
            return;
        }
        let Some(card) = self.card.as_mut() else {
            self.message = "Open a formatted card first.".into();
            return;
        };
        self.message = match card.delete_sample(self.bank, number) {
            Ok(()) => format!("Deleted sample from {}{number}.", self.bank),
            Err(error) => format!("Delete failed: {error}"),
        };
        self.refresh_after_change();
    }

    fn refresh_after_change(&mut self) {
        match Card::open(&self.root) {
            Ok(card) => self.card = Some(card),
            Err(error) => {
                self.card = None;
                self.message
                    .push_str(&format!(" Card refresh failed: {error}"));
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

fn paint_pad(ui: &egui::Ui, rect: Rect, number: u8, assigned: bool, selected: bool) {
    let painter = ui.painter();
    if assigned {
        painter.rect_filled(rect, 1.0, Color32::from_rgba_unmultiplied(230, 30, 15, 120));
    }
    painter.rect_stroke(
        rect,
        1.0,
        Stroke::new(1.0, Color32::WHITE),
        StrokeKind::Inside,
    );
    if selected {
        painter.rect_stroke(
            rect.expand(1.0),
            1.0,
            Stroke::new(2.0, Color32::from_rgb(25, 170, 45)),
            StrokeKind::Outside,
        );
    }
    let position = rect.center() + Vec2::new(0.0, -0.5);
    let font = FontId::monospace(19.0);
    for offset in [Vec2::ZERO, Vec2::new(0.6, 0.0), Vec2::new(0.0, 0.5)] {
        painter.text(
            position + offset,
            Align2::CENTER_CENTER,
            number.to_string(),
            font.clone(),
            Color32::WHITE,
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(player) = self.player.as_mut() {
            match player.try_wait() {
                Ok(Some(status)) => {
                    self.player = None;
                    self.playing = None;
                    if !status.success() {
                        self.message = format!("Playback failed: {status}");
                    }
                }
                Ok(None) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
                Err(error) => self.message = format!("Could not check playback: {error}"),
            }
        }
        let window = ctx.input(|input| input.screen_rect());
        let skin = skin_rect(window, self.skin.size_vec2());
        ctx.layer_painter(egui::LayerId::background())
            .with_clip_rect(window)
            .image(
                self.skin.id(),
                skin_fill_rect(window, self.skin.size_vec2()),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        ctx.layer_painter(egui::LayerId::background())
            .with_clip_rect(window)
            .image(
                self.skin.id(),
                skin,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            Rect::from_min_max(Pos2::new(window.left(), header_bottom(skin)), window.max),
            0.0,
            Color32::from_black_alpha(150),
        );
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(11, 10)))
            .show(ctx, |ui| {
                ui.add_space((header_bottom(skin) - ui.min_rect().top()).max(0.0) + 6.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label("SP-404SX Card Drive:");
                                ui.horizontal(|ui| {
                                    let input = ui.add(
                                        egui::TextEdit::singleline(&mut self.root)
                                            .desired_width(102.0)
                                            .hint_text("Card root"),
                                    );
                                    if ui
                                        .small_button("...")
                                        .on_hover_text("Browse for the card folder")
                                        .clicked()
                                    {
                                        self.browse_card();
                                    }
                                    let open = ui
                                        .small_button("↻")
                                        .on_hover_text("Open or refresh the card");
                                    if open.clicked()
                                        || (input.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                    {
                                        self.stop();
                                        self.open_card(true);
                                    }
                                });
                            });
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                ui.label("SP-404SX Bank");
                                let previous = self.bank;
                                egui::ComboBox::from_id_salt("bank")
                                    .selected_text(self.bank.to_string())
                                    .width(68.0)
                                    .show_ui(ui, |ui| {
                                        for bank in 'A'..='J' {
                                            ui.selectable_value(
                                                &mut self.bank,
                                                bank,
                                                bank.to_string(),
                                            );
                                        }
                                    });
                                if self.bank != previous {
                                    self.selected = None;
                                }
                            });
                        });
                        ui.add_space(14.0);
                        ui.vertical_centered(|ui| {
                            let (panel, _) =
                                ui.allocate_exact_size(Vec2::new(224.0, 150.0), Sense::hover());
                            for row in 0..3 {
                                for column in 0..4 {
                                    let number = (row * 4 + column + 1) as u8;
                                    let pos = panel.min
                                        + Vec2::new(
                                            6.0 + column as f32 * 56.0,
                                            6.0 + row as f32 * 50.0,
                                        );
                                    let rect = Rect::from_min_size(pos, Vec2::new(44.0, 38.0));
                                    let assigned = self
                                        .card
                                        .as_ref()
                                        .and_then(|card| card.pad(self.bank, number))
                                        .is_some_and(|pad| pad.file.is_some());
                                    let response = ui.interact(
                                        rect,
                                        ui.id().with(("pad", number)),
                                        Sense::click(),
                                    );
                                    paint_pad(
                                        ui,
                                        rect,
                                        number,
                                        assigned,
                                        self.selected == Some(number),
                                    );
                                    if response.clicked() {
                                        self.selected = Some(number);
                                        if assigned && response.double_clicked() {
                                            self.play(number);
                                        } else if assigned
                                            && self.playing == Some((self.bank, number))
                                        {
                                            self.stop();
                                        } else if !assigned {
                                            self.import(number);
                                        }
                                    }
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    self.card.is_some() && self.selected.is_some(),
                                    egui::Button::new("Import").min_size(Vec2::new(75.0, 23.0)),
                                )
                                .clicked()
                            {
                                if let Some(number) = self.selected {
                                    self.import(number);
                                }
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let can_delete = self.selected.is_some_and(|number| {
                                        self.card
                                            .as_ref()
                                            .and_then(|card| card.pad(self.bank, number))
                                            .is_some_and(|pad| pad.file.is_some())
                                    });
                                    if ui
                                        .add_enabled(
                                            can_delete,
                                            egui::Button::new("Delete")
                                                .min_size(Vec2::new(75.0, 23.0)),
                                        )
                                        .clicked()
                                    {
                                        if let Some(number) = self.selected {
                                            self.delete(number);
                                        }
                                    }
                                },
                            );
                        });
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new("Stop").min_size(Vec2::new(75.0, 23.0)))
                                .clicked()
                            {
                                self.stop();
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new("Exit")
                                                .min_size(Vec2::new(75.0, 23.0)),
                                        )
                                        .clicked()
                                    {
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                    }
                                },
                            );
                        });
                        ui.add_space(4.0);
                        ui.label("Click an empty pad to import; double-click to play.");
                        if !self.message.is_empty() {
                            ui.label(&self.message);
                        }
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app(initial_root: Option<String>, settings: &Path) -> App {
        let ctx = egui::Context::default();
        let skin = ctx.load_texture("test skin", skin_image().unwrap(), skin_texture_options());
        App::new(initial_root, skin, Ok(settings.to_owned()))
    }

    fn create_card(root: &Path) {
        let sample_dir = root.join("ROLAND/SP-404SX/SMPL");
        std::fs::create_dir_all(&sample_dir).unwrap();
        std::fs::write(sample_dir.join("PAD_INFO.BIN"), vec![0; 3840]).unwrap();
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap();
        assert_eq!((icon.width, icon.height), (256, 256));
    }

    #[test]
    fn skin_shows_header_without_distortion_or_cropping() {
        let image = skin_image().unwrap();
        let image_size = Vec2::new(image.size[0] as f32, image.size[1] as f32);
        for size in [
            Vec2::new(310.0, 340.0),
            Vec2::new(800.0, 300.0),
            Vec2::new(300.0, 800.0),
        ] {
            let window = Rect::from_min_size(Pos2::new(10.0, 20.0), size);
            let rect = skin_rect(window, image_size);
            assert!(window.contains_rect(rect));
            assert!((rect.width() / rect.height() - image_size.x / image_size.y).abs() < 0.0001);
            assert_eq!(rect.center().x, window.center().x);
            assert_eq!(rect.top(), window.top());
            assert!(header_bottom(rect) > rect.top());
            assert!(header_bottom(rect) < rect.bottom());
            assert!(skin_fill_rect(window, image_size).contains_rect(window));
        }
    }

    #[test]
    fn selecting_a_folder_opens_the_card() {
        let root = tempfile::tempdir().unwrap();
        create_card(root.path());
        let mut app = test_app(None, &root.path().join("settings/last-card"));

        app.open_selected_card(root.path());

        assert_eq!(app.root, root.path().to_str().unwrap());
        assert!(app.card.is_some());
        assert_eq!(app.message, "Card opened.");
    }

    #[test]
    fn selecting_an_invalid_folder_reports_the_error() {
        let root = tempfile::tempdir().unwrap();
        let mut app = test_app(None, &root.path().join("settings/last-card"));

        app.open_selected_card(root.path());

        assert!(app.card.is_none());
        assert!(app.message.contains("PAD_INFO.BIN"));
    }

    #[test]
    fn remembers_successful_selection_and_restores_it() {
        let temp = tempfile::tempdir().unwrap();
        let card = temp.path().join("card");
        let settings = temp.path().join("settings/last-card");
        create_card(&card);
        let mut app = test_app(None, &settings);
        app.open_selected_card(&card);
        drop(app);

        let restored = test_app(None, &settings);
        assert_eq!(restored.root, card.to_str().unwrap());
        assert!(restored.card.is_some());
    }

    #[test]
    fn failed_selection_does_not_replace_last_good_folder() {
        let temp = tempfile::tempdir().unwrap();
        let card = temp.path().join("card");
        let settings = temp.path().join("settings/last-card");
        create_card(&card);
        let mut app = test_app(None, &settings);
        app.open_selected_card(&card);
        app.open_selected_card(&temp.path().join("missing"));

        assert!(app.card.is_none());
        assert!(app.message.contains("PAD_INFO.BIN"));
        let restored = test_app(None, &settings);
        assert_eq!(restored.root, card.to_str().unwrap());
        assert!(restored.card.is_some());
    }

    #[test]
    fn explicit_card_takes_precedence_and_becomes_last_used() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let settings = temp.path().join("settings/last-card");
        create_card(&first);
        create_card(&second);
        test_app(None, &settings).open_selected_card(&first);

        let explicit = test_app(Some(second.to_str().unwrap().to_owned()), &settings);
        assert!(explicit.card.is_some());
        assert_eq!(explicit.root, second.to_str().unwrap());
        let restored = test_app(None, &settings);
        assert_eq!(restored.root, second.to_str().unwrap());
    }

    #[test]
    fn missing_remembered_card_remains_visible_with_error() {
        let temp = tempfile::tempdir().unwrap();
        let card = temp.path().join("missing");
        let settings = temp.path().join("settings/last-card");
        save_last_card(&settings, card.to_str().unwrap()).unwrap();

        let app = test_app(None, &settings);
        assert_eq!(app.root, card.to_str().unwrap());
        assert!(app.card.is_none());
        assert!(app.message.contains("PAD_INFO.BIN"));
        assert_eq!(read_last_card(&settings).unwrap().unwrap(), app.root);
    }

    #[test]
    fn settings_failures_are_visible() {
        let temp = tempfile::tempdir().unwrap();
        let card = temp.path().join("card");
        create_card(&card);
        let mut app = test_app(None, temp.path());
        assert!(app
            .message
            .contains("Could not read remembered card folder"));

        app.open_selected_card(&card);
        assert!(app.card.is_some());
        assert!(app.message.contains("Could not remember card folder"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_folder_without_changing_the_card() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let mut app = test_app(None, &temp.path().join("settings/last-card"));
        app.root = "previous".into();
        let path = std::ffi::OsString::from_vec(vec![b'/', 0xff]);

        app.open_selected_card(Path::new(&path));

        assert_eq!(app.root, "previous");
        assert!(app.message.contains("not valid UTF-8"));
    }
}
