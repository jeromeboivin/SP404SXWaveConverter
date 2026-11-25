use crate::{write::inspect_source, Pad, PadSettings};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone)]
struct State {
    samples: Arc<Vec<i16>>,
    play: Range<usize>,
    settings: PadSettings,
}

pub struct SampleEditor {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    state: State,
    original_samples: Arc<Vec<i16>>,
    undo: Vec<State>,
    redo: Vec<State>,
}

#[derive(Debug, Clone, Copy)]
pub struct BeatGrid {
    pub bpm: f64,
    pub offset: f64,
    pub confidence: f64,
}

impl BeatGrid {
    pub fn snap(&self, frame: usize, sample_rate: u32, frames: usize) -> usize {
        if !self.bpm.is_finite() || self.bpm <= 0.0 || !self.offset.is_finite() {
            return frame.min(frames);
        }
        let step = 60.0 * f64::from(sample_rate) / self.bpm;
        let origin = self.offset * f64::from(sample_rate);
        let snapped = origin + ((frame as f64 - origin) / step).round() * step;
        snapped.round().clamp(0.0, frames as f64) as usize
    }
}

impl SampleEditor {
    pub fn open(pad: &Pad) -> io::Result<Self> {
        let path = pad.file.as_ref().ok_or_else(|| invalid("pad is empty"))?;
        let format = inspect_source(path)?;
        let bits = format
            .bits
            .ok_or_else(|| invalid("sample bit depth is unavailable"))?;
        if format.sample_rate > 44100 {
            return Err(invalid("editor supports sample rates up to 44.1 kHz"));
        }
        if !matches!(
            format.codec.as_str(),
            "pcm_s8" | "pcm_u8" | "pcm_s16le" | "pcm_s16be" | "pcm_u16le" | "pcm_u16be"
        ) || !matches!(bits, 8 | 16)
            || !matches!(format.channels, 1 | 2)
            || format.channels != u32::from(pad.channels)
        {
            return Err(invalid(
                "editor supports 8- or 16-bit mono/stereo PCM samples",
            ));
        }
        let block_align = format.channels * (bits / 8);
        if pad.start < 512
            || pad.end <= pad.start
            || (pad.start - 512) % block_align != 0
            || (pad.end - 512) % block_align != 0
        {
            return Err(invalid("invalid pad playback offsets"));
        }
        let output = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-f", "s16le", "-c:a", "pcm_s16le", "-"])
            .output()
            .map_err(|e| io::Error::other(format!("could not run ffmpeg: {e}")))?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if output.stdout.len() % (format.channels as usize * 2) != 0 {
            return Err(invalid("incomplete sample frames"));
        }
        let samples: Vec<_> = output
            .stdout
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let play =
            ((pad.start - 512) / block_align) as usize..((pad.end - 512) / block_align) as usize;
        if play.end > samples.len() / format.channels as usize {
            return Err(invalid("pad playback offsets exceed the sample"));
        }
        let samples = Arc::new(samples);
        Ok(Self {
            sample_rate: format.sample_rate,
            channels: format.channels as u16,
            bits: bits as u16,
            state: State {
                samples: Arc::clone(&samples),
                play,
                settings: pad.settings,
            },
            original_samples: samples,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn frames(&self) -> usize {
        self.state.samples.len() / usize::from(self.channels)
    }
    pub fn samples(&self) -> &[i16] {
        &self.state.samples
    }
    pub fn play_range(&self) -> Range<usize> {
        self.state.play.clone()
    }
    pub fn settings(&self) -> PadSettings {
        self.state.settings
    }
    pub fn audio_changed(&self) -> bool {
        !Arc::ptr_eq(&self.state.samples, &self.original_samples)
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Estimate a periodic beat grid; rhythmic ambiguity can produce half/double tempo.
    pub fn detect_beats(&self) -> io::Result<BeatGrid> {
        let hop = (self.sample_rate as usize / 100).max(1);
        let channels = usize::from(self.channels);
        let frames = self.frames();
        if frames < self.sample_rate as usize * 3 {
            return Err(invalid(
                "at least three seconds of rhythmic audio are needed for beat detection",
            ));
        }
        let mut energy = Vec::with_capacity(frames.div_ceil(hop));
        for chunk in self.state.samples.chunks(hop * channels) {
            let mean =
                chunk.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / chunk.len() as f64;
            energy.push(mean.sqrt());
        }
        let mut novelty = vec![0.0; energy.len()];
        let mut background = 0.0;
        for (i, level) in energy.iter().copied().enumerate() {
            novelty[i] = (level - background * 1.15).max(0.0);
            background = background * 0.8 + level * 0.2;
        }
        let raw = novelty.clone();
        for i in 1..novelty.len() - 1 {
            novelty[i] = 0.25 * raw[i - 1] + 0.5 * raw[i] + 0.25 * raw[i + 1];
        }
        let norm = novelty.iter().map(|n| n * n).sum::<f64>();
        if norm < 1.0 {
            return Err(invalid("no rhythmic onsets detected"));
        }
        let hops_per_second = f64::from(self.sample_rate) / hop as f64;
        let correlation = |lag: f64| -> f64 {
            let whole = lag.floor() as usize;
            let fraction = lag - whole as f64;
            if whole + 1 >= novelty.len() {
                return 0.0;
            }
            let mut sum = 0.0;
            for i in 0..novelty.len() - whole - 1 {
                sum += novelty[i]
                    * (novelty[i + whole] * (1.0 - fraction) + novelty[i + whole + 1] * fraction);
            }
            sum / norm
        };
        let mut best = (0.0, 120.0);
        for quarter_bpm in 280..=720 {
            let bpm = quarter_bpm as f64 / 4.0;
            let lag = 60.0 * hops_per_second / bpm;
            let score = correlation(lag) * 0.6
                + correlation(2.0 * lag) * 0.25
                + correlation(3.0 * lag) * 0.15;
            if score > best.0 {
                best = (score, bpm);
            }
        }
        if best.1 * 2.0 <= 180.0 {
            let lag = 30.0 * hops_per_second / best.1;
            let double_score = correlation(lag) * 0.6
                + correlation(2.0 * lag) * 0.25
                + correlation(3.0 * lag) * 0.15;
            if double_score >= best.0 * 0.8 {
                best = (double_score, best.1 * 2.0);
            }
        }
        if best.0 < 0.08 {
            return Err(invalid(
                "no sufficiently regular beat detected; enter a grid manually",
            ));
        }
        let period = 60.0 * hops_per_second / best.1;
        let mut phase_scores = vec![0.0; period.ceil() as usize];
        for (i, onset) in novelty.iter().enumerate() {
            let phase = (i as f64 % period).round() as usize % phase_scores.len();
            phase_scores[phase] += onset;
        }
        let phase = phase_scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .ok_or_else(|| invalid("no beat phase found"))?;
        Ok(BeatGrid {
            bpm: best.1,
            offset: phase as f64 / hops_per_second,
            confidence: best.0.clamp(0.0, 1.0),
        })
    }

    /// Mono mix of the whole sample as f32 in -1..1, for analysis.
    pub fn mono_f32(&self) -> Vec<f32> {
        let channels = usize::from(self.channels);
        self.state
            .samples
            .chunks_exact(channels)
            .map(|frame| {
                frame.iter().map(|s| f32::from(*s)).sum::<f32>() / channels as f32 / 32768.0
            })
            .collect()
    }

    /// Replace the audio in `range` with `audio` of the same length (undoable).
    pub fn replace_audio(&mut self, range: Range<usize>, audio: &[i16]) -> io::Result<()> {
        self.check_range(&range)?;
        let channels = usize::from(self.channels);
        if audio.len() != (range.end - range.start) * channels {
            return Err(invalid("replacement audio has the wrong length"));
        }
        self.checkpoint();
        Arc::make_mut(&mut self.state.samples)[range.start * channels..range.end * channels]
            .copy_from_slice(audio);
        Ok(())
    }

    /// Samples of `range` (interleaved).
    pub fn samples_in(&self, range: Range<usize>) -> io::Result<&[i16]> {
        self.check_range(&range)?;
        let channels = usize::from(self.channels);
        Ok(&self.state.samples[range.start * channels..range.end * channels])
    }

    /// Time-stretch `range` by `ratio` (new length / old length) without changing pitch.
    /// Play range edges move with the audio.
    pub fn time_stretch(&mut self, range: Range<usize>, ratio: f64) -> io::Result<()> {
        self.check_range(&range)?;
        if !ratio.is_finite() || !(0.25..=4.0).contains(&ratio) {
            return Err(invalid("stretch ratio must be between 0.25 and 4"));
        }
        let channels = usize::from(self.channels);
        let old_len = range.end - range.start;
        let new_len = ((old_len as f64 * ratio).round() as usize).max(1);
        let input: Vec<f32> = self.state.samples[range.start * channels..range.end * channels]
            .iter()
            .map(|s| f32::from(*s) / 32768.0)
            .collect();
        let output = crate::ai::stretch(&input, self.channels, self.sample_rate, new_len)?;
        let map = |position: usize| {
            if position <= range.start {
                position
            } else if position >= range.end {
                position - old_len + new_len
            } else {
                range.start + ((position - range.start) as f64 * ratio).round() as usize
            }
        };
        let play = map(self.state.play.start)..map(self.state.play.end);
        if play.start >= play.end {
            return Err(invalid("stretch would collapse the pad playback range"));
        }
        let mut samples = Vec::with_capacity(self.state.samples.len() - old_len * channels + output.len());
        samples.extend_from_slice(&self.state.samples[..range.start * channels]);
        samples.extend(
            output
                .iter()
                .map(|v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16),
        );
        samples.extend_from_slice(&self.state.samples[range.end * channels..]);
        self.checkpoint();
        self.state.samples = Arc::new(samples);
        self.state.play = play;
        Ok(())
    }

    fn check_range(&self, range: &Range<usize>) -> io::Result<()> {
        if range.start >= range.end || range.end > self.frames() {
            return Err(invalid("select a non-empty region within the sample"));
        }
        Ok(())
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.state.clone());
        self.redo.clear();
    }

    pub fn set_play_range(&mut self, range: Range<usize>) -> io::Result<()> {
        self.check_range(&range)?;
        if range != self.state.play {
            self.checkpoint();
            self.state.play = range;
        }
        Ok(())
    }

    pub fn set_settings(&mut self, settings: PadSettings) -> io::Result<()> {
        settings.validate_changes(self.state.settings)?;
        if settings != self.state.settings {
            self.checkpoint();
            self.state.settings = settings;
        }
        Ok(())
    }

    pub fn trim(&mut self, range: Range<usize>) -> io::Result<()> {
        self.check_range(&range)?;
        let channels = usize::from(self.channels);
        self.checkpoint();
        self.state.samples =
            Arc::new(self.state.samples[range.start * channels..range.end * channels].to_vec());
        self.state.play = 0..range.end - range.start;
        Ok(())
    }

    pub fn delete(&mut self, range: Range<usize>) -> io::Result<()> {
        self.check_range(&range)?;
        if range.end - range.start == self.frames() {
            return Err(invalid("cannot delete the entire sample"));
        }
        let shift = |position: usize| {
            if position <= range.start {
                position
            } else if position >= range.end {
                position - (range.end - range.start)
            } else {
                range.start
            }
        };
        let play = shift(self.state.play.start)..shift(self.state.play.end);
        if play.start == play.end {
            return Err(invalid(
                "deletion would remove the entire pad playback range; trim instead",
            ));
        }
        let channels = usize::from(self.channels);
        self.checkpoint();
        Arc::make_mut(&mut self.state.samples).drain(range.start * channels..range.end * channels);
        self.state.play = play;
        Ok(())
    }

    pub fn normalize(&mut self) -> io::Result<()> {
        let peak = self
            .state
            .samples
            .iter()
            .map(|sample| i32::from(*sample).abs())
            .max()
            .unwrap_or(0);
        if peak == 0 {
            return Err(invalid("cannot normalize silent audio"));
        }
        self.checkpoint();
        for sample in Arc::make_mut(&mut self.state.samples) {
            *sample = ((i32::from(*sample) * 32767) / peak).clamp(-32768, 32767) as i16;
        }
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        if let Some(state) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.state, state));
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(state) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.state, state));
            true
        } else {
            false
        }
    }

    pub fn write_wav(&self, path: &Path) -> io::Result<()> {
        self.write_wav_range(path, 0..self.frames())
    }

    pub fn write_wav_range(&self, path: &Path, range: Range<usize>) -> io::Result<()> {
        self.check_range(&range)?;
        let block = u32::from(self.channels) * u32::from(self.bits / 8);
        let len = u32::try_from(range.end - range.start)
            .ok()
            .and_then(|n| n.checked_mul(block))
            .ok_or_else(|| invalid("sample is too large"))?;
        let riff = 36u32
            .checked_add(len)
            .and_then(|n| n.checked_add(len % 2))
            .ok_or_else(|| invalid("sample is too large"))?;
        let mut file = BufWriter::new(std::fs::File::create(path)?);
        file.write_all(b"RIFF")?;
        file.write_all(&riff.to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&self.channels.to_le_bytes())?;
        file.write_all(&self.sample_rate.to_le_bytes())?;
        file.write_all(&(self.sample_rate * block).to_le_bytes())?;
        file.write_all(&(block as u16).to_le_bytes())?;
        file.write_all(&self.bits.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&len.to_le_bytes())?;
        let channels = usize::from(self.channels);
        let samples = &self.state.samples[range.start * channels..range.end * channels];
        if self.bits == 8 {
            let bytes: Vec<u8> = samples
                .iter()
                .map(|s| ((i32::from(*s) >> 8) + 128) as u8)
                .collect();
            file.write_all(&bytes)?;
        } else {
            for sample in samples {
                file.write_all(&sample.to_le_bytes())?;
            }
        }
        if len % 2 != 0 {
            file.write_all(&[0])?;
        }
        file.flush()?;
        file.get_ref().sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(samples: Vec<i16>, channels: u16, play: Range<usize>) -> SampleEditor {
        let samples = Arc::new(samples);
        SampleEditor {
            sample_rate: 22050,
            channels,
            bits: 16,
            state: State {
                samples: Arc::clone(&samples),
                play,
                settings: PadSettings::default(),
            },
            original_samples: samples,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    #[test]
    fn pad_settings_and_play_range_are_validated_and_undoable() {
        let mut sample = editor(vec![1, 2, 3, 4], 1, 0..4);
        let initial = sample.settings();
        let mut settings = initial;
        settings.level = 90;
        settings.loop_mode = 1;
        sample.set_settings(settings).unwrap();
        sample.set_play_range(1..3).unwrap();
        assert!(!sample.audio_changed());
        assert!(sample.set_play_range(3..3).is_err());
        assert!(sample.set_play_range(0..5).is_err());
        let mut invalid = settings;
        invalid.gate = 2;
        assert!(sample.set_settings(invalid).is_err());
        assert!(sample.undo());
        assert_eq!(sample.play_range(), 0..4);
        assert_eq!(sample.settings(), settings);
        assert!(sample.undo());
        assert_eq!(sample.settings(), initial);
        assert!(sample.redo());
        assert_eq!(sample.settings(), settings);
        sample.trim(1..3).unwrap();
        assert!(sample.audio_changed());
        assert!(sample.undo());
        assert!(!sample.audio_changed());
    }

    #[test]
    fn editing_keeps_stereo_frames_and_tracks_undo_and_play_range() {
        let mut sample = editor(vec![1, 11, 2, 12, 3, 13, 4, 14, 5, 15], 2, 1..5);
        sample.delete(1..3).unwrap();
        assert_eq!(sample.samples(), &[1, 11, 4, 14, 5, 15]);
        assert_eq!(sample.play_range(), 1..3);
        assert!(sample.undo());
        assert_eq!(sample.play_range(), 1..5);
        assert!(sample.redo());
        sample.trim(1..3).unwrap();
        assert_eq!(sample.samples(), &[4, 14, 5, 15]);
        assert_eq!(sample.play_range(), 0..2);
        assert!(!sample.can_redo());
        assert!(sample.undo());
        assert_eq!(sample.samples(), &[1, 11, 4, 14, 5, 15]);
    }

    #[test]
    fn invalid_or_silent_edits_leave_history_unchanged() {
        let mut sample = editor(vec![0, 0, 0], 1, 1..2);
        assert!(sample.delete(0..3).is_err());
        assert!(sample.delete(1..2).is_err());
        assert!(sample.trim(2..2).is_err());
        assert!(sample.normalize().is_err());
        assert!(!sample.can_undo());
        assert_eq!(sample.samples(), &[0, 0, 0]);
    }

    #[test]
    fn normalization_scales_both_channels_equally_and_is_undoable() {
        let mut sample = editor(vec![-10000, 5000, 10000, 0], 2, 0..2);
        sample.normalize().unwrap();
        assert_eq!(sample.samples(), &[-32767, 16383, 32767, 0]);
        assert!(sample.undo());
        assert_eq!(sample.samples(), &[-10000, 5000, 10000, 0]);
    }

    #[test]
    fn writes_only_selected_frames_for_preview() {
        let sample = editor(vec![1, 11, 2, 12, 3, 13, 4, 14], 2, 0..4);
        let path = tempfile::NamedTempFile::new().unwrap();
        sample.write_wav_range(path.path(), 1..3).unwrap();
        let data = std::fs::read(path.path()).unwrap();
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 8);
        assert_eq!(&data[44..], &[2, 0, 12, 0, 3, 0, 13, 0]);
        assert!(sample.write_wav_range(path.path(), 3..3).is_err());
        assert!(sample.write_wav_range(path.path(), 0..5).is_err());
        assert_eq!(std::fs::read(path.path()).unwrap(), data);

        let mut eight_bit = editor(vec![-32768, -16384, 0, 16384, 32512], 1, 0..5);
        eight_bit.bits = 8;
        eight_bit.write_wav_range(path.path(), 1..4).unwrap();
        let data = std::fs::read(path.path()).unwrap();
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 3);
        assert_eq!(&data[44..], &[64, 128, 192, 0]);
    }

    #[test]
    fn detects_regular_clicks_and_snaps_to_grid() {
        let rate = 10000usize;
        let mut samples = vec![0; rate * 8];
        for beat in 0..16 {
            let start = rate / 5 + beat * rate / 2;
            if start + 40 <= samples.len() {
                samples[start..start + 40].fill(25000);
            }
        }
        let mut sample = editor(samples, 1, 0..rate * 8);
        sample.sample_rate = rate as u32;
        let grid = sample.detect_beats().unwrap();
        assert!((grid.bpm - 120.0).abs() < 1.0, "{grid:?}");
        assert!(
            (grid.snap(7300, rate as u32, sample.frames()) as isize - 7000).abs() <= 100,
            "{grid:?}"
        );
    }

    #[test]
    fn silence_does_not_invent_a_beat() {
        let sample = editor(vec![0; 22050 * 4], 1, 0..22050 * 4);
        assert!(sample.detect_beats().is_err());
    }

    #[test]
    fn estimates_tempos_with_quieter_offbeat_events() {
        let rate = 10000usize;
        for bpm in [90.0, 120.0, 160.0] {
            let mut samples = vec![0; rate * 12];
            let mut beat = 0;
            loop {
                let start = ((0.27 + beat as f64 * 60.0 / bpm) * rate as f64).round() as usize;
                if start + 45 >= samples.len() {
                    break;
                }
                samples[start..start + 45].fill(if beat % 4 == 0 { 28000 } else { 18000 });
                let offbeat = start + (30.0 / bpm * rate as f64) as usize;
                if offbeat + 30 < samples.len() {
                    samples[offbeat..offbeat + 30].fill(5000);
                }
                beat += 1;
            }
            let mut sample = editor(samples, 1, 0..rate * 12);
            sample.sample_rate = rate as u32;
            let grid = sample.detect_beats().unwrap();
            assert!((grid.bpm - bpm).abs() < 2.0, "expected {bpm}, got {grid:?}");
            assert!(
                (grid.snap(2700, rate as u32, sample.frames()) as isize - 2700).abs() <= 150,
                "{grid:?}"
            );
        }
    }
    fn sine(rate: u32, hz: f32, seconds: f32) -> Vec<i16> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 12000.0) as i16)
            .collect()
    }

    fn crossings_per_second(samples: &[i16], rate: u32) -> f32 {
        let crossings = samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        crossings as f32 * rate as f32 / samples.len() as f32
    }

    #[test]
    fn replace_audio_keeps_length_and_is_undoable() {
        let mut sample = editor(vec![1, 2, 3, 4, 5, 6], 2, 0..3);
        sample.replace_audio(1..3, &[9, 9, 8, 8]).unwrap();
        assert_eq!(sample.samples(), &[1, 2, 9, 9, 8, 8]);
        assert!(sample.audio_changed());
        assert!(sample.replace_audio(0..1, &[1]).is_err());
        assert!(sample.undo());
        assert_eq!(sample.samples(), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn time_stretch_changes_length_but_not_pitch() {
        let mut sample = editor(sine(22050, 440.0, 2.0), 1, 0..44100);
        sample.time_stretch(0..44100, 1.5).unwrap();
        assert_eq!(sample.frames(), 66150);
        assert_eq!(sample.play_range(), 0..66150);
        let middle = &sample.samples()[16000..50000];
        let hz = crossings_per_second(middle, 22050);
        assert!((hz - 440.0).abs() < 15.0, "pitch drifted to {hz}");
        assert!(sample.undo());
        assert_eq!(sample.frames(), 44100);
        assert!(sample.time_stretch(0..44100, 9.0).is_err());
    }

    #[test]
    fn time_stretch_of_a_region_moves_later_play_edges() {
        let mut sample = editor(sine(22050, 220.0, 2.0), 1, 1000..40000);
        sample.time_stretch(10000..20000, 2.0).unwrap();
        assert_eq!(sample.frames(), 54100);
        assert_eq!(sample.play_range(), 1000..50000);
    }

    #[test]
    fn neural_beats_find_a_click_track_tempo_when_models_are_installed() {
        if crate::ai::model_dir().is_none() {
            eprintln!("skipping: beat-this models not installed");
            return;
        }
        let rate = 22050usize;
        let mut samples = vec![0i16; rate * 12];
        for beat in 0..24 {
            let start = rate / 4 + beat * rate / 2;
            for (i, s) in samples[start..start + 1500].iter_mut().enumerate() {
                let decay = 1.0 - i as f32 / 1500.0;
                *s = ((i as f32 * 0.05).sin() * 24000.0 * decay) as i16;
            }
        }
        let len = samples.len();
        let sample = editor(samples, 1, 0..len);
        let grid = crate::ai::detect_beats(&sample.mono_f32(), 22050).unwrap();
        assert!((grid.bpm - 120.0).abs() < 3.0, "{grid:?}");
    }
}
