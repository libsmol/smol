use flate2::read::GzDecoder;
use ruzstd::decoding::StreamingDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::Archive;

/// The python-build-standalone release and the CPython it builds
const RELEASE: &str = "20261026";
const VERSION: &str = "3.14.10";
/// Each target's archive and its sha256.
/// release.yml reads the targets from here, each a quoted triple on a line of its own
const BUILDS: [(&str, &str, &str); 5] = [
    (
        "x86_64-unknown-linux-gnu",
        "pgo+lto-full.tar.zst",
        "4a2823d5deacb631e21b3c8387a1ec9be57e1b0ed37b7770e2e71cfe7b94eb94",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "pgo+lto-full.tar.zst",
        "2c2b62b6e02a1717b408c30d49a8bf293b280b2b4a22bb948f9b85ecc1f5ce54",
    ),
    (
        "x86_64-apple-darwin",
        "pgo+lto-full.tar.zst",
        "efce6e258263296b5a5476ab982b721590c7d25eac7636785a1a46d9bf1cf46b",
    ),
    (
        "aarch64-apple-darwin",
        "pgo+lto-full.tar.zst",
        "137db511241f7693ddc57586af1744c7b6375982ab4c31c4a82d10115657d462",
    ),
    (
        "x86_64-pc-windows-msvc",
        "install_only_stripped.tar.gz",
        "c1876a9795d3a94ee49a1499a69cd91e26ca3e2829593f93fa6e4db90d05402b",
    ),
];
/// The end record of a zip with no entries
const EMPTY_ZIP: [u8; 22] = [
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=pack.py");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    let target = env::var("TARGET").expect("target");
    let host = env::var("HOST").expect("host");
    let zip = out.join("stdlib.zip");
    println!("cargo:rustc-env=SMOL_STDLIB={}", zip.display());
    println!("cargo:rustc-env=SMOL_PYTHON_VERSION={VERSION}");

    // Checking a target without a build, as clippy does for windows-gnu, needs no CPython
    let built = BUILDS.iter().find(|(name, ..)| *name == target);
    let Some((_, flavor, sha)) = built.filter(|_| !target.contains("windows")) else {
        fs::write(&zip, EMPTY_ZIP).expect("empty zip");
        return;
    };

    // Shared by every profile and target folder, since the archive is large.
    // Cargo tags the target directory and each target's folder in it, and the outermost is the root
    let tagged = out
        .ancestors()
        .filter(|dir| dir.join("CACHEDIR.TAG").exists());
    let root = tagged.last().expect("target dir");
    let cache = root.join("pbs");
    let named = format!("cpython-{VERSION}+{RELEASE}-{target}");
    let folder = cache.join(&named);
    let unpacked = folder.join(".unpacked");
    if !unpacked.exists() {
        fs::create_dir_all(&cache).expect("cache");
        let archive = cache.join(format!("{named}-{flavor}"));
        let hashed = |bytes: &[u8]| -> String {
            let digest = Sha256::digest(bytes);
            let digits: Vec<String> = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            digits.concat()
        };
        let kept = fs::read(&archive)
            .ok()
            .filter(|bytes| hashed(bytes) == *sha);
        if kept.is_none() {
            let url = format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{RELEASE}/{named}-{flavor}"
            );
            let response = ureq::get(&url).call().expect("download");
            let mut body = response.into_body().into_reader();
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).expect("download");
            if hashed(&bytes) != *sha {
                panic!("{url}: checksum mismatch");
            }
            fs::write(&archive, &bytes).expect("archive");
        }

        // Unpacked anew, so one that stopped halfway never counts as done
        fs::remove_dir_all(&folder).ok();
        let file = File::open(&archive).expect("archive");
        let file = BufReader::new(file);
        let decoded: Box<dyn Read> = match flavor.ends_with(".zst") {
            true => Box::new(StreamingDecoder::new(file).expect("zstd")),
            false => Box::new(GzDecoder::new(file)),
        };
        Archive::new(decoded).unpack(&folder).expect("unpack");
        fs::write(&unpacked, "").expect("mark");
    }
    let python = folder.join("python");

    let described = python.join("PYTHON.json");
    let info = fs::read_to_string(described).expect("PYTHON.json");
    let info: Value = serde_json::from_str(&info).expect("PYTHON.json");
    let core = &info["build_info"]["core"];
    let relative = core["static_lib"].as_str().expect("static lib");
    let library = python.join(relative);

    let file = File::open(&library).expect("libpython");
    let mut archive = ar::Archive::new(file);
    let mut members = Vec::new();
    while let Some(entry) = archive.next_entry() {
        let entry = entry.expect("libpython");
        let name = String::from_utf8_lossy(entry.header().identifier());
        members.push(name.into_owned());
    }

    // The installed libpython leaves out objects that the python executable links on its own
    let mut extra = Vec::new();
    let mut statics = Vec::new();
    let mut systems = Vec::new();
    let extensions = info["build_info"]["extensions"]
        .as_object()
        .expect("extensions");
    for variants in extensions.values() {
        let variant = &variants[0];
        if !variant["shared_lib"].is_null() {
            continue;
        }
        for object in variant["objs"].as_array().into_iter().flatten() {
            let relative = object.as_str().expect("object");
            let object = python.join(relative);
            let name = object
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            if !members.contains(&name) && !extra.contains(&object) {
                extra.push(object);
            }
        }
        for link in variant["links"].as_array().into_iter().flatten() {
            let name = link["name"].as_str().expect("link name").to_owned();
            let framework = link["framework"].as_bool().unwrap_or(false);
            if let Some(path) = link["path_static"].as_str() {
                let path = python.join(path);
                if !statics.contains(&(name.clone(), path.clone())) {
                    statics.push((name, path));
                }
            } else if !systems.contains(&(name.clone(), framework)) {
                systems.push((name, framework));
            }
        }
    }
    for link in core["links"].as_array().into_iter().flatten() {
        let name = link["name"].as_str().expect("link name").to_owned();
        let framework = link["framework"].as_bool().unwrap_or(false);
        if !systems.contains(&(name.clone(), framework)) {
            systems.push((name, framework));
        }
    }

    // Renamed, since a crate may link its own copy of the same library, such as rusqlite's SQLite
    println!("cargo:rustc-link-search=native={}", out.display());
    for (name, path) in statics {
        let renamed = out.join(format!("libpy{name}.a"));
        fs::remove_file(&renamed).ok();
        if fs::hard_link(&path, &renamed).is_err() {
            fs::copy(&path, &renamed).expect("static lib");
        }
        println!("cargo:rustc-link-lib=static=py{name}");
    }

    // libpython and the objects it leaves out are LLVM bitcode, which only lld reads.
    // Compiled once into native objects, every later link stays quick and small
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let printed = Command::new(rustc).args(["--print", "sysroot"]).output();
    let printed = printed.expect("sysroot");
    let sysroot = String::from_utf8(printed.stdout).expect("sysroot");
    let lld = Path::new(sysroot.trim()).join(format!("lib/rustlib/{host}/bin/rust-lld"));
    let native = folder.join("pycore");
    if !native.exists() {
        let partial = folder.join("pycore.part");
        fs::remove_dir_all(&partial).ok();
        fs::create_dir_all(&partial).expect("pycore");
        let mut linking = Command::new(&lld);
        match target.contains("apple") {
            // ld64.lld has no -r, so a dylib links it and its LTO leaves the objects in the folder
            true => {
                let arch = match target.starts_with("aarch64") {
                    true => "arm64",
                    false => "x86_64",
                };
                let deployment = info["apple_sdk_deployment_target"].as_str();
                let deployment = deployment.expect("deployment target");
                let sdk = info["apple_sdk_version"].as_str().expect("sdk version");
                linking
                    .args(["-flavor", "darwin", "-arch", arch, "-platform_version"])
                    .args(["macos", deployment, sdk])
                    .args(["-dylib", "-undefined", "dynamic_lookup", "-all_load"])
                    .arg("-object_path_lto")
                    .arg(&partial)
                    .arg("-o")
                    .arg(folder.join("pycore.dylib"));
            }
            false => {
                linking
                    .args(["-flavor", "gnu", "-r", "--whole-archive", "-o"])
                    .arg(partial.join("pycore.o"));
            }
        };
        let status = linking
            .arg(&library)
            .args(&extra)
            .status()
            .expect("rust-lld");
        if !status.success() {
            panic!("rust-lld: {status}");
        }
        fs::remove_file(folder.join("pycore.dylib")).ok();
        fs::rename(&partial, &native).expect("pycore");
    }
    for (name, framework) in systems {
        let kind = match framework {
            true => "framework",
            false => "dylib",
        };
        println!("cargo:rustc-link-lib={kind}={name}");
    }

    // GNU ld looks up an archive only for what the files before it left undefined,
    // and the libraries don't come in that order, so Linux links with lld as well.
    // A dependency's link arguments don't reach a binary, so libsmol's build script repeats them
    let mut arguments = match target.contains("apple") {
        true => vec!["-Wl,-export_dynamic".to_owned()],
        false => {
            let tools = Path::new(sysroot.trim()).join(format!("lib/rustlib/{host}/bin/gcc-ld"));
            let tools = tools.display();
            vec![
                "-fuse-ld=lld".to_owned(),
                format!("-B{tools}"),
                "-Wl,--export-dynamic".to_owned(),
            ]
        }
    };
    // Objects link whole, keeping every C API function that extension modules pip installs
    // look up at load time. In an archive Apple's linker takes them only 8-byte aligned
    let objects = fs::read_dir(&native).expect("pycore");
    for object in objects {
        let object = object.expect("pycore").path();
        arguments.push(object.display().to_string());
    }
    for argument in &arguments {
        println!("cargo:rustc-link-arg={argument}");
    }
    println!("cargo:link_args={}", arguments.join(";"));

    let (minor, _) = VERSION.rsplit_once('.').expect("version");
    let interpreter = python.join(format!("install/bin/python{minor}"));
    let stdlib = python.join(format!("install/lib/python{minor}"));
    let manifest = env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir");
    let packing = Path::new(&manifest).join("pack.py");
    // The target's own python compiles the stdlib, which another host runs only through emulation
    let status = Command::new(&interpreter)
        .arg("-I")
        .arg(packing)
        .arg(stdlib)
        .arg(&zip)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("pack.py: {status}"),
        Err(e) => panic!(
            "{}: {e} (building for {target} on {host})",
            interpreter.display()
        ),
    }
}
