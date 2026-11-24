use crate::{Card, Pad, PadSettings, PADS_PER_BANK, PAD_INFO_SIZE, RECORD_SIZE};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

const DATA_OFFSET: u32 = 512;

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn sample_dir(root: &Path) -> PathBuf {
    root.join("ROLAND").join("SP-404SX").join("SMPL")
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| error(format!("could not run {program}: {e}")))?;
    if !output.status.success() {
        return Err(error(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|e| error(format!("{program} output is invalid: {e}")))
}

pub(crate) struct AudioFormat {
    pub(crate) codec: String,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u32,
    pub(crate) bits: Option<u32>,
}

pub(crate) fn inspect_source(source: &Path) -> io::Result<AudioFormat> {
    let result = run(
        "ffprobe",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-select_streams".as_ref(),
            "a:0".as_ref(),
            "-show_entries".as_ref(),
            "stream=codec_name,sample_rate,channels,bits_per_sample,bits_per_raw_sample".as_ref(),
            "-of".as_ref(),
            "default=noprint_wrappers=1".as_ref(),
            "-i".as_ref(),
            source.as_os_str(),
        ],
    )?;
    let field = |key: &str| {
        result.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name == key).then_some(value)
        })
    };
    let number = |key| {
        field(key)
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|n| *n > 0)
    };
    Ok(AudioFormat {
        codec: field("codec_name")
            .filter(|codec| !codec.is_empty())
            .ok_or_else(|| error("no audio stream found"))?
            .to_owned(),
        sample_rate: number("sample_rate").ok_or_else(|| error("invalid audio sample rate"))?,
        channels: number("channels").ok_or_else(|| error("invalid audio channel count"))?,
        bits: number("bits_per_raw_sample").or_else(|| number("bits_per_sample")),
    })
}

fn create_sample(source: &Path, directory: &Path, index: usize) -> io::Result<(PathBuf, u32, u8)> {
    let format = inspect_source(source)?;
    let channels = format.channels.min(2) as u8;
    let sample_rate = format.sample_rate.min(44100);
    let bits = if (matches!(format.codec.as_str(), "pcm_u8" | "pcm_s8" | "flac" | "alac"))
        && format.bits.is_some_and(|bits| bits <= 8)
    {
        8u16
    } else {
        16u16
    };
    let (codec, raw_format) = if bits == 8 {
        ("pcm_u8", "u8")
    } else {
        ("pcm_s16le", "s16le")
    };
    let rate_arg = sample_rate.to_string();
    let channels_arg = channels.to_string();
    let raw = directory.join("audio.raw");
    run(
        "ffmpeg",
        &[
            "-nostdin".as_ref(),
            "-hide_banner".as_ref(),
            "-loglevel".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            source.as_os_str(),
            "-map".as_ref(),
            "0:a:0".as_ref(),
            "-vn".as_ref(),
            "-ar".as_ref(),
            rate_arg.as_ref(),
            "-ac".as_ref(),
            channels_arg.as_ref(),
            "-c:a".as_ref(),
            codec.as_ref(),
            "-f".as_ref(),
            raw_format.as_ref(),
            raw.as_os_str(),
        ],
    )?;
    let mut audio = File::open(&raw)?;
    let length = audio.metadata()?.len();
    let block_align = u16::from(channels) * (bits / 8);
    if length == 0 || length % u64::from(block_align) != 0 {
        return Err(error("the converted audio has no complete sample frames"));
    }
    let length = u32::try_from(length).map_err(|_| error("sample is too large"))?;
    let end = DATA_OFFSET
        .checked_add(length)
        .ok_or_else(|| error("sample is too large"))?;
    let size = end
        .checked_add(length % 2)
        .ok_or_else(|| error("sample is too large"))?;
    let mut header = [0u8; DATA_OFFSET as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(size - 8).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&18u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&u16::from(channels).to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits.to_le_bytes());
    header[38..42].copy_from_slice(b"RLND");
    header[42..46].copy_from_slice(&458u32.to_le_bytes());
    header[46..54].copy_from_slice(b"roifspsx");
    header[54..58].copy_from_slice(&4u32.to_le_bytes());
    header[58..62].copy_from_slice(&(index as u32).to_le_bytes());
    header[504..508].copy_from_slice(b"data");
    header[508..512].copy_from_slice(&length.to_le_bytes());
    let staged = directory.join("sample.new");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    output.write_all(&header)?;
    io::copy(&mut audio, &mut output)?;
    if length % 2 != 0 {
        output.write_all(&[0])?;
    }
    output.sync_all()?;
    Ok((staged, end, channels))
}

