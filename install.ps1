# Installs lxw on Windows:
#   irm https://raw.githubusercontent.com/davutac/lxw-cli/main/install.ps1 | iex
# Installs the latest release, or $env:LXW_VERSION (e.g. 0.2.0), into
# %LOCALAPPDATA%\Programs\lxw, or $env:LXW_INSTALL_DIR, after checking the binary
# against the release's SHA256SUMS, and adds that folder to your user PATH.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $releases = 'https://github.com/davutac/lxw-cli/releases'
    $base = if ($env:LXW_VERSION) { "$releases/download/v$($env:LXW_VERSION.TrimStart('v'))" } else { "$releases/latest/download" }
    $arch = if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'arm64' } else { 'amd64' }
    $asset = "lxw-windows-$arch.exe"
    $dir = if ($env:LXW_INSTALL_DIR) { $env:LXW_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\lxw' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) "lxw-install-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile "$tmp\$asset"
        Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
        $expected = Get-Content "$tmp\SHA256SUMS" | ForEach-Object { $hash, $name = -split $_; if ($name -eq $asset) { $hash } }
        if (-not $expected -or (Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash -ne $expected) {
            throw "lxw: checksum mismatch for $asset; not installed"
        }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Move-Item -Force "$tmp\$asset" (Join-Path $dir 'lxw.exe')
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', ((@($userPath, $dir) | Where-Object { $_ }) -join ';'), 'User')
        $env:Path = "$env:Path;$dir"
        Write-Host "Added $dir to your user PATH (new terminals pick it up)."
    }
    Write-Host "Installed $(& (Join-Path $dir 'lxw.exe') --version) to $dir\lxw.exe"
}
