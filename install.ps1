# Usage: irm https://raw.githubusercontent.com/libsmol/smol/main/install.ps1 | iex
# The script block keeps iex from leaving variables and settings in the caller's session
& {
    $ErrorActionPreference = "Stop"
    $ProgressPreference = "SilentlyContinue"

    $repo = "libsmol/smol"
    $version = if ($env:SMOL_VERSION) { $env:SMOL_VERSION } else { "latest" }
    $bin = if ($env:SMOL_INSTALL_DIR) {
        $env:SMOL_INSTALL_DIR
    } else {
        Join-Path $HOME ".local\bin"
    }

    # Windows on ARM64 runs x64 programs too
    $arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    if ($arch -ne "X64" -and $arch -ne "Arm64") {
        throw "install: ${arch}: unsupported architecture"
    }
    $asset = "smol-x86_64-pc-windows-msvc.zip"
    $base = if ($version -eq "latest") {
        "https://github.com/$repo/releases/latest/download"
    } else {
        "https://github.com/$repo/releases/download/$version"
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $archive = Join-Path $tmp $asset
        $sums = Join-Path $tmp "SHA256SUMS"
        Write-Output "downloading $base/$asset"
        Invoke-WebRequest "$base/$asset" -OutFile $archive -UseBasicParsing
        Invoke-WebRequest "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing

        $line = Get-Content $sums |
            Where-Object { ($_ -split "\s+")[1] -eq $asset } |
            Select-Object -First 1
        if (-not $line) {
            throw "install: ${asset}: not in SHA256SUMS"
        }
        $expected = ($line -split "\s+")[0]
        $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $expected) {
            throw "install: ${asset}: checksum mismatch"
        }

        Expand-Archive $archive -DestinationPath $tmp
        $unpacked = Join-Path $tmp "smol-x86_64-pc-windows-msvc"
        New-Item -ItemType Directory -Force -Path $bin | Out-Null
        foreach ($name in "smol.exe", "libsmol.exe") {
            $installed = Join-Path $bin $name

            # A running binary can't be overwritten, so it is renamed aside
            # and deleted by a later install once nothing holds it open
            Get-ChildItem "$installed.*.old" -ErrorAction SilentlyContinue | ForEach-Object {
                try {
                    [IO.File]::Open($_.FullName, "Open", "Read", "None").Dispose()
                    Remove-Item $_.FullName -Force
                } catch {
                }
            }
            if (Test-Path $installed) {
                $stamp = Get-Date -Format "yyyyMMddHHmmss"
                Move-Item $installed "$installed.$stamp.old"
            }
            Copy-Item (Join-Path $unpacked $name) $installed
            Write-Output $installed
        }
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    # [Environment] expands entries such as %USERPROFILE%\... and stores the Path as REG_SZ, losing them for good
    # Opened for writing, and made first in a profile that lacks it
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Environment")
    try {
        $user = $key.GetValue("Path", "", "DoNotExpandEnvironmentNames")
        $paths = @($user -split ";" | Where-Object { $_ })
        $expanded = $paths | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd("\") }
        if ($expanded -notcontains $bin.TrimEnd("\")) {
            $joined = ($paths + $bin) -join ";"
            $key.SetValue("Path", $joined, "ExpandString")
            # Removing a variable that isn't set changes nothing but tells running programs to reload the environment
            [Environment]::SetEnvironmentVariable("SMOL_PATH_CHANGED", $null, "User")
            Write-Output "added $bin to your PATH; open a new terminal to run smol"
        }
    } finally {
        $key.Dispose()
    }
}
