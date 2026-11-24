use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use sp404sx_wave_converter::editor::{BeatGrid, SampleEditor};
use sp404sx_wave_converter::Pad;
use std::ops::Range;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

pub enum Action {
    None,
    Save,
    Close,
}

pub struct EditorView {
    pub sample: SampleEditor,
    pub root: PathBuf,
    pub pad: Pad,
    pub original: tempfile::NamedTempFile,
    selection: Range<usize>,
    anchor: Option<usize>,
    grid: BeatGrid,
    snap: bool,
    zoom: f32,
    view_start: usize,
    peaks: Vec<Vec<(i16, i16)>>,
    player: Option<Child>,
    preview: Option<tempfile::NamedTempFile>,
    loop_preview: bool,
    bars_ruler: bool,
    pending_play: Option<Range<usize>>,
    playhead: Option<(Instant, usize, usize, bool)>,
    ai_job: Option<mpsc::Receiver<Result<BeatGrid, String>>>,
    target_bpm: f64,
    message: String,
}

impl EditorView {
    pub fn open(root: PathBuf, pad: Pad) -> Result<Self, String> {
        let path = pad.file.as_ref().ok_or("Pad is empty")?;
        let mut original = tempfile::NamedTempFile::new()
            .map_err(|e| format!("Could not create sample snapshot: {e}"))?;
        let mut input =
            std::fs::File::open(path).map_err(|e| format!("Could not read sample: {e}"))?;
        std::io::copy(&mut input, original.as_file_mut())
            .map_err(|e| format!("Could not snapshot sample: {e}"))?;
        let mut snapshot_pad = pad.clone();
        snapshot_pad.file = Some(original.path().to_owned());
        let sample = SampleEditor::open(&snapshot_pad).map_err(|e| e.to_string())?;
        let selection = sample.play_range();
        let mut editor = Self {
            sample,
            root,
            pad,
            original,
            selection,
            anchor: None,
            grid: BeatGrid {
                bpm: 120.0,
                offset: 0.0,
                confidence: 0.0,
            },
            snap: false,
            zoom: 1.0,
            view_start: 0,
            peaks: Vec::new(),
            player: None,
            preview: None,
            loop_preview: false,
            bars_ruler: false,
            pending_play: None,
            playhead: None,
            ai_job: None,
            target_bpm: 120.0,
            message: String::new(),
        };
        editor.rebuild_peaks();
        Ok(editor)
    }

    pub fn set_message(&mut self, message: String) {
        self.message = message;
    }

    fn rebuild_peaks(&mut self) {
        let frames = self.sample.frames();
        let channels = usize::from(self.sample.channels);
        let width = frames.div_ceil(8192).max(1);
        self.peaks = vec![Vec::new(); channels];
        for chunk in self.sample.samples().chunks(width * channels) {
            for channel in 0..channels {
                let mut min = i16::MAX;
                let mut max = i16::MIN;
                for frame in chunk.chunks_exact(channels) {
                    min = min.min(frame[channel]);
                    max = max.max(frame[channel]);
                }
                self.peaks[channel].push((min, max));
            }
        }
    }

    fn stop(&mut self) -> Result<(), String> {
        if let Some(mut child) = self.player.take() {
            match child.try_wait() {
                Err(e) => {
                    self.player = Some(child);
                    return Err(format!("Could not check preview: {e}"));
                }
                Ok(status) => match status {
                    None => {
                        if let Err(e) = child.kill().and_then(|_| child.wait()) {
                            self.player = Some(child);
                            return Err(format!("Could not stop preview: {e}"));
                        }
                    }
                    Some(status) if !status.success() => {
                        return Err(format!("Preview failed: {status}"))
                    }
                    Some(_) => {}
                },
            }
        }
        self.preview = None;
        self.playhead = None;
        Ok(())
    }

