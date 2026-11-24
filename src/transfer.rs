use crate::{Card, PADS_PER_BANK, RECORD_SIZE};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadTransfer {
    Copy,
    Move,
    Swap,
}

fn fail(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn index(bank: char, number: u8) -> io::Result<usize> {
    let bank = bank.to_ascii_uppercase();
    if !('A'..='J').contains(&bank) || !(1..=12).contains(&number) {
        return Err(fail("invalid bank or pad"));
    }
    Ok((bank as usize - 'A' as usize) * PADS_PER_BANK + usize::from(number - 1))
}

fn stage_sample(snapshot: &Path, staged: &Path, pad_index: usize) -> io::Result<()> {
    fs::copy(snapshot, staged)?;
    let mut file = OpenOptions::new().read(true).write(true).open(staged)?;
    let length = file.metadata()?.len();
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    if &header[..4] == b"RIFF" && &header[8..] == b"WAVE" {
        let riff_end = u64::from(u32::from_le_bytes(header[4..8].try_into().unwrap())) + 8;
        if riff_end > length || riff_end < 12 {
            return Err(fail("invalid WAV header"));
        }
        let mut position = 12u64;
        while position + 8 <= riff_end {
            file.seek(SeekFrom::Start(position))?;
            let mut chunk = [0u8; 8];
            file.read_exact(&mut chunk)?;
            let size = u64::from(u32::from_le_bytes(chunk[4..8].try_into().unwrap()));
            let next = position + 8 + size + size % 2;
            if next > riff_end {
                return Err(fail("invalid WAV chunk length"));
            }
            if &chunk[..4] == b"RLND" && size >= 16 {
                let mut marker = [0; 12];
                file.read_exact(&mut marker)?;
                if &marker[..8] == b"roifspsx" && &marker[8..] == 4u32.to_le_bytes().as_slice() {
                    file.write_all(&(pad_index as u32).to_le_bytes())?;
                }
            }
            position = next;
        }
    } else if &header[..4] != b"FORM" || !matches!(&header[8..], b"AIFF" | b"AIFC") {
        return Err(fail("unsupported pad sample container"));
    }
    file.sync_all()
}

fn destination(dir: &Path, bank: char, number: u8, sample: &Path) -> io::Result<PathBuf> {
    let extension = sample
        .extension()
        .and_then(|s| s.to_str())
        .ok_or_else(|| fail("sample extension is unavailable"))?;
    Ok(dir.join(format!(
        "{}{number:07}.{}",
        bank.to_ascii_uppercase(),
        extension.to_ascii_uppercase()
    )))
}

impl Card {
    /// Copy or move to an empty pad, or swap two occupied pads, retaining pad settings.
    pub fn transfer_sample(
        &mut self,
        from: (char, u8),
        to: (char, u8),
        operation: PadTransfer,
    ) -> io::Result<()> {
        let from_index = index(from.0, from.1)?;
        let to_index = index(to.0, to.1)?;
        if from_index == to_index {
            return Err(fail("choose two different pads"));
        }
        let from_pad = self
            .pad(from.0, from.1)
            .ok_or_else(|| fail("invalid source pad"))?;
        let to_pad = self
            .pad(to.0, to.1)
            .ok_or_else(|| fail("invalid destination pad"))?;
        let source = from_pad
            .file
            .clone()
            .ok_or_else(|| fail("source pad is empty"))?;
        let target = to_pad.file.clone();
        if operation == PadTransfer::Swap && target.is_none() {
            return Err(fail("swap requires two occupied pads"));
        }
        if operation != PadTransfer::Swap && target.is_some() {
            return Err(fail("destination pad must be empty"));
        }
        let dir = self.root.join("ROLAND/SP-404SX/SMPL");
        let transaction = dir.join(".sp404sx-converter-transaction");
        fs::create_dir(&transaction).map_err(|e| {
            fail(format!(
                "cannot start transaction at {}: {e}; check for an unfinished transaction",
                transaction.display()
            ))
        })?;
        let mut retain = false;
        let result = (|| {
            let initial = Card::open(&self.root)?;
            if initial.info != self.info
                || initial.pad(from.0, from.1).and_then(|p| p.file.as_ref()) != Some(&source)
                || initial.pad(to.0, to.1).and_then(|p| p.file.as_ref()) != target.as_ref()
            {
                return Err(fail(
                    "card changed since it was opened; refresh and try again",
                ));
            }
            let source_snapshot = transaction.join("source.snapshot");
            fs::copy(&source, &source_snapshot)?;
            let target_snapshot = transaction.join("target.snapshot");
            if let Some(target) = &target {
                fs::copy(target, &target_snapshot)?;
            }
            let target_new = transaction.join("target.new");
            stage_sample(&source_snapshot, &target_new, to_index)?;
            let source_new = transaction.join("source.new");
            if operation == PadTransfer::Swap {
                stage_sample(&target_snapshot, &source_new, from_index)?;
            }
            let target_path = destination(&dir, to.0, to.1, &source)?;
            let source_path = if let Some(target) = &target {
                destination(&dir, from.0, from.1, target)?
            } else {
                source.clone()
            };
            let mut info = self.info.clone();
            let source_record =
                self.info[from_index * RECORD_SIZE..(from_index + 1) * RECORD_SIZE].to_vec();
            let target_record =
                self.info[to_index * RECORD_SIZE..(to_index + 1) * RECORD_SIZE].to_vec();
            info[to_index * RECORD_SIZE..(to_index + 1) * RECORD_SIZE]
                .copy_from_slice(&source_record);
            if operation == PadTransfer::Swap {
                info[from_index * RECORD_SIZE..(from_index + 1) * RECORD_SIZE]
                    .copy_from_slice(&target_record);
            } else if operation == PadTransfer::Move {
                info[from_index * RECORD_SIZE..(from_index + 1) * RECORD_SIZE]
                    .copy_from_slice(&crate::write::empty_pad_record());
            }
            for (name, bytes) in [("PAD_INFO.original", &self.info), ("PAD_INFO.new", &info)] {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(transaction.join(name))?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            let mut manifest = File::create(transaction.join("operation.txt"))?;
            writeln!(
                manifest,
                "operation={operation:?}\nsource={}\ntarget={}",
                source.display(),
                target_path.display()
            )?;
            manifest.sync_all()?;

            let current = Card::open(&self.root)?;
            if current.info != self.info
                || current.pad(from.0, from.1).and_then(|p| p.file.as_ref()) != Some(&source)
                || current.pad(to.0, to.1).and_then(|p| p.file.as_ref()) != target.as_ref()
                || !crate::write::files_equal(&source, &source_snapshot)?
                || match &target {
                    Some(path) => !crate::write::files_equal(path, &target_snapshot)?,
                    None => false,
                }
            {
                return Err(fail("card changed during transfer; refresh and try again"));
            }

            let source_backup = transaction.join("source.original");
            let target_backup = transaction.join("target.original");
            let mut source_backed = false;
            let mut target_backed = false;
            let mut source_installed = false;
            let mut target_installed = false;
            let install = (|| -> io::Result<()> {
                if operation != PadTransfer::Copy {
                    fs::rename(&source, &source_backup)?;
                    source_backed = true;
                }
                if let Some(target) = &target {
                    fs::rename(target, &target_backup)?;
                    target_backed = true;
                }
                fs::rename(&target_new, &target_path)?;
                target_installed = true;
                if operation == PadTransfer::Swap {
                    fs::rename(&source_new, &source_path)?;
                    source_installed = true;
                }
                fs::rename(transaction.join("PAD_INFO.new"), dir.join("PAD_INFO.BIN"))?;
                Ok(())
            })();
            if let Err(e) = install {
                let rollback = (|| -> io::Result<()> {
                    if source_installed {
                        fs::remove_file(&source_path)?;
                    }
                    if target_installed {
                        fs::remove_file(&target_path)?;
                    }
                    if target_backed {
                        fs::rename(&target_backup, target.as_ref().unwrap())?;
                    }
                    if source_backed {
                        fs::rename(&source_backup, &source)?;
                    }
                    Ok(())
                })();
                if let Err(rollback) = rollback {
                    retain = true;
                    return Err(fail(format!(
                        "transfer failed: {e}; rollback failed: {rollback}; recover from {}",
                        transaction.display()
                    )));
                }
                return Err(fail(format!("transfer failed: {e}")));
            }
            Ok(())
        })();
        if result.is_ok() {
            match Card::open(&self.root) {
                Ok(card) => *self = card,
                Err(e) => {
                    return Err(fail(format!(
                        "transfer committed but card could not be refreshed: {e}; inspect {}",
                        transaction.display()
                    )))
                }
            }
        }
        if !retain {
            if let Err(cleanup) = fs::remove_dir_all(&transaction) {
                return Err(fail(format!(
                    "{}; transaction cleanup failed: {cleanup}; inspect {}",
                    match &result {
                        Ok(()) => "transfer committed".to_string(),
                        Err(e) => e.to_string(),
                    },
                    transaction.display()
                )));
            }
        }
        result
    }
}
