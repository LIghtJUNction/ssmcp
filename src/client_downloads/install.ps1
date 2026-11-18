# Same-origin compatible release; verified user installation, then temporary join.
[CmdletBinding()]
param(
    [string]$Server,
    [Parameter(Mandatory = $true)][string]$Token,
    [string]$Name,
    [string]$InstallDir,
    [switch]$NoWatchdog,
    [ValidateRange(1,3600)][int]$ReconnectDelay = 5
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$downloadOrigin = @@ORIGIN@@
$requiredWire = @@WIRE@@
if (-not $Server) { $Server = $downloadOrigin }
if ($Server.TrimEnd('/') -cne $downloadOrigin) { throw 'The join server must match this gateway.' }
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Use the shell installation command on Linux and macOS.'
}
try { $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
catch { $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
$target = switch ($architecture.ToLowerInvariant()) {
    'x64' { 'x86_64-pc-windows-msvc' }
    'amd64' { 'x86_64-pc-windows-msvc' }
    'arm64' { 'aarch64-pc-windows-msvc' }
    default { throw 'This Windows CPU architecture is not supported.' }
}
if (-not $InstallDir) { $InstallDir = $env:SSMCP_INSTALL_DIR }
if (-not $InstallDir) {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is required for a user installation.' }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'ssmcp\bin'
}
if (-not [IO.Path]::IsPathRooted($InstallDir)) { throw 'The installation directory must be an absolute path.' }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-join-' + [Guid]::NewGuid().ToString('N'))
$binaryPath = Join-Path $temporaryDirectory 'ssmcp.exe'
$installedPath = Join-Path $InstallDir 'ssmcp.exe'
$stagePath = $null
$backupPath = $null
$handler = $null
$client = $null
$cancellation = New-Object Threading.CancellationTokenSource
$cancellation.CancelAfter(180000)

function Receive-File([string]$Uri, [string]$Path, [long]$Limit) {
    $response = $null; $source = $null; $output = $null
    try {
        $response = $client.GetAsync($Uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancellation.Token).GetAwaiter().GetResult()
        if (-not $response.IsSuccessStatusCode) {
            throw "Cannot download the compatible client release for $target. Ask the gateway administrator to configure client_download_dir."
        }
        $declaredLength = $response.Content.Headers.ContentLength
        if ($null -ne $declaredLength -and ($declaredLength -le 0 -or $declaredLength -gt $Limit)) {
            throw 'The download has an invalid size.'
        }
        $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $output = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $buffer = New-Object byte[] 65536
        [long]$received = 0
        while (($count = $source.ReadAsync($buffer, 0, $buffer.Length, $cancellation.Token).GetAwaiter().GetResult()) -gt 0) {
            $received += $count
            if ($received -gt $Limit) { throw 'The download exceeds its size limit.' }
            $output.Write($buffer, 0, $count)
        }
        if ($received -eq 0 -or ($null -ne $declaredLength -and $received -ne $declaredLength)) {
            throw 'The download is empty or incomplete.'
        }
        return $received
    } finally {
        if ($output) { $output.Dispose() }
        if ($source) { $source.Dispose() }
        if ($response) { $response.Dispose() }
    }
}

function Ensure-InstallDirectory([string]$Path) {
    $rootPath = [IO.Path]::GetPathRoot($Path)
    $current = $rootPath
    foreach ($component in $Path.Substring($rootPath.Length).Split([char[]]@('\','/'), [StringSplitOptions]::RemoveEmptyEntries)) {
        $current = Join-Path $current $component
        if (Test-Path -LiteralPath $current) {
            $attributes = [IO.File]::GetAttributes($current)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
                ($attributes -band [IO.FileAttributes]::Directory) -eq 0) {
                throw 'The installation directory must not contain symlinks, junctions or non-directories.'
            }
        } else {
            [IO.Directory]::CreateDirectory($current) | Out-Null
        }
    }
}

try {
    [IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
    Add-Type -AssemblyName System.Net.Http
    $handler = New-Object Net.Http.HttpClientHandler
    $handler.AllowAutoRedirect = $false
    $handler.UseCookies = $false
    $handler.UseDefaultCredentials = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(180)
    $manifestPath = Join-Path $temporaryDirectory 'release'
    Receive-File "$downloadOrigin/client/release/$target" $manifestPath 16384 | Out-Null
    $fields = [IO.File]::ReadAllLines($manifestPath)
    if ($fields.Length -ne 5 -or $fields[0] -cne 'ssmcp-release-v1') { throw 'The gateway returned an invalid client release manifest.' }
    $version = $fields[1]
    if ($version.Length -gt 64 -or $version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') { throw 'Invalid client release version.' }
    if ($fields[2] -cne $requiredWire.ToString()) { throw 'The published client is incompatible with this gateway.' }
    [long]$size = 0
    if ($fields[3] -cnotmatch '^[0-9]{1,9}$' -or -not [long]::TryParse($fields[3], [ref]$size) -or $size -le 0 -or $size -gt 268435456) { throw 'Invalid client release size.' }
    $digest = $fields[4]
    if ($digest -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid client release SHA-256.' }
    $received = Receive-File "$downloadOrigin/client/bin/$target" $binaryPath $size
    if ($received -ne $size) { throw 'The downloaded client is incomplete or changed during publication.' }
    $actual = (Get-FileHash -LiteralPath $binaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $digest) { throw 'Client SHA-256 mismatch; the existing installation was not changed.' }
    $reported = (& $binaryPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -cne "ssmcp $version") { throw 'Client version mismatch; the existing installation was not changed.' }
    Ensure-InstallDirectory $InstallDir
    if (Test-Path -LiteralPath $installedPath) {
        $attributes = [IO.File]::GetAttributes($installedPath)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            ($attributes -band [IO.FileAttributes]::Directory) -ne 0) {
            throw 'Refusing to replace a symlink or directory at the client installation path.'
        }
    }
    $stagePath = Join-Path $InstallDir ('.ssmcp.new.' + [Guid]::NewGuid().ToString('N'))
    [IO.File]::Copy($binaryPath, $stagePath, $false)
    if ([IO.File]::Exists($installedPath)) {
        $backupPath = Join-Path $InstallDir ('.ssmcp.previous.' + [Guid]::NewGuid().ToString('N'))
        [IO.File]::Replace($stagePath, $installedPath, $backupPath, $true)
    } else {
        [IO.File]::Move($stagePath, $installedPath)
    }
    $stagePath = $null
    if ($backupPath -and [IO.File]::Exists($backupPath)) { [IO.File]::Delete($backupPath); $backupPath = $null }
    Write-Host "Installed ssmcp $version at $installedPath"
    Write-Host 'Starting the authorized temporary session. Keep this terminal open; Ctrl-C stops it.'
    $joinArguments = @('join', '--server', $Server, '--token', $Token)
    if ($Name) { $joinArguments += @('--name', $Name) }
    if ($NoWatchdog) { $joinArguments += '--no-watchdog' }
    else { $joinArguments += @('--reconnect-delay', $ReconnectDelay.ToString()) }
    & $installedPath @joinArguments
    if ($LASTEXITCODE -ne 0) { throw "The temporary session exited with code $LASTEXITCODE." }
} finally {
    $cancellation.Cancel()
    if ($client) { $client.Dispose() }
    elseif ($handler) { $handler.Dispose() }
    $cancellation.Dispose()
    if ($stagePath -and [IO.File]::Exists($stagePath)) { Remove-Item -LiteralPath $stagePath -Force }
    # Keep a replacement backup if an unexpected filesystem error interrupted it.
    if (Test-Path -LiteralPath $temporaryDirectory) { Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force }
}