    fn preview(&mut self) -> Result<(), String> {
        self.stop()?;
        if self.selection.start >= self.selection.end || self.selection.end > self.sample.frames() {
            return Err("Select a non-empty region first".into());
        }
        let file = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile()
            .map_err(|e| format!("Could not create preview: {e}"))?;
        self.sample
            .write_wav_range(file.path(), self.selection.clone())
            .map_err(|e| format!("Could not write preview: {e}"))?;
        let mut command = Command::new("ffplay");
        command.args(["-nodisp", "-autoexit", "-loglevel", "error"]);
        if self.loop_preview {
            command.args(["-loop", "0"]);
        }
        let child = command
            .arg(file.path())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| format!("Could not start ffplay: {e}"))?;
        self.preview = Some(file);
        self.player = Some(child);
        self.playhead = Some((
            Instant::now(),
            self.selection.start,
            self.selection.end,
            self.loop_preview,
        ));
        Ok(())
    }

    fn after_edit(&mut self, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                self.selection = self.sample.play_range();
                self.view_start = 0;
                self.snap = false;
                self.rebuild_peaks();
                self.message =
                    "Edited in memory; check the beat grid before snapping again.".into();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn toggle_preview(&mut self) {
        let result = if self.player.is_some() {
            self.stop()
        } else {
            self.preview()
        };
        if let Err(e) = result {
            self.message = e;
        }
    }

    fn do_undo(&mut self) {
        if self.sample.undo() {
            self.selection = self.sample.play_range();
            self.pending_play = None;
            self.snap = false;
            self.rebuild_peaks();
            self.message = "Undone; check the beat grid before snapping again.".into();
        }
    }

    fn do_redo(&mut self) {
        if self.sample.redo() {
            self.selection = self.sample.play_range();
            self.pending_play = None;
            self.snap = false;
            self.rebuild_peaks();
            self.message = "Redone; check the beat grid before snapping again.".into();
        }
    }

    fn start_ai_detect(&mut self) {
        if self.ai_job.is_some() {
            return;
        }
        let mono = self.sample.mono_f32();
        let rate = self.sample.sample_rate;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = sp404sx_wave_converter::ai::detect_beats(&mono, rate)
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.ai_job = Some(rx);
        self.message = "Analysing beats with beat-this (first use downloads ~83 MB of models to ~/.sp404sx-wave-converter)...".into();
    }

    fn poll_ai_job(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.ai_job else { return };
        match rx.try_recv() {
            Ok(Ok(grid)) => {
                self.grid = grid;
                self.target_bpm = grid.bpm;
                self.snap = true;
                self.ai_job = None;
                self.message = format!(
                    "beat-this: {:.2} BPM, {:.0}% of beats on the grid. Check and adjust as needed.",
                    grid.bpm,
                    grid.confidence * 100.0
                );
            }
            Ok(Err(e)) => {
                self.ai_job = None;
                self.message = format!("Beat analysis failed: {e}");
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(200))
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ai_job = None;
                self.message = "Beat analysis failed: worker stopped".into();
            }
        }
    }

    fn stretch_to_target(&mut self) {
        if self.grid.bpm <= 0.0 || self.target_bpm <= 0.0 {
            self.message = "Set a valid grid BPM and target BPM first".into();
            return;
        }
        if let Err(e) = self.stop() {
            self.message = e;
            return;
        }
        let ratio = self.grid.bpm / self.target_bpm;
        let result = self.sample.time_stretch(0..self.sample.frames(), ratio);
        let ok = result.is_ok();
        self.after_edit(result);
        if ok {
            self.grid.offset *= ratio;
            self.grid.bpm = self.target_bpm;
            self.snap = true;
            self.message = format!(
                "Stretched to {:.2} BPM (x{ratio:.3}) with Signalsmith Stretch; pitch unchanged.",
                self.target_bpm
            );
        }
    }

    fn seconds(&self, frames: usize) -> f64 {
        frames as f64 / f64::from(self.sample.sample_rate)
    }

    fn waveform(&mut self, ui: &mut egui::Ui) {
        const RULER: f32 = 20.0;
        let frames = self.sample.frames();
        let sample_rate = f64::from(self.sample.sample_rate);
        let screen_height = ui.ctx().input(|input| input.screen_rect()).height();
        let height = (screen_height - 250.0).clamp(200.0, 700.0);
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width().max(200.0), height),
            Sense::click_and_drag(),
        );
        let wave_rect = Rect::from_min_max(Pos2::new(rect.left(), rect.top() + RULER), rect.max);

        // Wheel: zoom around the pointer; shift/horizontal wheel: pan.
        if response.hovered() {
            let (scroll, shift, pointer) = ui.input(|input| {
                (
                    input.smooth_scroll_delta,
                    input.modifiers.shift,
                    input.pointer.hover_pos(),
                )
            });
            let visible = ((frames as f32 / self.zoom).ceil() as usize).clamp(1, frames);
            if shift || scroll.x != 0.0 {
                let delta = if scroll.x != 0.0 { scroll.x } else { scroll.y };
                let shift_frames = (delta / rect.width() * visible as f32) as isize;
                self.view_start = (self.view_start as isize - shift_frames)
                    .clamp(0, (frames - visible) as isize)
                    as usize;
            } else if scroll.y != 0.0 {
                let ratio = pointer
                    .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
                    .unwrap_or(0.5);
                let anchor = self.view_start as f32 + ratio * visible as f32;
                self.zoom = (self.zoom * (scroll.y * 0.005).exp()).clamp(1.0, 256.0);
                let new_visible = ((frames as f32 / self.zoom).ceil() as usize).clamp(1, frames);
                self.view_start = (anchor - ratio * new_visible as f32)
                    .max(0.0)
                    .min((frames - new_visible) as f32) as usize;
            }
        }
        let visible = ((frames as f32 / self.zoom).ceil() as usize).clamp(1, frames);
        self.view_start = self.view_start.min(frames - visible);
        let view_start = self.view_start;
        let snap = self.snap;
        let grid = self.grid;
        let sample_rate_u32 = self.sample.sample_rate;

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(27, 32, 49));
        let x = |frame: usize| {
            rect.left() + (frame.saturating_sub(view_start) as f32 / visible as f32) * rect.width()
        };
        let in_view = |frame: usize| frame >= view_start && frame <= view_start + visible;
        let frame_at = |pos: Pos2| {
            let ratio = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let frame = (view_start + (ratio * visible as f32).round() as usize).min(frames);
            if snap {
                grid.snap(frame, sample_rate_u32, frames)
            } else {
                frame
            }
        };

        // Time ruler.
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(rect.width(), RULER)),
            0.0,
            Color32::from_rgb(20, 24, 38),
        );
        if self.bars_ruler && grid.bpm > 0.0 {
            // Bars and quarter notes (4/4) from the beat grid's BPM and offset.
            let beat = 60.0 / grid.bpm * sample_rate;
            let offset = grid.offset * sample_rate;
            let px_per_beat = (beat / visible as f64 * f64::from(rect.width())) as f32;
            let bar_step = [1i64, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024]
                .into_iter()
                .find(|s| px_per_beat * 4.0 * *s as f32 >= 48.0)
                .unwrap_or(1024);
            let first = ((view_start as f64 - offset) / beat).ceil() as i64;
            let last = (((view_start + visible) as f64 - offset) / beat).floor() as i64;
            for n in first..=last {
                let bar = n.div_euclid(4);
                let quarter = n.rem_euclid(4);
                let is_bar = quarter == 0;
                if (is_bar && bar % bar_step != 0) || (!is_bar && px_per_beat < 6.0) {
                    continue;
                }
                let frame = (offset + n as f64 * beat).round().max(0.0) as usize;
                let tx = x(frame);
                let (tick, color) = if is_bar {
                    (RULER - 2.0, Color32::from_gray(190))
                } else {
                    (6.0, Color32::from_gray(120))
                };
                painter.line_segment(
                    [
                        Pos2::new(tx, rect.top() + RULER - tick),
                        Pos2::new(tx, rect.top() + RULER),
                    ],
                    Stroke::new(1.0, color),
                );
                if is_bar && px_per_beat * 4.0 >= 20.0 {
                    painter.line_segment(
                        [Pos2::new(tx, rect.top() + RULER), Pos2::new(tx, rect.bottom())],
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 22)),
                    );
                }
                let label = if is_bar {
                    Some(format!("{}", bar + 1))
                } else if px_per_beat >= 44.0 {
                    Some(format!("{}.{}", bar + 1, quarter + 1))
                } else {
                    None
                };
                if let Some(label) = label {
                    painter.text(
                        Pos2::new(tx + 3.0, rect.top() + 2.0),
                        egui::Align2::LEFT_TOP,
                        label,
                        egui::FontId::proportional(10.0),
                        Color32::from_gray(if is_bar { 210 } else { 150 }),
                    );
                }
            }
        } else {
        let seconds_visible = visible as f64 / sample_rate;
        let min_step = seconds_visible * 70.0 / f64::from(rect.width());
        let step = [
            0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0,
        ]
        .into_iter()
        .find(|s| *s >= min_step)
        .unwrap_or(60.0);
        let first_tick = ((view_start as f64 / sample_rate) / step).ceil() as i64;
        let last_tick = (((view_start + visible) as f64 / sample_rate) / step).floor() as i64;
        for tick in first_tick..=last_tick {
            let seconds = tick as f64 * step;
            let tx = x((seconds * sample_rate).round() as usize);
            painter.line_segment(
                [
                    Pos2::new(tx, rect.top() + RULER - 6.0),
                    Pos2::new(tx, rect.top() + RULER),
                ],
                Stroke::new(1.0, Color32::from_gray(140)),
            );
            let label = if step < 1.0 {
                format!("{:.0} ms", seconds * 1000.0)
            } else {
                format!("{seconds:.1} s")
            };
            painter.text(
                Pos2::new(tx + 3.0, rect.top() + 2.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(10.0),
                Color32::from_gray(170),
            );
        }
        }

        let selected_start = self
            .selection
            .start
            .max(view_start)
            .min(view_start + visible);
        let selected_end = self.selection.end.max(view_start).min(view_start + visible);
        if selected_start < selected_end {
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(x(selected_start), wave_rect.top()),
                    Pos2::new(x(selected_end), wave_rect.bottom()),
                ),
                0.0,
                Color32::from_rgba_unmultiplied(88, 117, 195, 75),
            );
        }
        if self.snap && self.grid.bpm > 0.0 {
            let period = 60.0 * sample_rate / self.grid.bpm;
            let offset = self.grid.offset * sample_rate;
            let first = ((view_start as f64 - offset) / period).ceil() as i64;
            let last = (((view_start + visible) as f64 - offset) / period).floor() as i64;
            if last - first < 500 {
                for beat in first..=last {
                    let frame = (offset + beat as f64 * period).round().max(0.0) as usize;
                    let bx = x(frame);
                    painter.line_segment(
                        [
                            Pos2::new(bx, wave_rect.top()),
                            Pos2::new(bx, wave_rect.bottom()),
                        ],
                        Stroke::new(1.0, Color32::from_rgb(44, 151, 158)),
                    );
                }
            }
        }
        let channels = self.peaks.len();
        let buckets = self.peaks[0].len();
        let bucket_frames = frames.div_ceil(8192).max(1);
        let pixels = wave_rect.width().ceil().max(1.0) as usize;
        for channel in 0..channels {
            let lane = wave_rect.height() / channels as f32;
            let center = wave_rect.top() + lane * (channel as f32 + 0.5);
            if channel > 0 {
                painter.line_segment(
                    [
                        Pos2::new(rect.left(), wave_rect.top() + lane * channel as f32),
                        Pos2::new(rect.right(), wave_rect.top() + lane * channel as f32),
                    ],
                    Stroke::new(1.0, Color32::from_gray(70)),
                );
            }
            painter.line_segment(
                [
                    Pos2::new(rect.left(), center),
                    Pos2::new(rect.right(), center),
                ],
                Stroke::new(1.0, Color32::from_rgb(45, 52, 78)),
            );
            for pixel in 0..pixels {
                let start = view_start + pixel * visible / pixels;
                let end = view_start + (pixel + 1) * visible / pixels;
                let first = (start / bucket_frames).min(buckets - 1);
                let last = (end / bucket_frames).min(buckets - 1);
                let (mut min, mut max) = (i16::MAX, i16::MIN);
                for &(lo, hi) in &self.peaks[channel][first..=last] {
                    min = min.min(lo);
                    max = max.max(hi);
                }
                let px = rect.left() + pixel as f32;
                let scale = lane * 0.45 / 32768.0;
                painter.line_segment(
                    [
                        Pos2::new(px, center - max as f32 * scale),
                        Pos2::new(px, center - min as f32 * scale),
                    ],
                    Stroke::new(1.0, Color32::from_rgb(226, 133, 205)),
                );
            }
        }

        // Pad start/end markers: dim what the pad will not play, drag the handles to trim.
        let play = self
            .pending_play
            .clone()
            .unwrap_or_else(|| self.sample.play_range());
        let dim = Color32::from_black_alpha(140);
        if play.start > view_start {
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(rect.left(), wave_rect.top()),
                    Pos2::new(x(play.start.min(view_start + visible)), wave_rect.bottom()),
                ),
                0.0,
                dim,
            );
        }
        if play.end < view_start + visible {
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(x(play.end.max(view_start)), wave_rect.top()),
                    Pos2::new(rect.right(), wave_rect.bottom()),
                ),
                0.0,
                dim,
            );
        }
        let mut new_play = play.clone();
        let mut committed = false;
        let mut handle_busy = false;
        for (is_start, frame, color, label) in [
            (true, play.start, Color32::from_rgb(110, 220, 130), "S"),
            (false, play.end, Color32::from_rgb(255, 170, 80), "E"),
        ] {
            if !in_view(frame) {
                continue;
            }
            let hx = x(frame);
            let grab = Rect::from_min_max(
                Pos2::new(hx - 6.0, wave_rect.top()),
                Pos2::new(hx + 6.0, wave_rect.bottom()),
            );
            let handle = ui.interact(grab, ui.id().with(("play_handle", is_start)), Sense::drag());
            if handle.hovered() || handle.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if handle.dragged() {
                handle_busy = true;
                if let Some(pos) = handle.interact_pointer_pos() {
                    let target = frame_at(pos);
                    if is_start {
                        new_play.start = target.min(play.end.saturating_sub(1));
                    } else {
                        new_play.end = target.max(play.start + 1).min(frames);
                    }
                }
            }
            if handle.drag_stopped() {
                committed = true;
            }
            let active = handle.hovered() || handle.dragged();
            painter.line_segment(
                [
                    Pos2::new(hx, wave_rect.top()),
                    Pos2::new(hx, wave_rect.bottom()),
                ],
                Stroke::new(if active { 2.5 } else { 1.5 }, color),
            );
            let flag = Rect::from_min_size(
                Pos2::new(if is_start { hx } else { hx - 14.0 }, wave_rect.top()),
                Vec2::new(14.0, 14.0),
            );
            painter.rect_filled(flag, 2.0, color);
            painter.text(
                flag.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(10.0),
                Color32::BLACK,
            );
        }
        if handle_busy {
            self.pending_play = Some(new_play.clone());
        }
        if committed {
            self.pending_play = None;
            if let Err(error) = self.sample.set_play_range(new_play) {
                self.message = error.to_string();
            }
        }

        // Playhead while previewing.
        if let Some((started, from, to, looping)) = self.playhead {
            let length = to.saturating_sub(from).max(1) as f64;
            let elapsed = started.elapsed().as_secs_f64() * sample_rate;
            let position = from
                + if looping {
                    elapsed % length
                } else {
                    elapsed.min(length)
                } as usize;
            if in_view(position) {
                let px = x(position);
                painter.line_segment(
                    [Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())],
                    Stroke::new(1.5, Color32::WHITE),
                );
            }
            ui.ctx().request_repaint();
        }

        if !handle_busy {
            if response.drag_started() || response.clicked() {
                let pos = if response.drag_started() {
                    ui.input(|input| input.pointer.press_origin())
                } else {
                    response.interact_pointer_pos()
                };
                if let Some(pos) = pos {
                    let frame = frame_at(pos);
                    self.anchor = Some(frame);
                    self.selection = frame..frame;
                }
            }
            if response.dragged() {
                if let (Some(anchor), Some(pos)) = (self.anchor, response.interact_pointer_pos()) {
                    let current = frame_at(pos);
                    self.selection = anchor.min(current)..anchor.max(current);
                }
            }
        }
        if response.drag_stopped() {
            self.anchor = None;
        }
        if frames > visible {
            ui.add(egui::Slider::new(&mut self.view_start, 0..=frames - visible).text("Scroll"));
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.set_width(215.0);
        ui.heading(format!("BANK: {}  PAD: {}", self.pad.bank, self.pad.number));
        ui.separator();
        let mut range = self.sample.play_range();
        ui.label(format!(
            "Channel: {}",
            if self.sample.channels == 1 {
                "MONO"
            } else {
                "STEREO"
            }
        ));
        ui.label(format!("Length (frames): {}", self.sample.frames()));
        ui.horizontal(|ui| {
            ui.label("Start");
            ui.add(egui::DragValue::new(&mut range.start).range(0..=range.end - 1));
        });
        ui.horizontal(|ui| {
            ui.label("End");
            ui.add(
                egui::DragValue::new(&mut range.end).range(range.start + 1..=self.sample.frames()),
            );
        });
        if let Err(error) = self.sample.set_play_range(range) {
            self.message = error.to_string();
        }
        if ui.button("Use selection for start/end").clicked() {
            if let Err(error) = self.sample.set_play_range(self.selection.clone()) {
                self.message = error.to_string();
            }
        }
        ui.separator();
        let mut settings = self.sample.settings();
        ui.horizontal(|ui| {
            Self::flag_checkbox(ui, "Gate", &mut settings.gate)
                .on_hover_text("Gate: sample plays only while the pad is held");
            Self::flag_checkbox(ui, "Pad loop", &mut settings.loop_mode)
                .on_hover_text("Loop: sample repeats between start and end");
        });
        ui.horizontal(|ui| {
            ui.label("Play mode");
            if settings.reverse <= 1 {
                egui::ComboBox::from_id_salt("pad_play_mode")
                    .selected_text(if settings.reverse == 0 {
                        "Forward"
                    } else {
                        "Reverse"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut settings.reverse, 0, "Forward");
                        ui.selectable_value(&mut settings.reverse, 1, "Reverse");
                    });
            } else {
                ui.label(format!("Unknown ({})", settings.reverse));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Level");
            if settings.level <= 127 {
                ui.add(egui::Slider::new(&mut settings.level, 0..=127));
            } else {
                ui.label(format!("Unknown ({})", settings.level));
            }
        });
        if let Err(error) = self.sample.set_settings(settings) {
            self.message = error.to_string();
        }
    }

    fn flag_checkbox(ui: &mut egui::Ui, label: &str, value: &mut u8) -> egui::Response {
        if *value <= 1 {
            let mut enabled = *value == 1;
            let response = ui.checkbox(&mut enabled, label);
            if response.changed() {
                *value = u8::from(enabled);
            }
            response
        } else {
            ui.label(format!("{label}: unknown ({value})"))
        }
    }

    fn styled(
        text: &str,
        enabled: bool,
        fill: Color32,
        stroke: Color32,
        color: Color32,
        strong: bool,
    ) -> egui::Button<'static> {
        let mut label = egui::RichText::new(text.to_owned()).size(13.0);
        if strong {
            label = label.strong();
        }
        if enabled {
            egui::Button::new(label.color(color))
                .fill(fill)
                .stroke(Stroke::new(1.0, stroke))
                .min_size(Vec2::new(0.0, 26.0))
        } else {
            egui::Button::new(label.color(Color32::from_gray(105)))
                .fill(Color32::from_rgb(40, 43, 50))
                .stroke(Stroke::new(1.0, Color32::from_gray(70)))
                .min_size(Vec2::new(0.0, 26.0))
                .sense(Sense::hover())
        }
    }

    fn button(text: &str, enabled: bool) -> egui::Button<'static> {
        Self::styled(
            text,
            enabled,
            Color32::from_rgb(52, 56, 66),
            Color32::from_gray(105),
            Color32::from_gray(230),
            false,
        )
    }

    fn primary(text: &str, enabled: bool) -> egui::Button<'static> {
        Self::styled(
            text,
            enabled,
            Color32::from_rgb(52, 112, 190),
            Color32::from_rgb(90, 150, 230),
            Color32::WHITE,
            true,
        )
    }

    fn danger(text: &str, enabled: bool) -> egui::Button<'static> {
        Self::styled(
            text,
            enabled,
            Color32::from_rgb(110, 48, 52),
            Color32::from_rgb(170, 90, 90),
            Color32::from_gray(240),
            false,
        )
    }

    fn group_label(ui: &mut egui::Ui, text: &str) {
        ui.label(
            egui::RichText::new(text)
                .small()
                .strong()
                .color(Color32::from_gray(150)),
        );
    }

    pub fn show(&mut self, ctx: &egui::Context, class: egui::ViewportClass) -> Action {
        self.poll_ai_job(ctx);
        if let Some(child) = self.player.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.player = None;
                    self.preview = None;
                    self.playhead = None;
                    if !status.success() {
                        self.message = format!("Preview failed: {status}");
                    }
                }
                Ok(None) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
                Err(e) => self.message = format!("Could not check preview: {e}"),
            }
        }
        if !ctx.wants_keyboard_input() {
            use egui::{Key, Modifiers};
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)) {
                self.toggle_preview();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z))
                || ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Y))
            {
                self.do_redo();
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Z)) {
                self.do_undo();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete))
                && self.selection.start < self.selection.end
            {
                let result = self.sample.delete(self.selection.clone());
                self.after_edit(result);
            }
        }
        let mut open = true;
        let mut action = Action::None;
        let mut contents = |ui: &mut egui::Ui| {
            {
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(120));
                widgets.inactive.bg_fill = Color32::from_gray(58);
                widgets.inactive.weak_bg_fill = Color32::from_gray(52);
                widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(180));
            }
            ui.horizontal_top(|ui| {
                ui.group(|ui| ui.vertical(|ui| self.settings_panel(ui)));
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} Hz · {} bit · {} channel(s) · {:.2} seconds",
                            self.sample.sample_rate,
                            self.sample.bits,
                            self.sample.channels,
                            self.seconds(self.sample.frames())
                        ));
                        ui.separator();
                        ui.label("Ruler");
                        ui.selectable_value(&mut self.bars_ruler, false, "Time");
                        ui.selectable_value(&mut self.bars_ruler, true, "Bars")
                            .on_hover_text("Bars and quarter notes (4/4) from the BPM and offset");
                        if self.bars_ruler {
                            ui.label("BPM");
                            ui.add(
                                egui::DragValue::new(&mut self.grid.bpm)
                                    .range(30.0..=300.0)
                                    .speed(0.1),
                            );
                            let max_offset = self.seconds(self.sample.frames());
                            ui.label("Offset (s)");
                            ui.add(
                                egui::DragValue::new(&mut self.grid.offset)
                                    .range(0.0..=max_offset)
                                    .speed(0.01),
                            );
                        }
                    });
                    self.waveform(ui);
                    ui.add_space(4.0);

                    Self::group_label(ui, "PLAYBACK");
                    ui.horizontal(|ui| {
                        let playing = self.player.is_some();
                        let label = if playing { "Restart preview" } else { "Preview selection" };
                        if ui
                            .add(Self::primary(label, true))
                            .on_hover_text("Play the selected region (Space toggles play/stop)")
                            .clicked()
                        {
                            if let Err(e) = self.preview() {
                                self.message = e;
                            }
                        }
                        if ui.add(Self::button("Stop", playing)).clicked() {
                            if let Err(e) = self.stop() {
                                self.message = e;
                            }
                        }
                        ui.checkbox(&mut self.loop_preview, "Loop preview");
                    });

                    ui.add_space(2.0);
                    Self::group_label(ui, "SELECTION");
                    ui.horizontal_wrapped(|ui| {
                        let frames = self.sample.frames();
                        ui.add(egui::DragValue::new(&mut self.selection.start).range(0..=frames));
                        ui.label("to");
                        ui.add(egui::DragValue::new(&mut self.selection.end).range(0..=frames));
                        let length = self.selection.end.saturating_sub(self.selection.start);
                        if self.bars_ruler && self.grid.bpm > 0.0 {
                            let beats = self.seconds(length) * self.grid.bpm / 60.0;
                            ui.label(format!("frames  ({beats:.2} quarters = {:.2} bars)", beats / 4.0));
                        } else {
                            ui.label(format!("frames  ({:.3} s)", self.seconds(length)));
                        }
                        ui.add(
                            egui::Slider::new(&mut self.zoom, 1.0..=256.0)
                                .logarithmic(true)
                                .text("Zoom"),
                        )
                        .on_hover_text("Mouse wheel zooms on the waveform; Shift+wheel pans");
                    });

                    ui.add_space(2.0);
                    Self::group_label(ui, "EDIT");
                    ui.horizontal_wrapped(|ui| {
                        let has_selection = self.selection.start < self.selection.end;
                        if ui
                            .add(Self::button("Trim to selection", has_selection))
                            .on_hover_text("Keep only the selection (undoable)")
                            .clicked()
                        {
                            let result = self.sample.trim(self.selection.clone());
                            self.after_edit(result);
                        }
                        if ui
                            .add(Self::danger("Delete selection", has_selection))
                            .on_hover_text("Remove the selection (Delete key, undoable)")
                            .clicked()
                        {
                            let result = self.sample.delete(self.selection.clone());
                            self.after_edit(result);
                        }
                        if ui
                            .add(Self::button("Normalize", true))
                            .on_hover_text("Scale the sample to full level (undoable)")
                            .clicked()
                        {
                            let result = self.sample.normalize();
                            self.after_edit(result);
                        }
                        ui.separator();
                        if ui
                            .add(Self::button("Undo", self.sample.can_undo()))
                            .on_hover_text("Ctrl+Z")
                            .clicked()
                        {
                            self.do_undo();
                        }
                        if ui
                            .add(Self::button("Redo", self.sample.can_redo()))
                            .on_hover_text("Ctrl+Y / Ctrl+Shift+Z")
                            .clicked()
                        {
                            self.do_redo();
                        }
                    });

                    ui.add_space(2.0);
                    ui.collapsing("Beat grid (snap selection to beats)", |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui.add(Self::button("Detect beats", true)).clicked() {
                                match self.sample.detect_beats() {
                                    Ok(grid) => {
                                        self.grid = grid;
                                        self.snap = true;
                                        self.message = format!(
                                            "Estimated {:.2} BPM; check the grid and adjust as needed.",
                                            grid.bpm
                                        );
                                    }
                                    Err(e) => {
                                        self.snap = false;
                                        self.message = e.to_string();
                                    }
                                }
                            }
                            if ui
                                .add(Self::button("Detect (AI)", self.ai_job.is_none()))
                                .on_hover_text("Neural beat tracking (beat-this); downloads its models on first use")
                                .clicked()
                            {
                                self.start_ai_detect();
                            }
                            ui.checkbox(&mut self.snap, "Snap to beats");
                            ui.label("BPM");
                            ui.add(
                                egui::DragValue::new(&mut self.grid.bpm)
                                    .range(30.0..=300.0)
                                    .speed(0.1),
                            );
                            ui.label("Offset (s)");
                            let max_offset = self.seconds(self.sample.frames());
                            ui.add(
                                egui::DragValue::new(&mut self.grid.offset)
                                    .range(0.0..=max_offset)
                                    .speed(0.01),
                            );
                        });
                    });

                    ui.collapsing("Tempo (change BPM, keep pitch)", |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("Grid {:.2} BPM  ->", self.grid.bpm));
                            ui.add(
                                egui::DragValue::new(&mut self.target_bpm)
                                    .range(30.0..=300.0)
                                    .speed(0.1)
                                    .suffix(" BPM"),
                            );
                            if ui
                                .add(Self::button("Stretch sample", true))
                                .on_hover_text("Time-stretch the whole sample so the grid tempo becomes the target")
                                .clicked()
                            {
                                self.stretch_to_target();
                            }
                        });
                    });

                    ui.separator();
                    if !self.message.is_empty() {
                        let failed = self.message.contains("failed")
                            || self.message.contains("Could not")
                            || self.message.contains("invalid");
                        ui.colored_label(
                            if failed {
                                Color32::from_rgb(255, 120, 110)
                            } else {
                                Color32::from_gray(190)
                            },
                            &self.message,
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add(Self::primary("Save to pad", self.sample.can_undo()))
                            .on_hover_text("Write the edited sample and pad settings to the card")
                            .clicked()
                        {
                            action = Action::Save;
                        }
                        if ui.add(Self::button("Close", true)).clicked() {
                            action = Action::Close;
                        }
                    });
                });
            });
        };
        if class == egui::ViewportClass::Embedded {
            egui::Window::new("Sample editor")
                .open(&mut open)
                .default_size([820.0, 550.0])
                .min_width(500.0)
                .frame(egui::Frame::window(&ctx.style()).fill(Color32::from_rgb(29, 31, 35)))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, &mut contents);
                });
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::central_panel(&ctx.style()).fill(Color32::from_rgb(29, 31, 35)))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, &mut contents);
                });
            if ctx.input(|input| input.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                open = false;
            }
        }
        if !open {
            action = Action::Close;
        }
        action
    }
}

