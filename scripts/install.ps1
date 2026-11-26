# Install the native Windows client from a verified GitHub Release archive.
[CmdletBinding()]
param(
    [string]$Version = $env:SSMCP_VERSION,
    [string]$InstallDir = $env:SSMCP_INSTALL_DIR,
    [switch]$NoModifyPath,
    # Accepted for older callers; browser dependencies are no longer installed.
    [switch]$SkipBrowserDependencies
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
function Wait-DownloadTask {
    param([Threading.Tasks.Task]$Task, [Diagnostics.Stopwatch]$Clock, [int]$PhaseMs, [int]$TotalMs, [string]$Phase)
    $remaining = [Math]::Min([long]$PhaseMs, [long]$TotalMs - $Clock.ElapsedMilliseconds)
    if ($remaining -le 0 -or -not $Task.Wait([int]$remaining)) { throw "Download $Phase deadline exceeded." }
    # Wait propagates failed/cancelled tasks without waiting past this phase's deadline.
}
function Receive-BoundedStream {
    param(
        [IO.Stream]$InputStream, [string]$OutFile, [long]$MaxBytes,
        [Diagnostics.Stopwatch]$Clock, [Threading.CancellationToken]$Token,
        [int]$TotalMs = 180000, [int]$IdleMs = 10000,
        [int]$RateWindowMs = 8000, [long]$MinimumRate = 65536
    )
    if ($MaxBytes -le 0) { throw 'Download size limit must be positive.' }
    $output = [IO.File]::Open($OutFile, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $buffer = New-Object byte[] 65536
        [long]$received = 0
        [long]$windowBytes = 0
        $rateClock = [Diagnostics.Stopwatch]::StartNew()
        while ($true) {
            # Reading one extra byte detects an overflowing stream before disk write.
            $count = [int][Math]::Min([long]$buffer.Length, $MaxBytes - $received + 1)
            $read = $InputStream.ReadAsync($buffer, 0, $count, $Token)
            Wait-DownloadTask $read $Clock $IdleMs $TotalMs 'body/idle'
            $length = $read.Result
            if ($length -eq 0) { break }
            if ($received + $length -gt $MaxBytes) { throw 'Download exceeds its size limit.' }
            $output.Write($buffer, 0, $length)
            $received += $length
            $windowBytes += $length
            if ($rateClock.ElapsedMilliseconds -ge $RateWindowMs) {
                if ($windowBytes * 1000 -lt $MinimumRate * $rateClock.ElapsedMilliseconds) {
                    throw 'Download stayed below the minimum transfer rate.'
                }
                $windowBytes = 0
                $rateClock.Restart()
            }
        }
    } finally { $output.Dispose() }
}
function Invoke-BoundedDownload {
    param([string]$Url, [string]$OutFile, [long]$MaxBytes)
    Add-Type -AssemblyName System.Net.Http
    $handler = New-Object Net.Http.HttpClientHandler
    # Each route handles only public artifacts; cookies and automatic credentials stay disabled.
    $handler.AllowAutoRedirect = $false
    $handler.UseCookies = $false
    $handler.UseDefaultCredentials = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [Threading.Timeout]::InfiniteTimeSpan
    $client.DefaultRequestHeaders.UserAgent.ParseAdd('ssmcp-installer')
    $cancellation = New-Object Threading.CancellationTokenSource
    $cancellation.CancelAfter(180000)
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $response = $null
    $request = $null
    $stream = $null
    try {
        $uri = [Uri]$Url
        for ($redirects = 0; ; $redirects++) {
            if ($uri.Scheme -ne 'https' -or $uri.UserInfo) { throw 'Only public HTTPS download sources are accepted.' }
            $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, $uri)
            $headers = $client.SendAsync($request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancellation.Token)
            Wait-DownloadTask $headers $clock 8000 180000 'connection/headers'
            $response = $headers.Result
            if ([int]$response.StatusCode -in @(301, 302, 303, 307, 308)) {
                if ($redirects -ge 5 -or -not $response.Headers.Location) { throw 'Download redirect limit exceeded or Location is missing.' }
                $uri = [Uri]::new($uri, $response.Headers.Location)
                $response.Dispose()
                $response = $null
                $request.Dispose()
                $request = $null
                continue
            }
            $null = $response.EnsureSuccessStatusCode()
            $size = $response.Content.Headers.ContentLength
            if ($null -ne $size -and $size -gt $MaxBytes) { throw 'Download exceeds its declared size limit.' }
            $open = $response.Content.ReadAsStreamAsync()
            Wait-DownloadTask $open $clock 10000 180000 'first-byte'
            $stream = $open.Result
            Receive-BoundedStream $stream $OutFile $MaxBytes $clock $cancellation.Token
            return
        }
    } finally {
        # Cancel and close the network stream on size, idle, low-rate, or total failure.
        $cancellation.Cancel()
        if ($stream) { $stream.Dispose() }
        if ($response) { $response.Dispose() }
        if ($request) { $request.Dispose() }
        $client.Dispose()
        $handler.Dispose()
        $cancellation.Dispose()
    }
}
function Expand-VerifiedRelease {
    param([string]$Archive, [string]$Destination)
    # The release package has one executable. Copy that exact ZIP entry with an
    # independent expanded-byte counter, so path normalization never decides a destination.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Add-Type -AssemblyName System.IO.Compression
    if (-not [IO.Compression.ZipArchiveEntry].GetProperty('ExternalAttributes')) { throw 'Verified ZIP extraction requires .NET Framework 4.7.2 or later. Update Windows/.NET, or use PowerShell 7.' }
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        if ($zip.Entries.Count -gt 64) { throw 'Release archive contains too many entries.' }
        $entries = @($zip.Entries | Where-Object { $_.FullName -ceq 'ssmcp.exe' })
        if ($entries.Count -ne 1) { throw 'Release archive must contain exactly one root ssmcp.exe.' }
        $entry = $entries[0]
        if ((($entry.ExternalAttributes -shr 16) -band 61440) -eq 40960 -or ($entry.ExternalAttributes -band 1024) -ne 0) { throw 'Archive links are not accepted.' }
        if ($entry.Length -le 0 -or $entry.Length -gt 268435456) { throw 'Release executable exceeds its expanded size limit.' }
        [IO.Directory]::CreateDirectory($Destination) | Out-Null
        $source = $entry.Open()
        $destinationPath = Join-Path $Destination 'ssmcp.exe'
        $output = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $buffer = New-Object byte[] 65536
            [long]$written = 0
            while (($count = $source.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $written += $count
                if ($written -gt $entry.Length -or $written -gt 268435456) { throw 'Release executable exceeds its expanded size limit.' }
                $output.Write($buffer, 0, $count)
            }
            if ($written -ne $entry.Length) { throw 'Release executable is truncated.' }
        } finally {
            $output.Dispose()
            $source.Dispose()
        }
    } finally { $zip.Dispose() }
}

