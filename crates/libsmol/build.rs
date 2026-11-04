use std::env;

fn main() {
    // The linker and the exports the static CPython needs, which the worker crate's build
    // script can't hand a binary itself
    let arguments = env::var("DEP_WORKER_LINK_ARGS").unwrap_or_default();
    for argument in arguments.split(';').filter(|argument| !argument.is_empty()) {
        println!("cargo:rustc-link-arg={argument}");
    }
}
