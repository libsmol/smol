$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$bin = Join-Path $HOME ".local\bin"

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

# libsmol looks for the worker module of its own version here,
# and a worker that still has the old one loaded keeps it until it exits
$config = if ($env:SMOL_CONFIG_DIR) { $env:SMOL_CONFIG_DIR } else { Join-Path $HOME ".smol" }
$store = Join-Path $config "runtime"
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value
$module = Join-Path $store "worker-$version.dll"
New-Item -ItemType Directory -Force -Path $store | Out-Null
if (Test-Path $module) {
    $stamp = Get-Date -Format "yyyyMMddHHmmss"
    Move-Item $module "$module.$stamp.old"
}
Copy-Item "target\release\_smol.dll" $module
Write-Output $module

$paths = $env:PATH -split ";"
if ($paths -notcontains $bin) {
    Write-Output "$bin is not in PATH; add it to run smol directly"
}