$pending = $null
$backup = $null
$installed = Join-Path $InstallDir 'ssmcp.exe'
try {
    [IO.Directory]::CreateDirectory($workDir) | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Write-Host "Downloading trusted checksums for $target..."
    # Mirrors receive only public binary URLs. The checksum trust root stays on GitHub.
    Invoke-BoundedDownload "$releaseUrl/SHA256SUMS" (Join-Path $workDir 'SHA256SUMS') 262144
    $pattern = '^([0-9a-fA-F]{64})  ' + [regex]::Escape($archive) + '$'
    $entries = @(Get-Content -LiteralPath (Join-Path $workDir 'SHA256SUMS') | Where-Object { $_ -match $pattern })
    if ($entries.Count -ne 1) { throw 'Release checksum entry is missing or duplicated.' }
    $expected = [regex]::Match($entries[0], $pattern).Groups[1].Value
    $archivePath = Join-Path $workDir $archive
    $verified = $false
    $lastFailure = ''
    $sources = @("$releaseUrl/$archive", "https://ghfast.top/$releaseUrl/$archive", "https://gh-proxy.com/$releaseUrl/$archive")
    foreach ($source in $sources) {
        Write-Host "Downloading public archive: $source"
        try {
            Invoke-BoundedDownload $source $archivePath 134217728
            $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
            if ($actual -ne $expected) { throw 'Checksum mismatch; rejecting this download source.' }
            $verified = $true
            break
        } catch {
            $lastFailure = $_.Exception.Message
            Write-Host "$lastFailure Trying the next source."
        }
    }
    if (-not $verified) { throw "All public download routes failed verification; existing installation was preserved. $lastFailure" }
    Expand-VerifiedRelease (Join-Path $workDir $archive) (Join-Path $workDir 'unpacked')
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
    Write-Host 'Use the controller username from the relay account page. In your project directory run:'
    Write-Host '  ssmcp USERNAME@ssmcp.lmm.best'
} finally {
    if ($pending -and (Test-Path -LiteralPath $pending)) { Remove-Item -LiteralPath $pending -Force }
    if (Test-Path -LiteralPath $workDir) { Remove-Item -LiteralPath $workDir -Recurse -Force }
}
