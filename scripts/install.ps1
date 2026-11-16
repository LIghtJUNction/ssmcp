# Install the native Windows client from a verified GitHub Release archive.
[CmdletBinding()]
param(
    [string]$Version = $env:SSMCP_VERSION,
    [string]$InstallDir = $env:SSMCP_INSTALL_DIR,
    [switch]$NoModifyPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Use install.sh on Linux and macOS.'
}
if (-not $Version) { $Version = 'latest' }
if ($Version -ne 'latest') {
    if ($Version -notmatch '^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
        throw 'Version must be vMAJOR.MINOR.PATCH.'
    }
    $Version = 'v' + $Version.TrimStart('v')
}
if (-not $InstallDir) {
    $InstallDir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'ssmcp\bin'
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
try {
    $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
} catch {
    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
}
$target = switch ($architecture.ToLowerInvariant()) {
    'x64' { 'x86_64-pc-windows-msvc' }
    'amd64' { 'x86_64-pc-windows-msvc' }
    'arm64' { 'aarch64-pc-windows-msvc' }
    default { throw "Unsupported Windows architecture: $architecture" }
}
$archive = "ssmcp-$target.zip"
$releaseUrl = 'https://github.com/LIghtJUNction/ssmcp/releases'
if ($Version -eq 'latest') { $releaseUrl += '/latest/download' } else { $releaseUrl += "/download/$Version" }
$workDir = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-install-' + [Guid]::NewGuid().ToString('N'))
$pending = $null
$backup = $null
$installed = Join-Path $InstallDir 'ssmcp.exe'
try {
    [IO.Directory]::CreateDirectory($workDir) | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Write-Host "Downloading ssmcp for $target..."
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 300 -Uri "$releaseUrl/SHA256SUMS" -OutFile (Join-Path $workDir 'SHA256SUMS')
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 300 -Uri "$releaseUrl/$archive" -OutFile (Join-Path $workDir $archive)
    $pattern = '^([0-9a-fA-F]{64})  ' + [regex]::Escape($archive) + '$'
    $entries = @(Get-Content -LiteralPath (Join-Path $workDir 'SHA256SUMS') | Where-Object { $_ -match $pattern })
    if ($entries.Count -ne 1) { throw 'Release checksum entry is missing or duplicated.' }
    $expected = [regex]::Match($entries[0], $pattern).Groups[1].Value
    $actual = (Get-FileHash -LiteralPath (Join-Path $workDir $archive) -Algorithm SHA256).Hash
    if ($actual -ne $expected) { throw 'Download checksum mismatch; existing installation was preserved.' }
    Expand-Archive -LiteralPath (Join-Path $workDir $archive) -DestinationPath (Join-Path $workDir 'unpacked')
    $binary = Join-Path $workDir 'unpacked\ssmcp.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw 'Release archive is missing ssmcp.exe.' }
    $reported = (& $binary --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -notmatch '^ssmcp [0-9]+\.[0-9]+\.[0-9]+$') {
        throw 'Downloaded executable cannot run on this system.'
    }
    if ($Version -ne 'latest' -and $reported -ne ('ssmcp ' + $Version.TrimStart('v'))) {
        throw 'Executable version differs from the requested release.'
    }
    [IO.Directory]::CreateDirectory($InstallDir) | Out-Null
    $pending = Join-Path $InstallDir ('.ssmcp-' + [Guid]::NewGuid().ToString('N') + '.exe')
    Copy-Item -LiteralPath $binary -Destination $pending
    if (Test-Path -LiteralPath $installed) {
        if (-not (Test-Path -LiteralPath $installed -PathType Leaf)) { throw 'Installation path is a directory.' }
        $backup = Join-Path $InstallDir ('.ssmcp-previous-' + [Guid]::NewGuid().ToString('N') + '.exe')
        Move-Item -LiteralPath $installed -Destination $backup
    }
    try {
        Move-Item -LiteralPath $pending -Destination $installed
        $pending = $null
    } catch {
        if ($backup -and -not (Test-Path -LiteralPath $installed)) {
            Move-Item -LiteralPath $backup -Destination $installed
            $backup = $null
        }
        throw
    }
    if ($backup) {
        try { Remove-Item -LiteralPath $backup -Force } catch {
            Write-Host "Previous binary retained at $backup; remove it after the running client exits."
        }
    }
    if (-not $NoModifyPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($InstallDir -notin @($userPath -split ';')) {
            [Environment]::SetEnvironmentVariable('Path', ($InstallDir + ';' + $userPath).TrimEnd(';'), 'User')
        }
        if ($InstallDir -notin @($env:Path -split ';')) { $env:Path = "$InstallDir;$env:Path" }
    }
    Write-Host "Installed $reported -> $installed"
    Write-Host 'Connect from your project directory: ssmcp user@host'
} finally {
    if ($pending -and (Test-Path -LiteralPath $pending)) { Remove-Item -LiteralPath $pending -Force }
    if (Test-Path -LiteralPath $workDir) { Remove-Item -LiteralPath $workDir -Recurse -Force }
}