fn pad_record(size: u32, channels: u8) -> [u8; RECORD_SIZE] {
    let mut record = [0; RECORD_SIZE];
    for (offset, value) in [(0, DATA_OFFSET), (4, size), (8, DATA_OFFSET), (12, size)] {
        record[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    record[16] = 127;
    record[19] = 1;
    record[21] = 1;
    record[22] = channels;
    record[24..28].copy_from_slice(&1200u32.to_be_bytes());
    record[28..32].copy_from_slice(&1200u32.to_be_bytes());
    record
}

pub(crate) fn empty_pad_record() -> [u8; RECORD_SIZE] {
    pad_record(DATA_OFFSET, 2)
}

fn restore(old: Option<(&Path, &Path)>, new_file: Option<&Path>) -> io::Result<()> {
    if let Some(path) = new_file {
        fs::remove_file(path)?;
    }
    if let Some((backup, original)) = old {
        fs::rename(backup, original)?;
    }
    Ok(())
}

impl Card {
    /// Import FFmpeg-supported audio into a pad, replacing its existing sample if present.
    pub fn import_sample(&mut self, bank: char, number: u8, source: &Path) -> io::Result<()> {
        self.change_sample(bank, number, Some(source), None)
    }

    /// Replace an edited sample while retaining its pad settings.
    pub fn save_edited_sample(
        &mut self,
        bank: char,
        number: u8,
        source: &Path,
        original_snapshot: &Path,
        playback: Range<usize>,
    ) -> io::Result<()> {
        let settings = self
            .pad(bank, number)
            .ok_or_else(|| error("invalid bank or pad"))?
            .settings;
        self.save_edited_pad(
            bank,
            number,
            Some(source),
            original_snapshot,
            playback,
            settings,
        )
    }

    /// Save pad settings and optionally replace the sample with edited audio.
    pub fn save_edited_pad(
        &mut self,
        bank: char,
        number: u8,
        source: Option<&Path>,
        original_snapshot: &Path,
        playback: Range<usize>,
        settings: PadSettings,
    ) -> io::Result<()> {
        self.change_sample(
            bank,
            number,
            Some(source.unwrap_or(original_snapshot)),
            Some(EditSpec {
                original_snapshot,
                playback,
                settings,
                metadata_only: source.is_none(),
            }),
        )
    }
}

struct EditSpec<'a> {
    original_snapshot: &'a Path,
    playback: Range<usize>,
    settings: PadSettings,
    metadata_only: bool,
}

pub(crate) fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let mut a = File::open(a)?;
    let mut b = File::open(b)?;
    let length = a.metadata()?.len();
    if length != b.metadata()?.len() {
        return Ok(false);
    }
    let mut remaining = length;
    let mut first = [0; 65536];
    let mut second = [0; 65536];
    while remaining > 0 {
        let count = remaining.min(first.len() as u64) as usize;
        a.read_exact(&mut first[..count])?;
        b.read_exact(&mut second[..count])?;
        if first[..count] != second[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

impl Pad {
    /// Return the pad's trimmed preview range in seconds.
    pub fn preview_range(&self) -> io::Result<(f64, f64)> {
        let file = self.file.as_ref().ok_or_else(|| error("pad is empty"))?;
        let size = self
            .file_size
            .ok_or_else(|| error("sample size is unavailable"))?;
        let format = inspect_source(file)?;
        let bits = format
            .bits
            .ok_or_else(|| error("sample bit depth is unavailable"))?;
        if !format.codec.starts_with("pcm_")
            || !matches!(bits, 8 | 16)
            || !matches!(self.channels, 1 | 2)
            || format.channels != u32::from(self.channels)
        {
            return Err(error("unsupported sample format or channel settings"));
        }
        let alignment = u32::from(self.channels) * (bits / 8);
        if self.start < DATA_OFFSET
            || self.end <= self.start
            || u64::from(self.end) > size
            || (self.start - DATA_OFFSET) % alignment != 0
            || (self.end - DATA_OFFSET) % alignment != 0
        {
            return Err(error("invalid sample trim settings"));
        }
        let bytes_per_second = f64::from(format.sample_rate) * f64::from(alignment);
        Ok((
            f64::from(self.start - DATA_OFFSET) / bytes_per_second,
            f64::from(self.end - self.start) / bytes_per_second,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> (tempfile::TempDir, Card) {
        let root = tempfile::tempdir().unwrap();
        let dir = sample_dir(root.path());
        fs::create_dir_all(&dir).unwrap();
        let mut info = Vec::new();
        for _ in 0..120 {
            info.extend_from_slice(&pad_record(DATA_OFFSET, 2));
        }
        fs::write(dir.join("PAD_INFO.BIN"), info).unwrap();
        let card = Card::open(root.path()).unwrap();
        (root, card)
    }

    fn wav(path: &Path, channels: u16) -> Vec<u8> {
        let samples: Vec<u8> = (0..(8 * channels))
            .flat_map(|n| (n as i16).to_le_bytes())
            .collect();
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&channels.to_le_bytes());
        data.extend_from_slice(&44100u32.to_le_bytes());
        data.extend_from_slice(&(44100u32 * u32::from(channels) * 2).to_le_bytes());
        data.extend_from_slice(&(channels * 2).to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        data.extend_from_slice(&samples);
        fs::write(path, data).unwrap();
        samples
    }

    #[test]
    fn import_replace_and_delete_update_only_target_pad() {
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        let pcm = wav(&source, 1);
        let dir = sample_dir(root.path());
        let initial_info = card.info.clone();
        card.import_sample('C', 4, &source).unwrap();
        let path = dir.join("C0000004.WAV");
        let sample = fs::read(&path).unwrap();
        assert_eq!(&sample[0..4], b"RIFF");
        assert_eq!(&sample[46..54], b"roifspsx");
        assert_eq!(u32::from_le_bytes(sample[58..62].try_into().unwrap()), 27);
        assert_eq!(&sample[504..508], b"data");
        assert_eq!(&sample[512..], pcm);
        let index = 27 * RECORD_SIZE;
        assert_eq!(&card.info[..index], &initial_info[..index]);
        assert_eq!(
            &card.info[index + RECORD_SIZE..],
            &initial_info[index + RECORD_SIZE..]
        );
        assert_eq!(
            &card.info[index..index + RECORD_SIZE],
            &pad_record(sample.len() as u32, 1)
        );
        assert_eq!(card.pad('C', 4).unwrap().channels, 1);
        assert_eq!(
            Card::open(root.path())
                .unwrap()
                .pad('C', 4)
                .unwrap()
                .file_size,
            Some(sample.len() as u64)
        );

        let stereo = wav(&source, 2);
        card.import_sample('C', 4, &source).unwrap();
        assert_eq!(&fs::read(&path).unwrap()[512..], stereo);
        assert_eq!(card.pad('C', 4).unwrap().channels, 2);

        card.delete_sample('C', 4).unwrap();
        assert!(!path.exists());
        assert_eq!(card.info, initial_info);
        assert!(card.pad('C', 4).unwrap().file.is_none());
        assert!(!dir.join(".sp404sx-converter-transaction").exists());
    }

    #[test]
    fn failed_import_and_stale_card_do_not_change_existing_files() {
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        wav(&source, 1);
        card.import_sample('A', 1, &source).unwrap();
        let dir = sample_dir(root.path());
        let before = fs::read(dir.join("A0000001.WAV")).unwrap();
        let info = card.info.clone();
        fs::write(&source, b"not audio").unwrap();
        assert!(card.import_sample('A', 1, &source).is_err());
        assert_eq!(fs::read(dir.join("A0000001.WAV")).unwrap(), before);
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), info);

        let mut modified = info.clone();
        modified[RECORD_SIZE + 16] = 99;
        fs::write(dir.join("PAD_INFO.BIN"), &modified).unwrap();
        assert!(card
            .delete_sample('A', 1)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert_eq!(fs::read(dir.join("A0000001.WAV")).unwrap(), before);
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), modified);
    }

    #[test]
    fn aiff_sample_is_detected_and_deleted() {
        let (root, _) = card();
        let dir = sample_dir(root.path());
        let path = dir.join("A0000001.aif");
        fs::write(&path, b"fixture").unwrap();
        let mut card = Card::open(root.path()).unwrap();
        assert_eq!(
            card.pad('A', 1).unwrap().file.as_deref(),
            Some(path.as_path())
        );
        card.delete_sample('A', 1).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn imports_aiff_and_refuses_unfinished_transactions() {
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        let pcm = wav(&source, 2);
        let aiff = root.path().join("source.aiff");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&source)
            .args(["-c:a", "pcm_s16be"])
            .args(["-f", "aiff"])
            .arg(&aiff)
            .status()
            .unwrap();
        assert!(status.success());
        card.import_sample('J', 12, &aiff).unwrap();
        let dir = sample_dir(root.path());
        let written = fs::read(dir.join("J0000012.WAV")).unwrap();
        assert_eq!(&written[512..], pcm);
        assert_eq!(u32::from_le_bytes(written[58..62].try_into().unwrap()), 119);

        fs::create_dir(dir.join(".sp404sx-converter-transaction")).unwrap();
        let message = card.delete_sample('J', 12).unwrap_err().to_string();
        assert!(message.contains("unfinished transaction"));
        assert_eq!(fs::read(dir.join("J0000012.WAV")).unwrap(), written);
    }

    #[test]
    fn imports_8_bit_and_preserves_odd_length_and_preview_timing() {
        let (root, mut card) = card();
        let source = root.path().join("eight.wav");
        let samples = [0u8, 64, 128, 192, 255];
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&42u32.to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&22050u32.to_le_bytes());
        data.extend_from_slice(&22050u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&8u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(&samples);
        data.push(0);
        fs::write(&source, data).unwrap();
        card.import_sample('A', 1, &source).unwrap();

        let sample = fs::read(sample_dir(root.path()).join("A0000001.WAV")).unwrap();
        assert_eq!(
            u32::from_le_bytes(sample[4..8].try_into().unwrap()),
            sample.len() as u32 - 8
        );
        assert_eq!(
            u32::from_le_bytes(sample[24..28].try_into().unwrap()),
            22050
        );
        assert_eq!(u16::from_le_bytes(sample[34..36].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(sample[508..512].try_into().unwrap()), 5);
        assert_eq!(&sample[512..517], &samples);
        assert_eq!(sample[517], 0);
        let mut pad = card.pad('A', 1).unwrap().clone();
        assert_eq!(pad.end, 517);
        pad.start = 513;
        let (start, duration) = pad.preview_range().unwrap();
        assert!((start - 1.0 / 22050.0).abs() < 1e-12);
        assert!((duration - 4.0 / 22050.0).abs() < 1e-12);

        let path = sample_dir(root.path()).join("A0000001.WAV");
        let snapshot = root.path().join("snapshot.wav");
        fs::copy(&path, &snapshot).unwrap();
        let mut editor = crate::editor::SampleEditor::open(card.pad('A', 1).unwrap()).unwrap();
        editor.trim(1..4).unwrap();
        let edited = root.path().join("edited.wav");
        editor.write_wav(&edited).unwrap();
        card.save_edited_sample('A', 1, &edited, &snapshot, editor.play_range())
            .unwrap();
        let saved = fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(saved[508..512].try_into().unwrap()), 3);
        assert_eq!(&saved[512..], &[64, 128, 192, 0]);
        assert_eq!(card.pad('A', 1).unwrap().end, 515);
    }

    #[test]
    fn imports_compressed_and_caps_rate_precision_and_channels() {
        let (root, mut card) = card();
        let mono_mp3 = root.path().join("mono.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=32000:duration=0.1",
            ])
            .arg(&mono_mp3)
            .status()
            .unwrap();
        assert!(status.success());
        card.import_sample('A', 1, &mono_mp3).unwrap();
        let mono = fs::read(sample_dir(root.path()).join("A0000001.WAV")).unwrap();
        assert_eq!(u16::from_le_bytes(mono[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(mono[24..28].try_into().unwrap()), 32000);
        assert_eq!(u16::from_le_bytes(mono[34..36].try_into().unwrap()), 16);
        assert!((card.pad('A', 1).unwrap().preview_range().unwrap().1 - 0.1).abs() < 0.001);

        let surround = root.path().join("surround.flac");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0|0|sin(2*PI*440*t)|0|0|0:s=96000:c=5.1:d=0.1",
                "-t",
                "0.1",
                "-c:a",
                "flac",
                "-sample_fmt",
                "s32",
            ])
            .arg(&surround)
            .status()
            .unwrap();
        assert!(status.success());
        card.import_sample('A', 2, &surround).unwrap();
        let stereo = fs::read(sample_dir(root.path()).join("A0000002.WAV")).unwrap();
        assert_eq!(u16::from_le_bytes(stereo[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(stereo[24..28].try_into().unwrap()),
            44100
        );
        assert_eq!(u16::from_le_bytes(stereo[34..36].try_into().unwrap()), 16);
        assert_eq!(card.pad('A', 2).unwrap().channels, 2);
        assert!(stereo[512..]
            .chunks_exact(4)
            .any(|frame| frame[0..2] != [0, 0] && frame[2..4] != [0, 0]));
        assert!((card.pad('A', 2).unwrap().preview_range().unwrap().1 - 0.1).abs() < 0.001);
    }

    #[test]
    fn preserves_stereo_lossless_samples_without_an_extension() {
        let (root, mut card) = card();
        let flac = root.path().join("source.flac");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=22050:duration=0.1",
                "-ac",
                "2",
                "-c:a",
                "flac",
            ])
            .arg(&flac)
            .status()
            .unwrap();
        assert!(status.success());
        let source = root.path().join("source");
        fs::copy(&flac, &source).unwrap();
        let decoded = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&source)
            .args(["-f", "s16le", "-c:a", "pcm_s16le", "-"])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        card.import_sample('B', 1, &source).unwrap();
        let sample = fs::read(sample_dir(root.path()).join("B0000001.WAV")).unwrap();
        assert_eq!(u16::from_le_bytes(sample[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(sample[24..28].try_into().unwrap()),
            22050
        );
        assert_eq!(u16::from_le_bytes(sample[34..36].try_into().unwrap()), 16);
        assert_eq!(&sample[512..], decoded.stdout);
    }

    #[test]
    fn edited_save_preserves_settings_and_rejects_same_size_source_changes() {
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        wav(&source, 2);
        card.import_sample('A', 1, &source).unwrap();
        let dir = sample_dir(root.path());
        let mut info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        info[16..32].copy_from_slice(&[31, 4, 5, 6, 7, 8, 2, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
        fs::write(dir.join("PAD_INFO.BIN"), &info).unwrap();
        card = Card::open(root.path()).unwrap();
        let path = dir.join("A0000001.WAV");
        let snapshot = root.path().join("snapshot.wav");
        fs::copy(&path, &snapshot).unwrap();
        let mut editor = crate::editor::SampleEditor::open(card.pad('A', 1).unwrap()).unwrap();
        editor.delete(2..4).unwrap();
        let edited = root.path().join("edited.wav");
        editor.write_wav(&edited).unwrap();
        let mut changed = fs::read(&path).unwrap();
        changed[512] ^= 1;
        fs::write(&path, &changed).unwrap();
        assert!(card
            .save_edited_sample('A', 1, &edited, &snapshot, editor.play_range())
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), info);
        assert_eq!(fs::read(&path).unwrap(), changed);
        fs::copy(&snapshot, &path).unwrap();
        let mut wrong_rate = fs::read(&edited).unwrap();
        wrong_rate[24..28].copy_from_slice(&32000u32.to_le_bytes());
        wrong_rate[28..32].copy_from_slice(&(32000u32 * 4).to_le_bytes());
        let wrong = root.path().join("wrong-rate.wav");
        fs::write(&wrong, wrong_rate).unwrap();
        assert!(card
            .save_edited_sample('A', 1, &wrong, &snapshot, editor.play_range())
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), fs::read(&snapshot).unwrap());
        assert!(card
            .save_edited_sample('A', 1, &edited, &snapshot, 0..99)
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), fs::read(&snapshot).unwrap());
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), info);
        let mut settings = card.pad('A', 1).unwrap().settings;
        settings.level = 90;
        settings.loop_mode = 1;
        card.save_edited_pad(
            'A',
            1,
            Some(&edited),
            &snapshot,
            editor.play_range(),
            settings,
        )
        .unwrap();
        let new_info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        let mut expected = info[16..32].to_vec();
        expected[0] = 90;
        expected[2] = 1;
        assert_eq!(&new_info[16..32], expected);
        assert_eq!(&new_info[32..], &info[32..]);
        assert_eq!(card.pad('A', 1).unwrap().start, 512);
        assert_eq!(card.pad('A', 1).unwrap().end, 512 + 6 * 4);
        let output = fs::read(&path).unwrap();
        let original = fs::read(&snapshot).unwrap();
        assert_eq!(
            &output[512..],
            [&original[512..520], &original[528..]].concat()
        );
    }

    #[test]
    fn metadata_only_edit_preserves_audio_and_rejects_conflicts() {
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        wav(&source, 2);
        card.import_sample('A', 1, &source).unwrap();
        let dir = sample_dir(root.path());
        let path = dir.join("A0000001.wav");
        fs::rename(dir.join("A0000001.WAV"), &path).unwrap();
        let mut info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        info[17] = 42;
        info[19] = 8;
        info[21] = 9;
        fs::write(dir.join("PAD_INFO.BIN"), &info).unwrap();
        card = Card::open(root.path()).unwrap();
        let snapshot = root.path().join("snapshot.wav");
        fs::copy(&path, &snapshot).unwrap();
        let original_audio = fs::read(&path).unwrap();
        let mut settings = card.pad('A', 1).unwrap().settings;
        settings.level = 64;
        settings.loop_mode = 1;
        settings.reverse = 1;

        let mut invalid = settings;
        invalid.reverse = 2;
        assert!(card
            .save_edited_pad('A', 1, None, &snapshot, 1..7, invalid)
            .is_err());
        for range in [2..2, 0..9, 7..6] {
            assert!(card
                .save_edited_pad('A', 1, None, &snapshot, range, settings)
                .is_err());
        }
        let mut changed_audio = original_audio.clone();
        changed_audio[512] ^= 1;
        fs::write(&path, &changed_audio).unwrap();
        assert!(card
            .save_edited_pad('A', 1, None, &snapshot, 1..7, settings)
            .is_err());
        fs::write(&path, &original_audio).unwrap();
        let mut changed_info = info.clone();
        changed_info[32] ^= 1;
        fs::write(dir.join("PAD_INFO.BIN"), &changed_info).unwrap();
        assert!(card
            .save_edited_pad('A', 1, None, &snapshot, 1..7, settings)
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), original_audio);
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), changed_info);
        fs::write(dir.join("PAD_INFO.BIN"), &info).unwrap();

        card.save_edited_pad('A', 1, None, &snapshot, 1..7, settings)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original_audio);
        assert!(!dir.join("A0000001.WAV").exists());
        info[8..12].copy_from_slice(&(512u32 + 4).to_be_bytes());
        info[12..16].copy_from_slice(&(512u32 + 28).to_be_bytes());
        info[16] = 64;
        info[18] = 1;
        info[20] = 1;
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), info);
        assert_eq!(card.pad('A', 1).unwrap().settings, settings);
        assert!(!dir.join(".sp404sx-converter-transaction").exists());
    }

    #[test]
    fn copy_move_and_swap_preserve_samples_and_settings() {
        use crate::PadTransfer;
        let (root, mut card) = card();
        let source = root.path().join("source.wav");
        wav(&source, 2);
        card.import_sample('A', 1, &source).unwrap();
        let dir = sample_dir(root.path());
        let mut info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        info[16] = 42;
        fs::write(dir.join("PAD_INFO.BIN"), &info).unwrap();
        card = Card::open(root.path()).unwrap();
        let source_path = dir.join("A0000001.WAV");
        let original = fs::read(&source_path).unwrap();
        assert!(card
            .transfer_sample(('A', 1), ('A', 1), PadTransfer::Copy)
            .is_err());
        assert!(card
            .transfer_sample(('A', 1), ('B', 1), PadTransfer::Swap)
            .is_err());
        card.transfer_sample(('A', 1), ('B', 1), PadTransfer::Copy)
            .unwrap();
        let copied = fs::read(dir.join("B0000001.WAV")).unwrap();
        assert_eq!(&copied[512..], &original[512..]);
        assert_eq!(u32::from_le_bytes(copied[58..62].try_into().unwrap()), 12);
        let info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        assert_eq!(&info[0..32], &info[12 * 32..13 * 32]);
        assert!(card
            .transfer_sample(('A', 1), ('B', 1), PadTransfer::Move)
            .is_err());
        card.transfer_sample(('B', 1), ('C', 2), PadTransfer::Move)
            .unwrap();
        assert!(!dir.join("B0000001.WAV").exists());
        let moved = fs::read(dir.join("C0000002.WAV")).unwrap();
        assert_eq!(&moved[512..], &original[512..]);
        assert_eq!(u32::from_le_bytes(moved[58..62].try_into().unwrap()), 25);
        let second_path = dir.join("C0000002.WAV");
        let mut second = moved;
        second[512] ^= 1;
        fs::write(&second_path, &second).unwrap();
        let mut info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        info[25 * 32 + 16] = 13;
        fs::write(dir.join("PAD_INFO.BIN"), &info).unwrap();
        card = Card::open(root.path()).unwrap();
        card.transfer_sample(('A', 1), ('C', 2), PadTransfer::Swap)
            .unwrap();
        let new_info = fs::read(dir.join("PAD_INFO.BIN")).unwrap();
        assert_eq!(new_info[16], 13);
        assert_eq!(new_info[25 * 32 + 16], 42);
        assert_eq!(&fs::read(&source_path).unwrap()[512..], &second[512..]);
        assert_eq!(&fs::read(&second_path).unwrap()[512..], &original[512..]);
        assert_eq!(
            u32::from_le_bytes(fs::read(&source_path).unwrap()[58..62].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_le_bytes(
                fs::read(dir.join("C0000002.WAV")).unwrap()[58..62]
                    .try_into()
                    .unwrap()
            ),
            25
        );
        let mut external_info = new_info.clone();
        external_info[33] ^= 1;
        fs::write(dir.join("PAD_INFO.BIN"), &external_info).unwrap();
        assert!(card
            .transfer_sample(('A', 1), ('B', 1), PadTransfer::Copy)
            .is_err());
        assert!(!dir.join("B0000001.WAV").exists());
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), external_info);
        assert!(!dir.join(".sp404sx-converter-transaction").exists());
        fs::write(&source_path, b"invalid sample").unwrap();
        card = Card::open(root.path()).unwrap();
        assert!(card
            .transfer_sample(('A', 1), ('B', 1), PadTransfer::Move)
            .is_err());
        assert_eq!(fs::read(&source_path).unwrap(), b"invalid sample");
        assert_eq!(fs::read(dir.join("PAD_INFO.BIN")).unwrap(), external_info);
        assert!(!dir.join(".sp404sx-converter-transaction").exists());
    }
}

