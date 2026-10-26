# Usage: .\build.ps1 [worker], where worker builds only the worker
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$bin = Join-Path $HOME ".local\bin"
# The worker links this wasi-libc until a wasi-sdk release has the fixes it carries
$sysroot = "smol-sysroot-34.0-2"
$sysrootSha256 = "20aab766330e1882683b3cc352e370c76156d6a966d4a0be044e219d0e03231e"

$marker = "target\wasi-sysroot\release"
$current = if (Test-Path $marker) { (Get-Content $marker -Raw).Trim() } else { "" }
if ($current -ne $sysroot) {
    $archive = "target\$sysroot.tar.gz"
    New-Item -ItemType Directory -Force -Path target | Out-Null
    Invoke-WebRequest "https://github.com/libsmol/wasi-libc/releases/download/$sysroot/$sysroot.tar.gz" -OutFile $archive
    $sum = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
    if ($sum -ne $sysrootSha256) {
        throw "build: ${archive}: sha256 mismatch"
    }

    # Cargo doesn't track the sysroot's files, so the worker would keep the old one
    Remove-Item target\worker, target\wasi-sysroot -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path target\wasi-sysroot | Out-Null
    tar -xzf $archive -C target\wasi-sysroot --strip-components 1
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
    Set-Content $marker $sysroot
    Remove-Item $archive
}

# The LLVM installer leaves its directory out of PATH unless asked to add it
if (-not $env:CC_wasm32_wasip2) {
    $targets = try { clang --print-targets 2>$null } catch { "" }
    if (-not ($targets -match "wasm32")) {
        $clang = Join-Path $env:ProgramFiles "LLVM\bin\clang.exe"
        $targets = try { & $clang --print-targets 2>$null } catch { "" }
        if (-not ($targets -match "wasm32")) {
            throw "build: clang: no wasm32 target (install LLVM with winget install LLVM.LLVM)"
        }
        $env:CC_wasm32_wasip2 = $clang
    }
}

# Cargo and rustup read the worker's config and toolchain from the current directory
Push-Location crates\worker
cargo build --release
$built = $LASTEXITCODE
Pop-Location
if ($built -ne 0) {
    exit $built
}
if ($args[0] -eq "worker") {
    exit 0
}

cargo build --release
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$files = @("smol.exe", "libsmol.exe") | ForEach-Object { "target\release\$_" }

New-Item -ItemType Directory -Force -Path $bin | Out-Null
foreach ($file in $files) {
    $target = Join-Path $bin (Split-Path $file -Leaf)

    # A running binary can't be overwritten, so it is renamed aside
    # and deleted by a later install once nothing holds it open
    Get-ChildItem "$target.*.old" -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            [IO.File]::Open($_.FullName, "Open", "Read", "None").Dispose()
            Remove-Item $_.FullName -Force
        } catch {
        }
    }
    if (Test-Path $target) {
        $stamp = Get-Date -Format "yyyyMMddHHmmss"
        Move-Item $target "$target.$stamp.old"
    }
    Copy-Item $file $target
    Write-Output $target
}

$paths = $env:PATH -split ";"
if ($paths -notcontains $bin) {
    Write-Output "$bin is not in PATH; add it to run smol directly"
}
