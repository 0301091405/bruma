# Installs or updates Bruma from PowerShell:
#
#   irm https://raw.githubusercontent.com/0301091405/bruma/main/install.ps1 | iex
#
# It downloads the setup program of the latest release, checks it against the
# release's checksum, and runs it without questions: for the current user, no
# administrator rights, started with Windows. Then it starts Bruma.
#
# The result is the same as running bruma-setup.exe by hand, uninstaller
# included. Remove it from "Installed apps" in Windows Settings.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ([Environment]::OSVersion.Version.Build -lt 17763 -or -not [Environment]::Is64BitOperatingSystem) {
    throw 'Bruma needs 64-bit Windows 10 (version 1809 or later) or Windows 11.'
}

# BRUMA_INSTALL_FROM points the script at a folder instead of the release
# page; the tests use it.
$from = if ($env:BRUMA_INSTALL_FROM) { $env:BRUMA_INSTALL_FROM } else { 'https://github.com/0301091405/bruma/releases/latest/download' }
$work = Join-Path $env:TEMP "bruma-install-$PID"
New-Item -ItemType Directory -Force $work | Out-Null

function Get-File($name) {
    $to = Join-Path $work $name
    if ($from -match '^https://') {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri "$from/$name" -OutFile $to
    } else {
        Copy-Item (Join-Path $from $name) $to
    }
    $to
}

try {
    Write-Host 'Downloading Bruma...'
    $sums = Get-Content (Get-File 'SHA256SUMS.txt')
    $setup = Get-File 'bruma-setup.exe'
    $want = ($sums | Where-Object { $_ -match '\sbruma-setup\.exe\s*$' } | Select-Object -First 1) -split '\s+' | Select-Object -First 1
    $have = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
    if (-not $want -or $want -ne $have) { throw 'The download does not match its checksum; nothing was installed.' }

    Write-Host 'Installing...'
    $run = Start-Process $setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=autostart' -Wait -PassThru
    if ($run.ExitCode -ne 0) { throw "Setup failed with exit code $($run.ExitCode)." }

    $exe = Join-Path $env:LOCALAPPDATA 'Programs\Bruma\bruma.exe'
    Start-Process $exe
    Write-Host 'Bruma is installed and running. Press PrintScreen to take a screenshot.'
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
