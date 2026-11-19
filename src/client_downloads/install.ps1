# Same-origin compatible release; verified user installation, then an explicit relay connection.
[CmdletBinding()]
param(
    [string]$Server,
    [Parameter(Mandatory = $true)][string[]]$Allow,
    [string]$Name,
    [string]$InstallDir,
    [switch]$SkipBrowserDependencies,
    [switch]$NoWatchdog,
    [ValidateRange(1,3600)][int]$ReconnectDelay = 5
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$downloadOrigin = @@ORIGIN@@
$requiredWire = @@WIRE@@
if (-not $Server) { $Server = $downloadOrigin }
if ($Server.TrimEnd('/') -cne $downloadOrigin) { throw 'The connection server must match this gateway.' }
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
    if (-not $SkipBrowserDependencies) {
        Install-SsmcpBrowserDependencies -ClientPath $binaryPath -Architecture $architecture -TemporaryDirectory $temporaryDirectory -Download {
            param($Url, $Path, $Limit)
            Receive-File $Url $Path $Limit
        }
    } else { Write-Host 'Browser dependencies explicitly skipped; installing only the ssmcp client.' }
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
    Write-Host 'Starting the controlled connection. Keep this terminal open; Ctrl-C stops it.'
    $connectionArguments = @('--server', $Server)
    foreach ($targetAccount in $Allow) { $connectionArguments += @('--allow', $targetAccount) }
    if ($Name) { $connectionArguments += @('--name', $Name) }
    if ($NoWatchdog) { $connectionArguments += '--no-watchdog' }
    else { $connectionArguments += @('--reconnect-delay', $ReconnectDelay.ToString()) }
    & $installedPath @connectionArguments
    if ($LASTEXITCODE -ne 0) { throw "The controlled connection exited with code $LASTEXITCODE." }
} finally {
    $cancellation.Cancel()
    if ($client) { $client.Dispose() }
    elseif ($handler) { $handler.Dispose() }
    $cancellation.Dispose()
    if ($stagePath -and [IO.File]::Exists($stagePath)) { Remove-Item -LiteralPath $stagePath -Force }
    # Keep a replacement backup if an unexpected filesystem error interrupted it.
    if (Test-Path -LiteralPath $temporaryDirectory) { Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force }
}
