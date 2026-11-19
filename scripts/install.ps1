# Install the native Windows client from a verified GitHub Release archive.
[CmdletBinding()]
param(
    [string]$Version = $env:SSMCP_VERSION,
    [string]$InstallDir = $env:SSMCP_INSTALL_DIR,
    [switch]$NoModifyPath,
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

# BEGIN SSMCP WINDOWS BROWSER DEPENDENCIES
# Kept identical in both self-contained Windows installers; the fixture checks this.
function Invoke-SsmcpBrowserCommand {
    param([string]$File, [string[]]$Arguments, [int]$TimeoutSeconds = 60)
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $File
    # ProcessStartInfo.ArgumentList is unavailable in Windows PowerShell 5.1.
    $start.Arguments = (@($Arguments | ForEach-Object {
        '"' + ($_ -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
    }) -join ' ')
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $start.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $start.EnvironmentVariables.Remove('NODE_OPTIONS')
    $start.EnvironmentVariables.Remove('NODE_PATH')
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw "Cannot start $File." }
        $process.StandardInput.Close()
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill() } catch { }
            throw "Browser dependency command timed out: $File"
        }
        $text = $output.GetAwaiter().GetResult()
        $errorText = $errors.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            if ($errorText.Length -gt 2048) { $errorText = $errorText.Substring(0, 2048) }
            throw "Browser dependency command failed (exit $($process.ExitCode)): $File. $errorText"
        }
        if ($text.Length -gt 1048576) { throw 'Browser dependency command returned excessive output.' }
        return $text.Trim()
    } finally { $process.Dispose() }
}

function Test-SsmcpNativeFile([string]$Path) {
    if (-not $Path -or -not [IO.Path]::IsPathRooted($Path) -or -not [IO.File]::Exists($Path)) { return $false }
    $file = [IO.File]::OpenRead($Path)
    try { return ($file.ReadByte() -eq 77 -and $file.ReadByte() -eq 90) }
    finally { $file.Dispose() }
}

