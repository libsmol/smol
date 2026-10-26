use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use zstd::stream::Encoder;

/// Where `./build.sh worker` leaves the guest, which its own workspace builds
const WORKER: &str = "../../target/worker/wasm32-wasip2/release/libsmol_worker.wasm";

fn main() {
    println!("cargo:rerun-if-changed={WORKER}");
    let wasm = match fs::read(WORKER) {
        Ok(wasm) => wasm,
        Err(e) => panic!("worker: {WORKER}: {e} (run ./build.sh worker first)"),
    };

    // The best level takes too long for every debug build
    let profile = env::var("PROFILE").unwrap_or_default();
    let level = match profile.as_str() {
        "release" => 19,
        _ => 3,
    };
    let threads = thread::available_parallelism().map_or(1, |count| count.get());
    let mut encoder = Encoder::new(Vec::new(), level).expect("zstd");
    encoder.multithread(threads as u32).expect("zstd threads");
    encoder.write_all(&wasm).expect("compress");
    let packed = encoder.finish().expect("compress");

    let out = env::var_os("OUT_DIR").expect("OUT_DIR");
    let path = PathBuf::from(out).join("worker.wasm.zst");
    fs::write(&path, packed).expect("write");

    // Names the compiled copy in the cache, so a rebuilt guest never loads an old one
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &wasm {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    println!("cargo:rustc-env=WORKER_HASH={hash:016x}");
}
