//! Prototype audio analysis backends: neural beat tracking (beat-this) and
//! pitch-preserving time stretching (Signalsmith Stretch).

use crate::editor::BeatGrid;
use beat_this::{calculate_bpm, BeatThis, RtenRuntime};
use signalsmith_stretch::Stretch;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MEL_MODEL: &str = "mel_spectrogram.onnx";
const BEAT_MODEL: &str = "beat_this.onnx";

/// Models fetched on first use: (file name, URL, SHA-256).
const DOWNLOADS: [(&str, &str, &str); 2] = [
    (
        MEL_MODEL,
        "https://raw.githubusercontent.com/danigb/beat-this-rs/main/models/mel_spectrogram.onnx",
        "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9",
    ),
    (
        BEAT_MODEL,
        "https://github.com/danigb/beat-this-rs/releases/download/model-large/beat_this.onnx",
        "5f810debe53459b559127fb55bbad40035bb47cc567b20e501670f968c770f02",
    ),
];

/// Where downloaded models are stored: `~/.sp404sx-wave-converter/models`.
pub fn home_model_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".sp404sx-wave-converter").join("models"))
}

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
    candidates.extend(home_model_dir());
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

fn download(url: &str, expected: &str, destination: &Path) -> io::Result<()> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| io::Error::other(format!("download of {url} failed: {e}")))?;
    let partial = destination.with_extension("onnx.part");
    let mut file = fs::File::create(&partial)?;
    let mut reader = response.into_reader();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let result = (|| -> io::Result<()> {
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n])?;
        }
        file.sync_all()?;
        let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if actual != expected {
            return Err(invalid(format!(
                "downloaded model has the wrong checksum ({actual}); not using it"
            )));
        }
        fs::rename(&partial, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Return the model folder, downloading the models into the home folder first if none is installed.
pub fn ensure_models() -> io::Result<PathBuf> {
    if let Some(dir) = model_dir() {
        return Ok(dir);
    }
    let dir = home_model_dir().ok_or_else(|| invalid("could not find your home folder"))?;
    fs::create_dir_all(&dir)?;
    for (name, url, sha) in DOWNLOADS {
        let target = dir.join(name);
        if !target.is_file() {
            download(url, sha, &target)?;
        }
    }
    Ok(dir)
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
    let dir = ensure_models()?;
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