impl Drop for EditorView {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_editor() -> EditorView {
        let mut wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        wav.write_all(b"RIFF").unwrap();
        wav.write_all(&2036u32.to_le_bytes()).unwrap();
        wav.write_all(b"WAVEfmt ").unwrap();
        wav.write_all(&16u32.to_le_bytes()).unwrap();
        wav.write_all(&1u16.to_le_bytes()).unwrap();
        wav.write_all(&1u16.to_le_bytes()).unwrap();
        wav.write_all(&1000u32.to_le_bytes()).unwrap();
        wav.write_all(&2000u32.to_le_bytes()).unwrap();
        wav.write_all(&2u16.to_le_bytes()).unwrap();
        wav.write_all(&16u16.to_le_bytes()).unwrap();
        wav.write_all(b"data").unwrap();
        wav.write_all(&2000u32.to_le_bytes()).unwrap();
        wav.write_all(&[0; 2000]).unwrap();
        wav.flush().unwrap();
        let pad = Pad {
            bank: 'A',
            number: 1,
            file: Some(wav.path().to_owned()),
            file_size: Some(2044),
            start: 512,
            end: 2512,
            channels: 1,
            settings: sp404sx_wave_converter::PadSettings::default(),
        };
        EditorView::open(PathBuf::new(), pad).unwrap()
    }

