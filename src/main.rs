use sp404sx_wave_converter::Card;
use std::env;
use std::process::{Command, ExitCode};

#[cfg(feature = "gui")]
mod gui;

fn print_usage() {
    println!(
        "SP-404SX/A card sample manager\n\
         Usage:\n  \
         sp404sx-wave-converter gui [CARD_ROOT]\n  \
         sp404sx-wave-converter list CARD_ROOT [BANK]\n  \
         sp404sx-wave-converter play CARD_ROOT BANK PAD\n\n\
         The list and play commands are read-only. CLI playback requires ffplay and \
         previews the entire sample without pad trim settings."
    );
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("gui") => {
            if args.len() > 2 {
                return Err("gui accepts at most one card root".into());
            }
            #[cfg(feature = "gui")]
            return gui::run(args.get(1).cloned());
            #[cfg(not(feature = "gui"))]
            return Err("this build does not include the GUI".into());
        }
        Some("list") => {
            if !(2..=3).contains(&args.len()) {
                return Err("list requires a card root and optionally a bank".into());
            }
            let bank = args.get(2).map(|value| parse_bank(value)).transpose()?;
            let card = Card::open(&args[1]).map_err(|error| error.to_string())?;
            for pad in card
                .pads()
                .iter()
                .filter(|pad| bank.is_none_or(|b| b == pad.bank))
            {
                match pad.file_size {
                    Some(size) => println!(
                        "{}{:02}  {size} bytes  {} channel(s)",
                        pad.bank, pad.number, pad.channels
                    ),
                    None => println!("{}{:02}  empty", pad.bank, pad.number),
                }
            }
        }
        Some("play") => {
            if args.len() != 4 {
                return Err("play requires a card root, bank and pad number".into());
            }
            let bank = parse_bank(&args[2])?;
            let number: u8 = args[3]
                .parse()
                .map_err(|_| "pad must be a number from 1 to 12")?;
            let card = Card::open(&args[1]).map_err(|error| error.to_string())?;
            let pad = card
                .pad(bank, number)
                .ok_or("pad must be a number from 1 to 12")?;
            let file = pad.file.as_ref().ok_or("pad is empty")?;
            let status = Command::new("ffplay")
                .args(["-nodisp", "-autoexit", "-loglevel", "error"])
                .arg(file)
                .status()
                .map_err(|error| format!("could not start ffplay: {error}"))?;
            if !status.success() {
                return Err(format!("ffplay failed: {status}"));
            }
        }
        Some("--help" | "-h" | "help") => print_usage(),
        Some(command) => return Err(format!("unknown command: {command}")),
    }
    Ok(())
}

fn parse_bank(value: &str) -> Result<char, String> {
    let mut chars = value.chars();
    let bank = chars
        .next()
        .ok_or("bank must be A through J")?
        .to_ascii_uppercase();
    if chars.next().is_some() || !('A'..='J').contains(&bank) {
        return Err("bank must be A through J".into());
    }
    Ok(bank)
}

fn main() -> ExitCode {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
