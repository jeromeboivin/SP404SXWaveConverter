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

/// Stem names in the order used by [`StemSet::stems`].
pub const STEM_NAMES: [&str; 4] = ["vocals", "drums", "bass", "other"];

/// Separated audio, interleaved i16 at the source's channel count and sample rate,
/// each stem exactly as long as the input.
#[derive(Clone)]
pub struct StemSet {
    pub stems: [Vec<i16>; 4],
}

impl StemSet {
    /// Sum the stems whose flag is set (clamped); `None` if nothing is selected.
    pub fn mix(&self, include: [bool; 4]) -> Option<Vec<i16>> {
        if !include.iter().any(|on| *on) {
            return None;
        }
        let len = self.stems[0].len();
        let mut out = vec![0i32; len];
        for (stem, on) in self.stems.iter().zip(include) {
            if on {
                for (acc, s) in out.iter_mut().zip(stem) {
                    *acc += i32::from(*s);
                }
            }
        }
        Some(
            out.into_iter()
                .map(|v| v.clamp(-32768, 32767) as i16)
                .collect(),
        )
    }
}

/// Write 16-bit PCM interleaved samples as a WAV file.
pub fn write_wav(path: &Path, samples: &[i16], channels: u16, sample_rate: u32) -> io::Result<()> {
    let len = u32::try_from(samples.len() * 2).map_err(|_| invalid("audio is too long"))?;
    let block = u32::from(channels) * 2;
    let mut out = io::BufWriter::new(fs::File::create(path)?);
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + len).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?;
    out.write_all(&channels.to_le_bytes())?;
    out.write_all(&sample_rate.to_le_bytes())?;
    out.write_all(&(sample_rate * block).to_le_bytes())?;
    out.write_all(&(block as u16).to_le_bytes())?;
    out.write_all(&16u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&len.to_le_bytes())?;
    for s in samples {
        out.write_all(&s.to_le_bytes())?;
    }
    out.flush()
}

/// Minimal reader for 16-bit PCM WAV files: (interleaved samples, channels, rate).
fn read_wav(path: &Path) -> io::Result<(Vec<i16>, u16, u32)> {
    let bytes = fs::read(path)?;
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid("stem output is not a WAV file"));
    }
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let end = (body + size).min(bytes.len());
        if id == b"fmt " && end - body >= 16 {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            if bits != 16 || channels == 0 {
                return Err(invalid("stem output is not 16-bit PCM"));
            }
            let samples = bytes[body..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            return Ok((samples, channels, rate));
        }
        pos = body + size + size % 2;
    }
    Err(invalid("stem output has no audio data"))
}

/// Convert interleaved audio to another channel count (1 or 2) and sample rate,
/// with a box filter when downsampling, and force an exact output length.
fn convert(
    samples: &[i16],
    from_channels: u16,
    from_rate: u32,
    to_channels: u16,
    to_rate: u32,
    out_frames: usize,
) -> Vec<i16> {
    let fc = usize::from(from_channels);
    let tc = usize::from(to_channels);
    let frames = samples.len() / fc;
    let channel = |frame: usize, c: usize| -> f32 {
        if frame >= frames {
            return 0.0;
        }
        if tc == 1 {
            (0..fc).map(|k| f32::from(samples[frame * fc + k])).sum::<f32>() / fc as f32
        } else {
            f32::from(samples[frame * fc + c.min(fc - 1)])
        }
    };
    let ratio = f64::from(from_rate) / f64::from(to_rate);
    let mut out = Vec::with_capacity(out_frames * tc);
    for j in 0..out_frames {
        let start = j as f64 * ratio;
        for c in 0..tc {
            let value = if (ratio - 1.0).abs() < 1e-9 {
                channel(j, c)
            } else if ratio > 1.0 {
                let (a, b) = (start.floor() as usize, ((start + ratio).ceil() as usize).max(1));
                let n = (a..b).count().max(1);
                (a..b).map(|f| channel(f, c)).sum::<f32>() / n as f32
            } else {
                let a = start.floor() as usize;
                let t = (start - a as f64) as f32;
                channel(a, c) * (1.0 - t) + channel(a + 1, c) * t
            };
            out.push(value.round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    out
}

/// Separate audio into vocals, drums, bass and other with htdemucs (stem-splitter-core).
/// The model (~210 MB) is downloaded by the crate on first use. This is slow on CPU.
pub fn separate(samples: &[i16], channels: u16, sample_rate: u32) -> io::Result<StemSet> {
    let frames = samples.len() / usize::from(channels.max(1));
    if frames == 0 {
        return Err(invalid("nothing to separate"));
    }
    let work = tempfile::tempdir()?;
    let input = work.path().join("input.wav");
    // The model needs 44.1 kHz stereo and the crate does not resample for us.
    let model_frames = (frames as f64 * 44100.0 / f64::from(sample_rate)).round() as usize;
    let model_input = convert(samples, channels, sample_rate, 2, 44100, model_frames);
    write_wav(&input, &model_input, 2, 44100)?;
    let result = stem_splitter_core::split_file(
        input.to_str().ok_or_else(|| invalid("temporary path is not UTF-8"))?,
        stem_splitter_core::SplitOptions {
            output_dir: work
                .path()
                .to_str()
                .ok_or_else(|| invalid("temporary path is not UTF-8"))?
                .to_owned(),
            ..Default::default()
        },
    )
    .map_err(|e| io::Error::other(format!("stem separation failed: {e}")))?;
    let paths = [
        result.vocals_path,
        result.drums_path,
        result.bass_path,
        result.other_path,
    ];
    let mut stems: [Vec<i16>; 4] = Default::default();
    for (slot, path) in stems.iter_mut().zip(paths) {
        let (data, c, r) = read_wav(Path::new(&path))?;
        *slot = convert(&data, c, r, channels, sample_rate, frames);
    }
    Ok(StemSet { stems })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_sums_ticked_stems_and_clamps() {
        let set = StemSet {
            stems: [vec![30000, -100], vec![30000, 50], vec![1, 1], vec![0, 0]],
        };
        assert_eq!(set.mix([true, true, false, false]).unwrap(), vec![32767, -50]);
        assert_eq!(set.mix([false, false, true, false]).unwrap(), vec![1, 1]);
        assert!(set.mix([false; 4]).is_none());
    }

    #[test]
    fn convert_changes_rate_channels_and_forces_length() {
        let stereo: Vec<i16> = (0..4410).flat_map(|i| [i as i16, i as i16]).collect();
        let mono = convert(&stereo, 2, 44100, 1, 22050, 2205);
        assert_eq!(mono.len(), 2205);
        assert!((i32::from(mono[1000]) - 2000).abs() < 3);
        let padded = convert(&stereo, 2, 44100, 2, 44100, 5000);
        assert_eq!(padded.len(), 10000);
        assert_eq!(padded[9999], 0);
    }

    #[test]
    fn wav_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        write_wav(&path, &[1, -2, 3, -4], 2, 22050).unwrap();
        assert_eq!(read_wav(&path).unwrap(), (vec![1, -2, 3, -4], 2, 22050));
    }
}
