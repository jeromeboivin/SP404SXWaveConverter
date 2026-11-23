use crate::{Card, PADS_PER_BANK, PAD_INFO_SIZE, RECORD_SIZE};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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

fn inspect_source(source: &Path) -> io::Result<u8> {
    let extension = source.extension().and_then(|s| s.to_str()).unwrap_or("");
    if !["wav", "aif", "aiff"]
        .iter()
        .any(|allowed| extension.eq_ignore_ascii_case(allowed))
    {
        return Err(error("select an uncompressed WAV or AIFF file"));
    }
    let result = run(
        "ffprobe",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-select_streams".as_ref(),
            "a:0".as_ref(),
            "-show_entries".as_ref(),
            "stream=codec_name,channels".as_ref(),
            "-of".as_ref(),
            "default=noprint_wrappers=1".as_ref(),
            "-i".as_ref(),
            source.as_os_str(),
        ],
    )?;
    let codec = result
        .lines()
        .find_map(|s| s.strip_prefix("codec_name="))
        .unwrap_or("");
    if !["pcm_s", "pcm_u", "pcm_f"]
        .iter()
        .any(|prefix| codec.starts_with(prefix))
    {
        return Err(error("the audio must be uncompressed PCM"));
    }
    let channels = result
        .lines()
        .find_map(|s| s.strip_prefix("channels="))
        .and_then(|s| s.parse::<u8>().ok());
    match channels {
        Some(channels @ (1 | 2)) => Ok(channels),
        _ => Err(error("the audio must have one or two channels")),
    }
}

fn create_sample(source: &Path, directory: &Path, index: usize) -> io::Result<(PathBuf, u32, u8)> {
    let channels = inspect_source(source)?;
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
            "44100".as_ref(),
            "-c:a".as_ref(),
            "pcm_s16le".as_ref(),
            "-f".as_ref(),
            "s16le".as_ref(),
            raw.as_os_str(),
        ],
    )?;
    let mut audio = File::open(&raw)?;
    let length = audio.metadata()?.len();
    if length == 0 || length % (u64::from(channels) * 2) != 0 {
        return Err(error("the converted audio has no complete sample frames"));
    }
    let length = u32::try_from(length).map_err(|_| error("sample is too large"))?;
    let size = DATA_OFFSET
        .checked_add(length)
        .ok_or_else(|| error("sample is too large"))?;
    let mut header = [0u8; DATA_OFFSET as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(size - 8).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&18u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&u16::from(channels).to_le_bytes());
    header[24..28].copy_from_slice(&44100u32.to_le_bytes());
    header[28..32].copy_from_slice(&(44100u32 * u32::from(channels) * 2).to_le_bytes());
    header[32..34].copy_from_slice(&(u16::from(channels) * 2).to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
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
    output.sync_all()?;
    Ok((staged, size, channels))
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
    /// Import uncompressed WAV/AIFF into a pad, replacing its existing sample if present.
    pub fn import_sample(&mut self, bank: char, number: u8, source: &Path) -> io::Result<()> {
        self.change_sample(bank, number, Some(source))
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
}

impl Card {
    /// Delete a pad's sample and reset its record to the factory defaults.
    pub fn delete_sample(&mut self, bank: char, number: u8) -> io::Result<()> {
        self.change_sample(bank, number, None)
    }

    fn change_sample(&mut self, bank: char, number: u8, source: Option<&Path>) -> io::Result<()> {
        let bank = bank.to_ascii_uppercase();
        let pad = self
            .pad(bank, number)
            .ok_or_else(|| error("invalid bank or pad"))?;
        if source.is_none() && pad.file.is_none() {
            return Err(error("the selected pad is empty"));
        }
        let old_path = pad.file.clone();
        let old_size = pad.file_size;
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
            let (staged, record) = if let Some(source) = source {
                let (staged, size, channels) = create_sample(source, &transaction, index)?;
                (Some(staged), pad_record(size, channels))
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
                if source.is_some() {
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
            let backup = transaction.join("original.sample");
            if let Some(old) = &old_path {
                fs::rename(old, &backup)?;
            }
            let old = old_path
                .as_deref()
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
            let new_file = source.map(|_| destination.as_path());
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
