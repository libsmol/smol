use libsmol::SETUP_ARG;
use std::env;
use std::ffi::OsString;
use std::process::{self, ExitCode};

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    if let Some(status) = libsmol::embedded(&args) {
        process::exit(status);
    }

    // An argument that isn't UTF-8 is no known one, which `env::args` would panic on
    let given = args.iter().skip(1).map(|arg| arg.to_str().unwrap_or(""));
    let given: Vec<&str> = given.collect();
    let ran = match given[..] {
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
