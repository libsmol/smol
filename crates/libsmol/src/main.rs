use libsmol::{SETUP_ARG, WORKER_ARG};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = match args[..] {
        [] => libsmol::run(),
        [SETUP_ARG] => libsmol::setup(),
        [WORKER_ARG] => libsmol::worker(),
        _ => {
            eprintln!("libsmol: usage: libsmol [setup]");
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
