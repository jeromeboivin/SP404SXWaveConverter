//! Prototype audio analysis backends: neural beat tracking (beat-this) and
//! pitch-preserving time stretching (Signalsmith Stretch).

use crate::editor::BeatGrid;
use beat_this::{calculate_bpm, BeatThis, RtenRuntime};
use signalsmith_stretch::Stretch;
use std::io;
use std::path::PathBuf;

const MEL_MODEL: &str = "mel_spectrogram.onnx";
const BEAT_MODEL: &str = "beat_this.onnx";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Folder holding the beat-this ONNX models: `$SP404_MODELS`, `./models`, or `models/` next to the executable.
pub fn model_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("SP404_MODELS") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from("models"));
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("models")))
    {
        candidates.push(dir);
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(MEL_MODEL).is_file() && dir.join(BEAT_MODEL).is_file())
}

/// Turn beat times (seconds) into a constant-tempo grid.
pub fn grid_from_beats(beats: &[f32], bpm: f32) -> BeatGrid {
    let period = 60.0 / f64::from(bpm);
    let mut offsets: Vec<f64> = beats
        .iter()
        .enumerate()
        .map(|(i, t)| f64::from(*t) - i as f64 * period)
        .collect();
    offsets.sort_by(|a, b| a.total_cmp(b));
    let offset = offsets[offsets.len() / 2].rem_euclid(period);
    let aligned = beats
        .iter()
        .filter(|t| {
            let phase = (f64::from(**t) - offset).rem_euclid(period);
            phase.min(period - phase) < period * 0.1
        })
        .count();
    BeatGrid {
        bpm: f64::from(bpm),
        offset,
        confidence: aligned as f64 / beats.len() as f64,
    }
}

/// Run the beat-this network on mono audio. Loads the models on every call (about a second).
pub fn detect_beats(mono: &[f32], sample_rate: u32) -> io::Result<BeatGrid> {
    let dir = model_dir().ok_or_else(|| {
        invalid(
            "beat-this models not found; put beat_this.onnx and mel_spectrogram.onnx in ./models or set SP404_MODELS",
        )
    })?;
    let mut tracker = BeatThis::new(
        &RtenRuntime,
        &dir.join(MEL_MODEL),
        &dir.join(BEAT_MODEL),
    )
    .map_err(|e| io::Error::other(format!("could not load beat models: {e}")))?;
    let analysis = tracker
        .analyze_audio(mono, sample_rate)
        .map_err(|e| io::Error::other(format!("beat analysis failed: {e}")))?;
    let bpm = calculate_bpm(&analysis)
        .ok_or_else(|| invalid("fewer than two beats found; enter a grid manually"))?;
    Ok(grid_from_beats(&analysis.beats, bpm))
}

/// Stretch interleaved audio to `out_frames` frames without changing pitch.
pub fn stretch(
    input: &[f32],
    channels: u16,
    sample_rate: u32,
    out_frames: usize,
) -> io::Result<Vec<f32>> {
    let channels_usize = usize::from(channels);
    if input.is_empty() || out_frames == 0 || input.len() % channels_usize != 0 {
        return Err(invalid("nothing to stretch"));
    }
    let mut stretcher = Stretch::preset_default(u32::from(channels), sample_rate);
    let mut output = vec![0.0f32; out_frames * channels_usize];
    stretcher.exact(input, &mut output);
    Ok(output)
}