    #[test]
    fn waveform_drag_starts_at_press_position() {
        let mut editor = test_editor();
        let ctx = egui::Context::default();
        let start = Pos2::new(56.0, 50.0);
        let end = Pos2::new(298.0, 50.0);
        let mut frame = |events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 300.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| editor.waveform(ui));
            });
        };
        frame(vec![]);
        frame(vec![
            egui::Event::PointerMoved(start),
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        frame(vec![egui::Event::PointerMoved(end)]);
        frame(vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!((editor.selection.start as isize - 100).abs() <= 5);
        assert!((editor.selection.end as isize - 600).abs() <= 5);
    }

    #[test]
    fn native_close_is_cancelled_until_the_editor_decides_to_close() {
        let mut editor = test_editor();
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut action = Action::None;
        let output = ctx.run(input, |ctx| {
            action = editor.show(ctx, egui::ViewportClass::Immediate);
        });
        assert!(matches!(action, Action::Close));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::CancelClose)));
    }

    #[test]
    fn empty_preview_selection_is_rejected() {
        let mut editor = test_editor();
        editor.selection = 2..2;
        assert!(editor.preview().unwrap_err().contains("non-empty"));
        assert!(editor.player.is_none());
        assert!(editor.preview.is_none());
    }

    #[test]
    #[ignore = "requires ffplay and a working audio backend"]
    fn loop_preview_keeps_its_file_until_stopped_and_one_shot_exits() {
        let mut editor = test_editor();
        editor.selection = 0..100;
        editor.loop_preview = true;
        editor.preview().unwrap();
        let path = editor.preview.as_ref().unwrap().path().to_owned();
        std::thread::sleep(std::time::Duration::from_secs(5));
        assert!(path.exists());
        assert!(editor
            .player
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none());
        editor.stop().unwrap();
        assert!(!path.exists());
        assert!(editor.player.is_none());

        editor.loop_preview = false;
        editor.preview().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if let Some(status) = editor.player.as_mut().unwrap().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "one-shot preview did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        editor.stop().unwrap();
        assert!(editor.preview.is_none());
    }
}