impl Card {
    /// Delete a pad's sample and reset its record to the factory defaults.
    pub fn delete_sample(&mut self, bank: char, number: u8) -> io::Result<()> {
        self.change_sample(bank, number, None, None)
    }

    fn change_sample(
        &mut self,
        bank: char,
        number: u8,
        source: Option<&Path>,
        edit: Option<EditSpec<'_>>,
    ) -> io::Result<()> {
        let bank = bank.to_ascii_uppercase();
        let pad = self
            .pad(bank, number)
            .ok_or_else(|| error("invalid bank or pad"))?;
        if source.is_none() && pad.file.is_none() {
            return Err(error("the selected pad is empty"));
        }
        let old_path = pad.file.clone();
        let old_size = pad.file_size;
        if let Some(edit) = &edit {
            edit.settings.validate_changes(pad.settings)?;
            let original = old_path
                .as_deref()
                .ok_or_else(|| error("the selected pad is empty"))?;
            if !files_equal(original, edit.original_snapshot)? {
                return Err(error("sample changed since the editor was opened"));
            }
        }
        let index = (bank as usize - 'A' as usize) * PADS_PER_BANK + usize::from(number - 1);
        let dir = sample_dir(&self.root);
        let transaction = dir.join(".sp404sx-converter-transaction");
        fs::create_dir(&transaction).map_err(|e| {
            error(format!(
                "cannot start transaction at {}: {e}; check for an unfinished transaction",
                transaction.display()
            ))
        })?;
        let mut retain = false;
        let result = (|| {
            let current = Card::open(&self.root)?;
            let current_pad = current
                .pad(bank, number)
                .ok_or_else(|| error("invalid pad"))?;
            if current.info != self.info
                || current_pad.file != old_path
                || current_pad.file_size != old_size
            {
                return Err(error(
                    "card changed since it was opened; refresh and try again",
                ));
            }
            let metadata_only = edit.as_ref().is_some_and(|edit| edit.metadata_only);
            let (staged, record) = if let Some(source) = source {
                let (staged, size, channels) = if metadata_only {
                    let mut snapshot_pad = pad.clone();
                    snapshot_pad.file = Some(source.to_owned());
                    let sample = crate::editor::SampleEditor::open(&snapshot_pad)?;
                    let block = usize::from(sample.channels) * usize::from(sample.bits / 8);
                    let size = sample
                        .frames()
                        .checked_mul(block)
                        .and_then(|n| n.checked_add(DATA_OFFSET as usize))
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| error("sample is too large"))?;
                    (None, size, sample.channels as u8)
                } else {
                    let (staged, size, channels) = create_sample(source, &transaction, index)?;
                    (Some(staged), size, channels)
                };
                let record = if let Some(edit) = &edit {
                    let source_format = inspect_source(source)?;
                    let format = inspect_source(staged.as_deref().unwrap_or(source))?;
                    let original_format = inspect_source(edit.original_snapshot)?;
                    let bits = format
                        .bits
                        .ok_or_else(|| error("converted sample bit depth is unavailable"))?;
                    if !matches!(bits, 8 | 16)
                        || channels != pad.channels
                        || source_format.channels != format.channels
                        || source_format.sample_rate != format.sample_rate
                        || source_format.bits != format.bits
                        || source_format.channels != original_format.channels
                        || source_format.sample_rate != original_format.sample_rate
                        || source_format.bits != original_format.bits
                    {
                        return Err(error("edited sample format does not match the pad"));
                    }
                    let block = usize::from(channels) * (bits as usize / 8);
                    let start = edit
                        .playback
                        .start
                        .checked_mul(block)
                        .and_then(|n| n.checked_add(DATA_OFFSET as usize));
                    let end = edit
                        .playback
                        .end
                        .checked_mul(block)
                        .and_then(|n| n.checked_add(DATA_OFFSET as usize));
                    let (Some(start), Some(end)) = (start, end) else {
                        return Err(error("invalid edited playback range"));
                    };
                    if start >= end || end > size as usize {
                        return Err(error("invalid edited playback range"));
                    }
                    let mut record: [u8; RECORD_SIZE] = self.info
                        [index * RECORD_SIZE..(index + 1) * RECORD_SIZE]
                        .try_into()
                        .map_err(|_| error("invalid pad record"))?;
                    if !metadata_only {
                        record[0..4].copy_from_slice(&DATA_OFFSET.to_be_bytes());
                        record[4..8].copy_from_slice(&size.to_be_bytes());
                    }
                    for (offset, value) in [(8, start as u32), (12, end as u32)] {
                        record[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
                    }
                    record[16] = edit.settings.level;
                    record[18] = edit.settings.loop_mode;
                    record[19] = edit.settings.gate;
                    record[20] = edit.settings.reverse;
                    record
                } else {
                    pad_record(size, channels)
                };
                (staged, record)
            } else {
                (None, pad_record(DATA_OFFSET, 2))
            };
            let mut info = self.info.clone();
            if info.len() != PAD_INFO_SIZE {
                return Err(error("invalid PAD_INFO.BIN size"));
            }
            info[index * RECORD_SIZE..(index + 1) * RECORD_SIZE].copy_from_slice(&record);
            let staged_info = transaction.join("PAD_INFO.new");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged_info)?;
            file.write_all(&info)?;
            file.sync_all()?;
            drop(file);
            let mut previous_info = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(transaction.join("PAD_INFO.original"))?;
            previous_info.write_all(&self.info)?;
            previous_info.sync_all()?;
            let mut manifest = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(transaction.join("operation.txt"))?;
            writeln!(manifest, "pad={bank}{number}")?;
            writeln!(
                manifest,
                "original={}",
                old_path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "(empty)".into())
            )?;
            writeln!(
                manifest,
                "new={}",
                if metadata_only {
                    old_path
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "(empty)".into())
                } else if source.is_some() {
                    format!("{bank}{number:07}.WAV")
                } else {
                    "(deleted)".into()
                }
            )?;
            manifest.sync_all()?;
            drop(manifest);
            drop(previous_info);

