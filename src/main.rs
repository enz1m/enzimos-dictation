//! enzimOS dictation: press the dictation key, speak, press it again (or let
//! it go), and the words go to the clipboard and the focused text field.
//!
//! `enzimos-dictation daemon` runs per session; `start`, `stop`, `toggle`,
//! `cancel` and `status` drive it over its private control socket (the
//! desktop binds them to the dictation key). Speech is recognised by the
//! local AI service through the enzimOS AI gateway; audio never leaves the
//! machine unless the user configured a provider that takes it.

mod config;
mod consent;
mod control;
mod daemon;
mod deliver;
mod feedback;
mod record;
mod transcribe;
mod wav;

use std::io::Write as _;
use std::process::ExitCode;

const USAGE: &str = "usage: enzimos-dictation daemon | start | stop | toggle | press | release | cancel | status | allow | deny";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("enzimos-dictation: {error}");
            return ExitCode::FAILURE;
        }
    };
    match args.first().map(String::as_str) {
        Some("daemon") if args.len() == 1 => match daemon::run(&config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("enzimos-dictation: {error}");
                ExitCode::FAILURE
            }
        },
        Some(command) if args.len() == 1 && control::Command::parse(command).is_some() => {
            match control::send(&config.control, command) {
                Ok(reply) => {
                    let _ = writeln!(std::io::stdout(), "{reply}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("enzimos-dictation: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
