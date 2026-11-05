use libsmol::SETUP_ARG;
use std::env;
use std::ffi::OsString;
use std::process::{self, ExitCode};

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    if let Some(status) = libsmol::embedded(&args) {
        process::exit(status);
    }

    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = match args[..] {
        [] => libsmol::run(),
        ["--version" | "-V"] => {
            println!("libsmol {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        [SETUP_ARG] => libsmol::setup(),
        _ => {
            eprintln!("libsmol: usage: libsmol [setup | --version]");
            return ExitCode::from(2);
        }
    };

    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("libsmol: {error}");
            ExitCode::FAILURE
        }
    }
}
