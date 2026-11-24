use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub mod ai;
pub mod editor;
mod transfer;
mod write;
pub use transfer::PadTransfer;

pub const BANKS: usize = 10;
pub const PADS_PER_BANK: usize = 12;
const RECORD_SIZE: usize = 32;
const PAD_INFO_SIZE: usize = BANKS * PADS_PER_BANK * RECORD_SIZE;

#[derive(Debug, Clone)]
pub struct Pad {
    pub bank: char,
    pub number: u8,
    pub file: Option<PathBuf>,
    pub file_size: Option<u64>,
    pub start: u32,
    pub end: u32,
    pub channels: u8,
    pub settings: PadSettings,
}

/// Raw SX/A settings bytes. Unknown values are retained so they are not silently changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadSettings {
    pub level: u8,
    pub loop_mode: u8,
    pub gate: u8,
    pub reverse: u8,
}

impl Default for PadSettings {
    fn default() -> Self {
        Self {
            level: 127,
            loop_mode: 0,
            gate: 1,
            reverse: 0,
        }
    }
}

impl PadSettings {
    pub fn validate_changes(self, original: Self) -> io::Result<()> {
        if self.level != original.level && self.level > 127 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid pad level",
            ));
        }
        for (name, value, previous) in [
            ("loop", self.loop_mode, original.loop_mode),
            ("gate", self.gate, original.gate),
            ("reverse", self.reverse, original.reverse),
        ] {
            if value != previous && value > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid pad {name} setting"),
                ));
            }
        }
        Ok(())
    }
}

impl Pad {
    fn from_record(index: usize, record: &[u8], entries: &[PathBuf]) -> io::Result<Self> {
        let bank = (b'A' + (index / PADS_PER_BANK) as u8) as char;
        let number = (index % PADS_PER_BANK + 1) as u8;
        let prefix = format!("{bank}{number:07}");
        let mut file = None;
        let mut file_size = None;
        for path in entries.iter().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    ["WAV", "AIF", "AIFF"].iter().any(|extension| {
                        name.eq_ignore_ascii_case(&format!("{prefix}.{extension}"))
                    })
                })
        }) {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if file.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("multiple sample files exist for {bank}{number}"),
                        ));
                    }
                    file_size = Some(metadata.len());
                    file = Some(path.clone());
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} is not a regular file", path.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("could not inspect {}: {error}", path.display()),
                    ));
                }
            }
        }
        let read_u32 = |offset| {
            u32::from_be_bytes([
                record[offset],
                record[offset + 1],
                record[offset + 2],
                record[offset + 3],
            ])
        };
        Ok(Self {
            bank,
            number,
            file,
            file_size,
            start: read_u32(8),
            end: read_u32(12),
            channels: record[22],
            settings: PadSettings {
                level: record[16],
                loop_mode: record[18],
                gate: record[19],
                reverse: record[20],
            },
        })
    }
}

#[derive(Debug)]
pub struct Card {
    pub root: PathBuf,
    pads: Vec<Pad>,
    info: Vec<u8>,
}

impl Card {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let sample_dir = root.join("ROLAND").join("SP-404SX").join("SMPL");
        let info_path = sample_dir.join("PAD_INFO.BIN");
        let info = fs::read(&info_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not read {}: {error}", info_path.display()),
            )
        })?;
        if info.len() != PAD_INFO_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is {} bytes; expected {PAD_INFO_SIZE}",
                    info_path.display(),
                    info.len()
                ),
            ));
        }
        let entries = fs::read_dir(&sample_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        let pads = info
            .chunks_exact(RECORD_SIZE)
            .enumerate()
            .map(|(index, record)| Pad::from_record(index, record, &entries))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { root, pads, info })
    }

    pub fn pads(&self) -> &[Pad] {
        &self.pads
    }

    pub fn pad(&self, bank: char, number: u8) -> Option<&Pad> {
        let bank = bank.to_ascii_uppercase();
        if !('A'..='J').contains(&bank) || !(1..=12).contains(&number) {
            return None;
        }
        self.pads
            .get((bank as usize - 'A' as usize) * PADS_PER_BANK + usize::from(number - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pad_record_and_filename() {
        let mut record = [0; RECORD_SIZE];
        record[8..12].copy_from_slice(&512u32.to_be_bytes());
        record[12..16].copy_from_slice(&1024u32.to_be_bytes());
        record[16..21].copy_from_slice(&[85, 9, 1, 0, 1]);
        record[22] = 2;
        let pad = Pad::from_record(24, &record, &[]).unwrap();
        assert_eq!((pad.bank, pad.number), ('C', 1));
        assert_eq!((pad.start, pad.end, pad.channels), (512, 1024, 2));
        assert_eq!(
            pad.settings,
            PadSettings {
                level: 85,
                loop_mode: 1,
                gate: 0,
                reverse: 1
            }
        );
        assert!(pad.file.is_none());
    }

    #[test]
    fn rejects_unformatted_card() {
        let error = Card::open("/nonexistent-sp404-test").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("PAD_INFO.BIN"));
    }

    #[test]
    fn opens_complete_card_and_maps_bank_and_pad() {
        let root = tempfile::tempdir().unwrap();
        let sample_dir = root.path().join("ROLAND/SP-404SX/SMPL");
        fs::create_dir_all(&sample_dir).unwrap();
        let mut info = vec![0; PAD_INFO_SIZE];
        let index = 2 * PADS_PER_BANK;
        let record = &mut info[index * RECORD_SIZE..(index + 1) * RECORD_SIZE];
        record[8..12].copy_from_slice(&512u32.to_be_bytes());
        record[12..16].copy_from_slice(&1024u32.to_be_bytes());
        record[22] = 2;
        fs::write(sample_dir.join("PAD_INFO.BIN"), info).unwrap();
        fs::write(sample_dir.join("C0000001.WAV"), b"sample").unwrap();

        let card = Card::open(root.path()).unwrap();
        assert_eq!(card.pads().len(), 120);
        let pad = card.pad('c', 1).unwrap();
        assert_eq!(pad.file_size, Some(6));
        assert_eq!((pad.start, pad.end, pad.channels), (512, 1024, 2));
        assert!(card.pad('C', 2).unwrap().file.is_none());
        assert!(card.pad('K', 1).is_none());
        assert!(card.pad('A', 0).is_none());
        assert!(card.pad('A', 13).is_none());
    }

    #[test]
    fn rejects_wrong_metadata_size() {
        let root = tempfile::tempdir().unwrap();
        let sample_dir = root.path().join("ROLAND/SP-404SX/SMPL");
        fs::create_dir_all(&sample_dir).unwrap();
        fs::write(sample_dir.join("PAD_INFO.BIN"), [0; 31]).unwrap();
        let error = Card::open(root.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("31 bytes"));
    }
}
