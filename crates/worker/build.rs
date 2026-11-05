use ruzstd::decoding::StreamingDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
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
        "pgo-full.tar.zst",
        "7a828feea84763362f5e56031a12eee5d2697196ccf987b21ccce5ec9ad4a00f",
    ),
];
/// The end record of a zip with no entries
const EMPTY_ZIP: [u8; 22] = [
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
/// The `inittab.rs` of a target whose CPython builds every extension module in, or that has none
const NO_MODULES: &str =
    "pub(crate) static MODULES: &[(&std::ffi::CStr, unsafe extern \"C\" fn() -> *mut pyo3::ffi::PyObject)] = &[];\n";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=pack.py");
    println!("cargo:rerun-if-changed=src/windows.c");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    let target = env::var("TARGET").expect("target");
    let host = env::var("HOST").expect("host");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let windows = target.contains("windows");
    let (minor, _) = VERSION.rsplit_once('.').expect("version");
    let dll = format!("python{}", minor.replace('.', ""));

    let zip = out.join("stdlib.zip");
    let inittab = out.join("inittab.rs");
    let mut forwarder = out.join("python3.dll");
    println!("cargo:rustc-env=SMOL_STDLIB={}", zip.display());
    println!("cargo:rustc-env=SMOL_PYTHON_VERSION={VERSION}");
    println!("cargo:rustc-env=SMOL_PYTHON_NAME={dll}");

    // Checking a target without a build, as clippy does for windows-gnu, needs no CPython
    let built = BUILDS.iter().find(|(name, ..)| *name == target);
    let Some((_, flavor, sha)) = built else {
        fs::write(&zip, EMPTY_ZIP).expect("empty zip");
        fs::write(&inittab, NO_MODULES).expect("inittab");
        fs::write(&forwarder, "").expect("python3.dll");
        println!("cargo:rustc-env=SMOL_PYTHON3_DLL={}", forwarder.display());
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

    // Cargo locks each profile's folder alone, so builds of other profiles may fill the cache alongside
    fs::create_dir_all(&cache).expect("cache");
    let held = File::create(cache.join(format!("{named}.lock"))).expect("lock");
    held.lock().expect("lock");

    if !unpacked.exists() {
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
        let decoded = StreamingDecoder::new(file).expect("zstd");
        Archive::new(decoded).unpack(&folder).expect("unpack");
        fs::write(&unpacked, "").expect("mark");
    }
    let python = folder.join("python");

    let described = python.join("PYTHON.json");
    let info = fs::read_to_string(described).expect("PYTHON.json");
    let info: Value = serde_json::from_str(&info).expect("PYTHON.json");
    let core = &info["build_info"]["core"];
    let extensions = info["build_info"]["extensions"]
        .as_object()
        .expect("extensions");
    println!("cargo:rustc-link-search=native={}", out.display());

    let mut arguments = Vec::new();
    let mut systems = Vec::new();
    if windows {
        // The official build makes a DLL of the core and a .pyd of each other extension module,
        // and they all link into one image here.
        // dl_nt.obj holds a DllMain, which _ctypes defines too, and two globals windows.c defines instead
        for object in core["objs"].as_array().expect("objects") {
            let relative = object.as_str().expect("object");
            if !relative.ends_with("/dl_nt.obj") {
                let object = python.join(relative);
                arguments.push(object.display().to_string());
            }
        }

        let mut modules = Vec::new();
        // Extensions may each carry a copy of the same library, such as expat
        let mut seen = HashSet::new();
        for (module, variants) in extensions {
            let variant = &variants[0];
            // _tkinter needs Tcl and Tk, which pbs builds only as DLLs
            if variant["in_core"].as_bool() == Some(true) || module == "_tkinter" {
                continue;
            }
            let init = variant["init_fn"].as_str().expect("init function");
            modules.push((module.clone(), init.to_owned()));
            for object in variant["objs"].as_array().into_iter().flatten() {
                let object = python.join(object.as_str().expect("object"));
                let base = object.file_name().expect("name").to_string_lossy();
                if seen.insert(base.to_lowercase()) {
                    arguments.push(object.display().to_string());
                }
            }
            for link in variant["links"].as_array().into_iter().flatten() {
                let library = link["name"].as_str().expect("link name").to_owned();
                let system = link["system"].as_bool() == Some(true);
                if system && !systems.contains(&(library.clone(), false)) {
                    systems.push((library, false));
                }
            }
        }
        // The official build gets the last two from its project defaults, which PYTHON.json leaves out
        let listed = core["links"].as_array().into_iter().flatten();
        let libraries = listed.map(|link| link["name"].as_str().expect("link name"));
        for library in libraries.chain(["advapi32", "bcrypt"]) {
            let library = library.to_owned();
            if !systems.contains(&(library.clone(), false)) {
                systems.push((library, false));
            }
        }
        for library in ["zlib-ng.lib", "liblzma.lib"] {
            let library = python.join("build/lib").join(library);
            arguments.push(library.display().to_string());
        }

        // pbs builds LTCG objects on Windows, which only MSVC's linker reads
        arguments.push("/LTCG".to_owned());
        // Extensions ask for the import libraries of the DLLs they were built for, which this image replaces
        arguments.push(format!("/NODEFAULTLIB:{dll}.lib"));
        arguments.push("/NODEFAULTLIB:python3.lib".to_owned());
        // Quiets a warning for each object, which would bury a real error: pbs ships no PDB for them (4099),
        // and they and PyO3 import from the DLL what this image defines (4217, 4286)
        arguments.push("/IGNORE:4099,4217,4286".to_owned());

        let mut table = String::from("unsafe extern \"C\" {\n");
        for (_, init) in &modules {
            table.push_str(&format!("    fn {init}() -> *mut pyo3::ffi::PyObject;\n"));
        }
        table.push_str("}\n\n/// The extension modules CPython's own table leaves out, as the official build makes .pyd files of them\n");
        table.push_str("pub(crate) static MODULES: &[(&std::ffi::CStr, unsafe extern \"C\" fn() -> *mut pyo3::ffi::PyObject)] = &[\n");
        for (module, init) in &modules {
            table.push_str(&format!("    (c\"{module}\", {init}),\n"));
        }
        table.push_str("];\n");
        fs::write(&inittab, table).expect("inittab");

        let winver = format!("\"{minor}\"");
        let shims = manifest.join("src/windows.c");
        cc::Build::new()
            .file(shims)
            .define("SMOL_WINVER", winver.as_str())
            .compile("smolwin");
        // pyo3-ffi asks for pythonXY.lib, which an empty library satisfies, as the objects define everything
        let empty = out.join("empty.c");
        fs::write(&empty, "").expect("empty.c");
        cc::Build::new()
            .file(&empty)
            .cargo_metadata(false)
            .compile("pythonXY");
        forwarder = python.join("install/python3.dll");
    } else {
        fs::write(&inittab, NO_MODULES).expect("inittab");
        fs::write(&forwarder, "").expect("python3.dll");

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

        // GNU ld looks up an archive only for what the files before it left undefined,
        // and the libraries don't come in that order, so Linux links with lld as well
        match target.contains("apple") {
            true => {
                arguments.push("-Wl,-export_dynamic".to_owned());
                // The objects check the macOS version through compiler-rt, which rustc's -nodefaultlibs
                // leaves out. std defines the same function, but a release's LTO drops it as unused
                let printed = Command::new("cc").arg("-print-resource-dir").output();
                let printed = printed.expect("cc");
                let resources = String::from_utf8(printed.stdout).expect("resource dir");
                let runtime = Path::new(resources.trim()).join("lib/darwin/libclang_rt.osx.a");
                if !runtime.exists() {
                    panic!("{}: not found", runtime.display());
                }
                arguments.push(runtime.display().to_string());
            }
            false => {
                let tools =
                    Path::new(sysroot.trim()).join(format!("lib/rustlib/{host}/bin/gcc-ld"));
                let tools = tools.display();
                arguments.push("-fuse-ld=lld".to_owned());
                arguments.push(format!("-B{tools}"));
                arguments.push("-Wl,--export-dynamic".to_owned());
            }
        }
        // Objects link whole, keeping every C API function that extension modules pip installs
        // look up at load time. In an archive Apple's linker takes them only 8-byte aligned
        let objects = fs::read_dir(&native).expect("pycore");
        for object in objects {
            let object = object.expect("pycore").path();
            arguments.push(object.display().to_string());
        }
    }
    drop(held);

    for (library, framework) in systems {
        let kind = match framework {
            true => "framework",
            false => "dylib",
        };
        println!("cargo:rustc-link-lib={kind}={library}");
    }
    // A dependency's link arguments don't reach a binary, so libsmol's build script repeats them
    for argument in &arguments {
        println!("cargo:rustc-link-arg={argument}");
    }
    println!("cargo:link_args={}", arguments.join(";"));
    println!("cargo:rustc-env=SMOL_PYTHON3_DLL={}", forwarder.display());

    let (interpreter, stdlib) = match windows {
        true => (
            python.join("install/python.exe"),
            python.join("install/Lib"),
        ),
        false => (
            python.join(format!("install/bin/python{minor}")),
            python.join(format!("install/lib/python{minor}")),
        ),
    };
    let packing = manifest.join("pack.py");
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