            // Recheck after conversion, before the first destructive rename.
            let current = Card::open(&self.root)?;
            let current_pad = current
                .pad(bank, number)
                .ok_or_else(|| error("invalid pad"))?;
            if current.info != self.info
                || current_pad.file != old_path
                || current_pad.file_size != old_size
            {
                return Err(error(
                    "card changed during conversion; refresh and try again",
                ));
            }
            if let Some(edit) = &edit {
                let original = old_path
                    .as_deref()
                    .ok_or_else(|| error("the selected pad is empty"))?;
                if !files_equal(original, edit.original_snapshot)? {
                    return Err(error(
                        "sample changed during editing; refresh and try again",
                    ));
                }
            }
            let backup = transaction.join("original.sample");
            if !metadata_only {
                if let Some(old) = &old_path {
                    fs::rename(old, &backup)?;
                }
            }
            let old = (!metadata_only)
                .then_some(old_path.as_deref())
                .flatten()
                .map(|original| (backup.as_path(), original));
            let destination = dir.join(format!("{bank}{number:07}.WAV"));
            if let Some(staged) = staged {
                if let Err(e) = fs::rename(staged, &destination) {
                    if let Err(rollback) = restore(old, None) {
                        retain = true;
                        return Err(error(format!("sample install failed: {e}; rollback failed: {rollback}; recover from {}", transaction.display())));
                    }
                    return Err(error(format!("sample install failed: {e}")));
                }
            }
            let new_file = (!metadata_only && source.is_some()).then_some(destination.as_path());
            if let Err(e) = fs::rename(&staged_info, dir.join("PAD_INFO.BIN")) {
                if let Err(rollback) = restore(old, new_file) {
                    retain = true;
                    return Err(error(format!(
                        "metadata update failed: {e}; rollback failed: {rollback}; recover from {}",
                        transaction.display()
                    )));
                }
                return Err(error(format!("metadata update failed: {e}")));
            }
            Ok(())
        })();
        if result.is_ok() {
            match Card::open(&self.root) {
                Ok(card) => *self = card,
                Err(e) => {
                    return Err(error(format!(
                        "change committed but card could not be refreshed: {e}; inspect {}",
                        transaction.display()
                    )));
                }
            }
        }
        if !retain {
            if let Err(cleanup) = fs::remove_dir_all(&transaction) {
                return Err(error(format!(
                    "{}; transaction cleanup failed: {cleanup}; inspect {}",
                    match &result {
                        Ok(()) => "change committed".to_string(),
                        Err(e) => e.to_string(),
                    },
                    transaction.display()
                )));
            }
        }
        result
    }
}