function Test-SsmcpSamePath([string]$Left, [string]$Right) {
    if (-not $Left -or -not $Right) { return $false }
    $paths = @($Left, $Right) | ForEach-Object {
        $value = [IO.Path]::GetFullPath($_)
        if ($value.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { $value = '\\' + $value.Substring(8) }
        elseif ($value.StartsWith('\\?\', [StringComparison]::Ordinal)) { $value = $value.Substring(4) }
        $value
    }
    return [StringComparer]::OrdinalIgnoreCase.Equals($paths[0], $paths[1])
}

function Get-SsmcpUsableNode([string]$Path) {
    if (-not (Test-SsmcpNativeFile $Path)) { return $null }
    try {
        $probe = Invoke-SsmcpBrowserCommand $Path @('--jitless', '--no-addons', '--max-old-space-size=32', '--max-semi-space-size=1', '--stack-size=256', '--eval', 'console.log(JSON.stringify({version:process.versions.node,executable:process.execPath}))')
        $node = $probe | ConvertFrom-Json
        if ($node.version -notmatch '^(22|24|26)\.[0-9]+\.[0-9]+$' -or -not (Test-SsmcpNativeFile $node.executable)) { return $null }
        $npm = Join-Path (Split-Path $node.executable -Parent) 'node_modules/npm/bin/npm-cli.js'
        if (-not [IO.File]::Exists($npm)) { return $null }
        $null = Invoke-SsmcpBrowserCommand $node.executable @($npm, '--version')
        return $node
    } catch { return $null }
}

function Ensure-SsmcpBrowserDirectory([string]$Path) {
    $root = [IO.Path]::GetPathRoot($Path)
    $current = $root
    foreach ($part in $Path.Substring($root.Length).Split([char[]]@('\','/'), [StringSplitOptions]::RemoveEmptyEntries)) {
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $attributes = [IO.File]::GetAttributes($current)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or ($attributes -band [IO.FileAttributes]::Directory) -eq 0) {
                throw 'The browser dependency directory must not contain links or non-directories.'
            }
        } else { [IO.Directory]::CreateDirectory($current) | Out-Null }
    }
}

function Expand-SsmcpNodeArchive {
    param([string]$Archive, [string]$Destination, [string]$RootName)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        if ($zip.Entries.Count -gt 20000) { throw 'Node.js archive contains too many entries.' }
        [long]$expanded = 0
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            $parts = $name.TrimEnd('/').Split('/')
            if ($name.Contains('\') -or $parts[0] -cne $RootName -or $parts.Count -lt 1) { throw 'Unsafe Node.js archive path.' }
            foreach ($part in $parts) {
                if (-not $part -or $part -in @('.', '..') -or $part -match '[ .]$|[<>:"|?*\x00-\x1f]' -or $part -match '^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\..*)?$') { throw 'Unsafe Node.js archive path.' }
            }
            if ((($entry.ExternalAttributes -shr 16) -band 61440) -eq 40960 -or ($entry.ExternalAttributes -band 1024) -ne 0) { throw 'Node.js archive links are not accepted.' }
            $expanded += $entry.Length
            if ($entry.Length -lt 0 -or $expanded -gt 536870912) { throw 'Node.js archive exceeds its expanded size limit.' }
            $path = Join-Path $Destination ($parts -join [IO.Path]::DirectorySeparatorChar)
            if ($name.EndsWith('/')) { [IO.Directory]::CreateDirectory($path) | Out-Null; continue }
            [IO.Directory]::CreateDirectory((Split-Path $path -Parent)) | Out-Null
            $archiveInput = $entry.Open()
            $output = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try {
                $buffer = New-Object byte[] 65536
                [long]$written = 0
                while (($count = $archiveInput.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $written += $count
                    if ($written -gt $entry.Length) { throw 'Node.js archive entry exceeds its declared size.' }
                    $output.Write($buffer, 0, $count)
                }
                if ($written -ne $entry.Length) { throw 'Node.js archive entry is incomplete.' }
            } finally { $output.Dispose(); $archiveInput.Dispose() }
        }
    } finally { $zip.Dispose() }
}

function Get-SsmcpUserEnvironment([string]$Name) { return [Environment]::GetEnvironmentVariable($Name, 'User') }
function Set-SsmcpUserEnvironment([string]$Name, [string]$Value) { [Environment]::SetEnvironmentVariable($Name, $Value, 'User') }

function Get-SsmcpPlaywright([string]$Node, [string]$Directory, [string]$Version) {
    if (-not [IO.File]::Exists((Join-Path $Directory 'node_modules/playwright/cli.js'))) { return $null }
    $probe = 'const p=require("node:path");const r=require("node:module").createRequire(p.join(process.argv[1],"package.json"));const version=r("playwright/package.json").version;if(version!==process.argv[2])throw new Error("Playwright version mismatch");console.log(JSON.stringify({version,cli:p.join(p.dirname(r.resolve("playwright/package.json")),"cli.js"),executable:r("playwright").chromium.executablePath()}));'
    try { return (Invoke-SsmcpBrowserCommand $Node @('--eval', $probe, $Directory, $Version)) | ConvertFrom-Json }
    catch { return $null }
}

function Install-SsmcpBrowserDependencies {
    param([string]$ClientPath, [string]$Architecture, [string]$TemporaryDirectory, [scriptblock]$Download)
    $originalPath = $env:Path
    $originalNode = $env:SSMCP_NODE
    $originalBrowser = $env:SSMCP_BROWSER_EXECUTABLE
    $playwrightVersion = '1.65.1'
    $completed = $false
    try {
        $requestedNode = $originalNode
        if (-not $requestedNode) { $requestedNode = Get-SsmcpUserEnvironment 'SSMCP_NODE' }
        $node = $null
        if ($requestedNode) {
            $node = Get-SsmcpUsableNode $requestedNode
            if (-not $node) { throw 'SSMCP_NODE must select a working native Node.js 22, 24 or 26 installation with adjacent npm. Fix or remove this explicit override before retrying.' }
        } else {
            $command = Get-Command node.exe -CommandType Application -ErrorAction SilentlyContinue
            if ($command) { $node = Get-SsmcpUsableNode $command.Source }
        }
        if (-not $node) {
            if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is required for the user-local Node.js installation.' }
            $nodeArch = switch ($Architecture.ToLowerInvariant()) { 'x64' { 'x64' } 'amd64' { 'x64' } 'arm64' { 'arm64' } default { throw 'Unsupported Node.js architecture.' } }
            Write-Host 'Installing the official Node.js 24 LTS distribution with npm...'
            $sums = Join-Path $TemporaryDirectory 'node-SHASUMS256.txt'
            & $Download 'https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt' $sums 262144 | Out-Null
            $pattern = '^([0-9a-f]{64})  (node-v24\.[0-9]+\.[0-9]+-win-' + $nodeArch + '\.zip)$'
            $checksumLines = @([IO.File]::ReadAllLines($sums) | Where-Object { $_ -cmatch $pattern })
            if ($checksumLines.Count -ne 1) { throw 'The official Node.js checksum entry is missing or duplicated.' }
            $match = [regex]::Match($checksumLines[0], $pattern)
            $filename = $match.Groups[2].Value
            $rootName = $filename.Substring(0, $filename.Length - 4)
            $nodeVersion = [regex]::Match($filename, '^node-(v24\.[0-9]+\.[0-9]+)-').Groups[1].Value
            $archive = Join-Path $TemporaryDirectory $filename
            & $Download "https://nodejs.org/dist/$nodeVersion/$filename" $archive 134217728 | Out-Null
            if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $match.Groups[1].Value) { throw 'Node.js SHA-256 mismatch; the archive was not executed.' }
            $unpacked = Join-Path $TemporaryDirectory 'node-unpacked'
            Expand-SsmcpNodeArchive $archive $unpacked $rootName
            $stagedNode = Get-SsmcpUsableNode (Join-Path $unpacked "$rootName/node.exe")
            if (-not $stagedNode) { throw 'The downloaded Node.js or bundled npm cannot run on this Windows system.' }
            $runtime = Join-Path $env:LOCALAPPDATA 'ssmcp/runtime'
            Ensure-SsmcpBrowserDirectory $runtime
            $destination = Join-Path $runtime $rootName
            if (Test-Path -LiteralPath $destination) {
                Ensure-SsmcpBrowserDirectory $destination
            } else { [IO.Directory]::Move((Join-Path $unpacked $rootName), $destination) }
            $node = Get-SsmcpUsableNode (Join-Path $destination 'node.exe')
            if (-not $node) { throw "The managed Node.js installation is unusable: $destination" }
        }
        $env:SSMCP_NODE = $node.executable
        $nodeDirectory = Split-Path $node.executable -Parent
        $env:Path = "$nodeDirectory;$originalPath"
        Write-Host "Using Node.js $($node.version): $($node.executable)"
        if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is required for the user-local Playwright package.' }
        $packageDirectory = Join-Path $env:LOCALAPPDATA "ssmcp/browser/playwright-$playwrightVersion"
        Ensure-SsmcpBrowserDirectory $packageDirectory
        $playwright = Get-SsmcpPlaywright $node.executable $packageDirectory $playwrightVersion
        if (-not $playwright) {
            Write-Host "Installing Playwright $playwrightVersion for the native browser tools..."
            $npm = Join-Path $nodeDirectory 'node_modules/npm/bin/npm-cli.js'
            $null = Invoke-SsmcpBrowserCommand $node.executable @($npm, 'install', '--prefix', $packageDirectory, '--no-audit', '--no-fund', '--ignore-scripts', '--omit=dev', '--save-exact', "playwright@$playwrightVersion") 360
            $playwright = Get-SsmcpPlaywright $node.executable $packageDirectory $playwrightVersion
        }
        if (-not $playwright -or $playwright.version -cne $playwrightVersion) { throw 'The pinned Playwright package did not pass its local verification.' }
        Write-Host 'Installing or reusing the matching Playwright Chromium browser...'
        $null = Invoke-SsmcpBrowserCommand $node.executable @($playwright.cli, 'install', 'chromium') 900
        $browser = $originalBrowser
        if (-not $browser) { $browser = Get-SsmcpUserEnvironment 'SSMCP_BROWSER_EXECUTABLE' }
        if (-not $browser) { $browser = $playwright.executable }
        if (-not (Test-SsmcpNativeFile $playwright.executable)) { throw 'Playwright Chromium installation did not produce its native browser executable.' }
        if (-not (Test-SsmcpNativeFile $browser)) { throw 'SSMCP_BROWSER_EXECUTABLE must name an existing native browser. Fix or remove this explicit override before retrying.' }
        # A fresh headless profile verifies the exact selected executable without opening a user window.
        $smoke = 'const r=require("node:module").createRequire(require("node:path").join(process.argv[1],"package.json"));const requested=process.argv[2],executable=require("node:fs").realpathSync.native(requested);const timer=setTimeout(()=>{console.error("Chromium verification timed out");process.exit(1)},45000);(async()=>{let b;try{b=await r("playwright").chromium.launch({executablePath:executable,headless:true,timeout:30000});console.log(JSON.stringify({requested,executable,version:b.version()}));}finally{if(b)await b.close();clearTimeout(timer)}})().catch(e=>{console.error(e.message);process.exit(1)});'
        $verified = (Invoke-SsmcpBrowserCommand $node.executable @('--eval', $smoke, $packageDirectory, $browser) 60) | ConvertFrom-Json
        if (-not $verified.version -or -not (Test-SsmcpSamePath $verified.requested $browser) -or -not (Test-SsmcpNativeFile $verified.executable)) { throw 'Chromium verification returned an unexpected executable.' }
        $browser = $verified.executable
        # The runtime must resolve these paths without relying on a changed PATH.
        $env:SSMCP_BROWSER_EXECUTABLE = $browser
        $env:Path = $originalPath
        $runtimeReport = (Invoke-SsmcpBrowserCommand $ClientPath @('setup', 'browser', '--native', '--check', '--json') 120) | ConvertFrom-Json
        if (-not $runtimeReport.PSObject.Properties['runtime']) {
            throw 'This ssmcp client cannot verify the selected persistent Node.js and Chromium paths. Publish an updated Windows client before using this installer.'
        }
        if (-not (Test-SsmcpSamePath $runtimeReport.runtime.node $node.executable) -or
            -not (Test-SsmcpSamePath $runtimeReport.runtime.browser_executable $browser)) {
            throw ('The client did not resolve the selected SSMCP_NODE / SSMCP_BROWSER_EXECUTABLE paths: ' + ($runtimeReport.runtime | ConvertTo-Json -Compress))
        }
        $changes = [ordered]@{ SSMCP_NODE = $node.executable; SSMCP_BROWSER_EXECUTABLE = $browser }
        $before = @{}
        try {
            foreach ($name in $changes.Keys) {
                $before[$name] = Get-SsmcpUserEnvironment $name
                Set-SsmcpUserEnvironment $name $changes[$name]
                if ((Get-SsmcpUserEnvironment $name) -cne $changes[$name]) { throw "Could not persist $name for new terminal sessions." }
            }
        } catch {
            foreach ($name in $before.Keys) { Set-SsmcpUserEnvironment $name $before[$name] }
            throw
        }
        $completed = $true
        Write-Host "Native browser dependencies ready: Node.js $($node.version), Playwright $playwrightVersion, Chromium $($verified.version)."
        Write-Host 'The current session and new terminals use the saved SSMCP_NODE and SSMCP_BROWSER_EXECUTABLE paths. Existing terminals need to be reopened.'
        Write-Host 'ssmcp native browser tools are ready. No external harness was installed or registered.'
    } finally {
        if (-not $completed) {
            $env:Path = $originalPath
            $env:SSMCP_NODE = $originalNode
            $env:SSMCP_BROWSER_EXECUTABLE = $originalBrowser
        }
    }
}
# END SSMCP WINDOWS BROWSER DEPENDENCIES

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
    if (-not $SkipBrowserDependencies) {
        Install-SsmcpBrowserDependencies -ClientPath $binary -Architecture $architecture -TemporaryDirectory $workDir -Download {
            param($Url, $Path, $Limit)
            Invoke-BoundedDownload $Url $Path $Limit
        }
    } else { Write-Host 'Browser dependencies explicitly skipped; installing only the ssmcp client.' }
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
    Write-Host ('Sign in: ssmcp login --server ' + 'https://ssmcp.lmm.best')
    Write-Host 'Then enter your project directory and run: ssmcp'
} finally {
    if ($pending -and (Test-Path -LiteralPath $pending)) { Remove-Item -LiteralPath $pending -Force }
    if (Test-Path -LiteralPath $workDir) { Remove-Item -LiteralPath $workDir -Recurse -Force }
}
