# Execute the production dependency helpers with isolated Windows dependency fixtures.
[CmdletBinding()]
param([string]$Out)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repository = Split-Path $PSScriptRoot -Parent
$sources = @('scripts/install.ps1', 'src/client_downloads/install.ps1')
$helper = $null
$checks = [Collections.Generic.List[string]]::new()
$root = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-windows-browser-fixture-' + [Guid]::NewGuid().ToString('N'))
$original = @{}
foreach ($name in @('Path', 'LOCALAPPDATA', 'SSMCP_NODE', 'SSMCP_BROWSER_EXECUTABLE')) { $original[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$result = [ordered]@{ schema = 'ssmcp.windows-browser-bootstrap.v1'; ok = $false; scope = 'Production PowerShell helpers with mocked Windows executables, downloads and user environment; no Windows installation'; platform = [Environment]::OSVersion.Platform.ToString(); checks = $checks }
function Assert-Fixture([bool]$Condition, [string]$Description) {
    if (-not $Condition) { throw $Description }
    $checks.Add($Description)
}
function Assert-FixtureFailure([scriptblock]$Action, [string]$Contains, [string]$Description) {
    $failed = $false
    try { & $Action } catch {
        $failed = $true
        if ($_.Exception.Message -notlike "*$Contains*") { throw "Unexpected failure for ${Description}: $($_.Exception.Message)" }
    }
    Assert-Fixture $failed $Description
}
function Write-NativeFixture([string]$Path) {
    [IO.Directory]::CreateDirectory((Split-Path $Path -Parent)) | Out-Null
    [IO.File]::WriteAllBytes($Path, [byte[]]@(77, 90, 0, 0, 0, 0))
}
function Write-NodeZip([string]$Path, [string]$RootName, [string[]]$Extra = @()) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::Open($Path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($name in @("$RootName/node.exe", "$RootName/npm.cmd", "$RootName/node_modules/npm/bin/npm-cli.js") + $Extra) {
            $entry = $zip.CreateEntry($name)
            $stream = $entry.Open()
            try {
                $bytes = if ($name.EndsWith('/node.exe')) { [byte[]]@(77, 90, 0, 0) } else { [Text.Encoding]::UTF8.GetBytes('// fixture') }
                $stream.Write($bytes, 0, $bytes.Length)
            } finally { $stream.Dispose() }
        }
    } finally { $zip.Dispose() }
}
try {
    [IO.Directory]::CreateDirectory($root) | Out-Null
    foreach ($relative in $sources) {
        $source = [IO.File]::ReadAllText((Join-Path $repository $relative)).Replace('@@ORIGIN@@', "'https://ssmcp.example'").Replace('@@WIRE@@', '7')
        $tokens = $null; $errors = $null
        $ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
        Assert-Fixture ($errors.Count -eq 0) "$relative parses as PowerShell"
        $match = [regex]::Match($source, '(?s)# BEGIN SSMCP WINDOWS BROWSER DEPENDENCIES\r?\n.*?# END SSMCP WINDOWS BROWSER DEPENDENCIES')
        Assert-Fixture $match.Success "$relative contains the dependency helper"
        if ($null -eq $helper) { $helper = $match.Value } else { Assert-Fixture ($helper -ceq $match.Value) 'Both standalone installers use exactly the same dependency implementation' }
        $calls = @($ast.FindAll({ param($n) $n -is [Management.Automation.Language.CommandAst] -and $n.GetCommandName() -eq 'Install-SsmcpBrowserDependencies' }, $true))
        Assert-Fixture ($calls.Count -eq 1) "$relative invokes dependency setup once"
        $installAt = $source.IndexOf('[IO.File]::Replace($stagePath')
        if ($installAt -lt 0) { $installAt = $source.IndexOf('Move-Item -LiteralPath $pending -Destination $installed') }
        Assert-Fixture ($calls[0].Extent.StartOffset -lt $installAt) "$relative verifies dependencies before replacing the installed client"
    }
    . ([ScriptBlock]::Create($helper))

    # Exercise the actual process argv transport before replacing external commands.
    $realInvoke = ${function:Invoke-SsmcpBrowserCommand}
    $pwsh = (Get-Process -Id $PID).Path
    $argvScript = Join-Path $root 'argv fixture.ps1'
    [IO.File]::WriteAllText($argvScript, '[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); [Console]::WriteLine((ConvertTo-Json -InputObject @($args) -Compress))')
    $wanted = @('white space', 'a"quote', 'C:\directory with space\', 'literal $() & text', ([char]0x4e2d + [string][char]0x6587 + ' path'))
    $actual = (& $realInvoke $pwsh (@('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $argvScript) + $wanted)) | ConvertFrom-Json
    Assert-Fixture (($actual | ConvertTo-Json -Compress) -ceq ($wanted | ConvertTo-Json -Compress)) 'Native argv preserves Unicode, spaces, embedded quotes, trailing backslashes and shell metacharacters'
    Assert-FixtureFailure { & $realInvoke $pwsh @('-NoProfile', '-NonInteractive', '-Command', 'exit 23') | Out-Null } 'exit 23' 'Nonzero native exit cannot be reported as success'

    # Import only the report formatter: these real exceptions do not start the
    # Windows smoke worker or install any browser dependencies.
    $smokeSource = [IO.File]::ReadAllText((Join-Path $repository 'scripts/smoke_windows_browser.ps1'))
    $smokeAst = [Management.Automation.Language.Parser]::ParseInput($smokeSource, [ref]$tokens, [ref]$errors)
    Assert-Fixture ($errors.Count -eq 0) 'The real Windows smoke script parses'
    $formatters = @($smokeAst.EndBlock.Statements | Where-Object { $_ -is [Management.Automation.Language.FunctionDefinitionAst] -and $_.Name -ceq 'Get-SmokeFailure' })
    Assert-Fixture ($formatters.Count -eq 1) 'The smoke failure formatter is unambiguous'
    . ([ScriptBlock]::Create($formatters[0].Extent.Text))
    function Get-FixtureSmokeFailure([scriptblock]$Action) {
        try { & $Action | Out-Null }
        catch { return Get-SmokeFailure 'fixture_failure' $_ }
        throw 'The smoke diagnostic fixture expected a real exception.'
    }
    $privateMarker = 'ssmcp-private-diagnostic-fixture'
    $diagnosticZip = Join-Path $root ($privateMarker + '.zip')
    $diagnosticDirectory = Join-Path $root $privateMarker
    Write-NodeZip $diagnosticZip 'node-diagnostic-fixture'
    Expand-SsmcpNodeArchive $diagnosticZip $diagnosticDirectory 'node-diagnostic-fixture'
    $failure = Get-FixtureSmokeFailure { Expand-SsmcpNodeArchive $diagnosticZip $diagnosticDirectory 'node-diagnostic-fixture' }
    Assert-Fixture ($failure.exception_type -ceq 'MethodInvocationException' -and $failure.phase -ceq 'fixture_failure') 'A real archive file collision retains the outer method exception and phase'
    Assert-Fixture (@($failure.exception_chain | Where-Object { $_.type -ceq 'System.IO.IOException' -and $_.hresult -ne 0 }).Count -eq 1) 'The real archive failure retains its underlying I/O type and numeric error code'
    Assert-Fixture (@($failure.frames | Where-Object { $_.function -ceq 'Expand-SsmcpNodeArchive' -and $_.source -ceq 'browser_dependency_block' -and $_.line -gt 0 }).Count -eq 1) 'The archive failure identifies the production helper and source line'
    Assert-Fixture (@($failure.frames | Where-Object { $_.function -ceq 'Get-FixtureSmokeFailure' }).Count -eq 0) 'Unlisted caller frames and their paths are omitted'
    Assert-Fixture (-not ($failure | ConvertTo-Json -Depth 10 -Compress).Contains($privateMarker)) 'Archive diagnostics omit the private path embedded in the real error message'
    $failure = Get-FixtureSmokeFailure { & $realInvoke (Join-Path $root "$privateMarker/missing.exe") @('private argument') }
    Assert-Fixture (@($failure.exception_chain | Where-Object { $_.type -ceq 'System.ComponentModel.Win32Exception' -and $_.native_error_code -ne 0 }).Count -eq 1) 'A real process start failure retains the native system error code'
    Assert-Fixture (-not ($failure | ConvertTo-Json -Depth 10 -Compress).Contains($privateMarker)) 'Process diagnostics omit executable paths and invocation arguments'
    $failure = Get-FixtureSmokeFailure { & $realInvoke $pwsh @('-NoProfile', '-NonInteractive', '-Command', "[Console]::Error.WriteLine('$privateMarker'); exit 23") }
    Assert-Fixture ($failure.exception_type -ceq 'RuntimeException' -and -not ($failure | ConvertTo-Json -Depth 10 -Compress).Contains($privateMarker)) 'A real nonzero process exit remains a failure without retaining stderr'
    $failure = Get-FixtureSmokeFailure {
        $inner = [Exception]::new($privateMarker)
        for ($i = 0; $i -lt 12; $i++) { $inner = [Exception]::new($privateMarker, $inner) }
        throw $inner
    }
    Assert-Fixture ($failure.exception_chain.Count -eq 6) 'Exception chains have a fixed six-entry bound'
    $failure = Get-FixtureSmokeFailure {
        function Invoke-SmokeWorker([int]$Depth) {
            if ($Depth -gt 0) { Invoke-SmokeWorker ($Depth - 1) }
            else { throw [IO.IOException]::new($privateMarker) }
        }
        Invoke-SmokeWorker 12
    }
    Assert-Fixture ($failure.frames.Count -eq 8) 'Allowed stack frames have a fixed eight-entry bound'
    Assert-Fixture ((($failure | ConvertTo-Json -Depth 10 -Compress).Length -lt 4096) -and
        -not ($failure | ConvertTo-Json -Depth 10 -Compress).Contains($privateMarker)) 'Bounded diagnostics never copy exception messages'

    $script:client = Join-Path $root 'ssmcp.exe'
    $script:nodeFile = Join-Path $root 'existing node/node.exe'
    Write-NativeFixture $script:nodeFile
    $npm = Join-Path (Split-Path $script:nodeFile -Parent) 'node_modules/npm/bin/npm-cli.js'
    [IO.Directory]::CreateDirectory((Split-Path $npm -Parent)) | Out-Null
    [IO.File]::WriteAllText($npm, '// npm fixture')
    $script:chromium = Join-Path $root 'browser cache/chrome.exe'
    Write-NativeFixture $script:chromium
    function Get-SsmcpUserEnvironment([string]$Name) { return $script:userEnvironment[$Name] }
    function Set-SsmcpUserEnvironment([string]$Name, [string]$Value) {
        if ($script:fault -eq 'persist' -and $Name -eq 'SSMCP_BROWSER_EXECUTABLE') { $script:fault = ''; throw 'fixture persistence failure' }
        if ($null -eq $Value -or $Value -eq '') { $script:userEnvironment.Remove($Name) } else { $script:userEnvironment[$Name] = $Value }
    }
    function Get-Command {
        param([string]$Name, [string]$CommandType, [string]$ErrorAction)
        if ($script:existingNode) { return [pscustomobject]@{ Source = $script:nodeFile } }
        return $null
    }
    function Invoke-SsmcpBrowserCommand {
        param([string]$File, [string[]]$Arguments, [int]$TimeoutSeconds = 60)
        $script:calls.Add([pscustomobject]@{ file = $File; arguments = $Arguments; node = $env:SSMCP_NODE; path = $env:Path })
        if ($Arguments[0] -eq '--jitless') { return (@{ version = '24.22.3'; executable = $File } | ConvertTo-Json -Compress) }
        if ($Arguments[-1] -eq '--version') { return '11.19.1' }
        if ($File -eq $script:client) {
            if (($Arguments -contains '--native') -and ($Arguments -contains '--check')) {
                $report = @{ runtime = @{ node = $env:SSMCP_NODE; browser_executable = $env:SSMCP_BROWSER_EXECUTABLE } }
                if ($script:fault -eq 'old_client') { $report.Remove('runtime') }
                if ($script:fault -eq 'wrong_runtime') { $report.runtime.node = $script:client }
                return ($report | ConvertTo-Json -Depth 5 -Compress)
            }
            throw 'The default bootstrap must not enter external harness setup.'
        }
        if ($Arguments[0].EndsWith('npm-cli.js') -and $Arguments[1] -eq 'install') {
            if ($script:fault -eq 'npm') { throw 'fixture npm install failed' }
            if ($Arguments[-1] -cne 'playwright@1.65.1' -or $Arguments -notcontains '--ignore-scripts') { throw 'Unexpected dependency package or lifecycle scripts enabled.' }
            [IO.Directory]::CreateDirectory((Split-Path $script:playwrightCli -Parent)) | Out-Null
            [IO.File]::WriteAllText($script:playwrightCli, '// pinned Playwright fixture')
            return 'Playwright package installed'
        }
        if ($Arguments[0] -eq $script:playwrightCli) {
            if ($script:fault -eq 'chromium_download') { throw 'fixture Chromium download failed' }
            return 'Chromium already installed or downloaded'
        }
        if ($Arguments[1].Contains('chromium.launch')) {
            if ($script:fault -eq 'chromium_launch') { throw 'fixture Chromium launch failed' }
            return (@{ requested = $Arguments[3]; executable = $Arguments[3]; version = '130.0.0.0' } | ConvertTo-Json -Compress)
        }
        if ($Arguments[1].Contains('executablePath()')) { return (@{ version = '1.65.1'; cli = $script:playwrightCli; executable = $script:chromium } | ConvertTo-Json -Compress) }
        throw "Unexpected external command in fixture: $File"
    }
    function Reset-Fixture([string]$Fault = '', [bool]$Existing = $true, [bool]$Playwright = $true) {
        $script:fault = $Fault
        $script:existingNode = $Existing
        $script:userEnvironment = @{}
        $script:calls = [Collections.Generic.List[object]]::new()
        $env:SSMCP_NODE = $null
        $env:SSMCP_BROWSER_EXECUTABLE = $null
        $env:Path = 'fixture old system Node path'
        $env:LOCALAPPDATA = Join-Path $root ('local-' + [Guid]::NewGuid().ToString('N'))
        $script:playwrightCli = Join-Path $env:LOCALAPPDATA 'ssmcp/browser/playwright-1.65.1/node_modules/playwright/cli.js'
        if ($Playwright) {
            [IO.Directory]::CreateDirectory((Split-Path $script:playwrightCli -Parent)) | Out-Null
            [IO.File]::WriteAllText($script:playwrightCli, '// pinned Playwright fixture')
        }
    }
    $noDownload = { param($Url, $Path, $Limit) throw "Unexpected download: $Url" }
    Reset-Fixture
    Install-SsmcpBrowserDependencies $script:client 'x64' $root $noDownload
    Assert-Fixture (@($script:calls | Where-Object { $_.arguments -contains 'playwright@1.65.1' }).Count -eq 0) 'An existing working Node, npm and pinned Playwright are reused without package installation'
    Assert-Fixture (@($script:calls | Where-Object { $_.arguments[0] -eq $script:playwrightCli -and $_.arguments[-1] -eq 'chromium' }).Count -eq 1) 'Chromium is installed through the exact pinned Playwright CLI'
    Assert-Fixture ($script:userEnvironment['SSMCP_NODE'] -eq $script:nodeFile -and $script:userEnvironment['SSMCP_BROWSER_EXECUTABLE'] -eq $script:chromium) 'Verified Node and Chromium paths are persisted for later terminals'
    Assert-Fixture ($env:Path -eq 'fixture old system Node path') 'Dependency setup does not rewrite the caller or user PATH'
    $verification = @($script:calls | Where-Object { $_.arguments -contains '--check' })[0]
    Assert-Fixture ($verification.path -eq 'fixture old system Node path' -and $verification.node -eq $script:nodeFile) 'Runtime discovery is verified with the original PATH, not the temporary install PATH'
    $env:SSMCP_NODE = $null; $env:SSMCP_BROWSER_EXECUTABLE = $null
    $script:existingNode = $false
    Install-SsmcpBrowserDependencies $script:client 'x64' $root $noDownload
    Assert-Fixture ($env:SSMCP_NODE -eq $script:nodeFile -and $env:SSMCP_BROWSER_EXECUTABLE -eq $script:chromium) 'A later process restores the same paths from persisted user environment without downloading'

    foreach ($fault in @('npm', 'chromium_download', 'chromium_launch', 'old_client', 'wrong_runtime', 'persist')) {
        Reset-Fixture $fault $true ($fault -ne 'npm')
        $message = switch ($fault) { 'npm' { 'npm install failed' } 'chromium_download' { 'Chromium download failed' } 'chromium_launch' { 'Chromium launch failed' } 'persist' { 'persistence failure' } 'wrong_runtime' { 'did not resolve' } default { 'updated Windows client' } }
        Assert-FixtureFailure { Install-SsmcpBrowserDependencies $script:client 'x64' $root $noDownload } $message "$fault fails explicitly"
        Assert-Fixture ($script:userEnvironment.Count -eq 0 -and -not $env:SSMCP_NODE -and -not $env:SSMCP_BROWSER_EXECUTABLE) "$fault does not persist a partial success or keep temporary process overrides"
    }
    Reset-Fixture
    $env:SSMCP_NODE = Join-Path $root 'missing node.exe'
    Assert-FixtureFailure { Install-SsmcpBrowserDependencies $script:client 'x64' $root $noDownload } 'SSMCP_NODE must select' 'An invalid explicit Node override cannot fall back to another installation'
    Reset-Fixture
    $env:SSMCP_BROWSER_EXECUTABLE = Join-Path $root 'missing chrome.exe'
    Assert-FixtureFailure { Install-SsmcpBrowserDependencies $script:client 'x64' $root $noDownload } 'SSMCP_BROWSER_EXECUTABLE must name' 'An invalid explicit browser override cannot be silently replaced'

    foreach ($arch in @('x64', 'arm64')) {
        Reset-Fixture '' $false $false
        $work = Join-Path $root ('download-' + $arch)
        [IO.Directory]::CreateDirectory($work) | Out-Null
        $script:zipName = "node-v24.22.3-win-$arch.zip"
        $script:sourceZip = Join-Path $root $script:zipName
        Write-NodeZip $script:sourceZip "node-v24.22.3-win-$arch"
        $download = {
            param($Url, $Path, $Limit)
            if ($Url -eq 'https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt') {
                [IO.File]::WriteAllText($Path, (Get-FileHash -LiteralPath $script:sourceZip -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + $script:zipName)
            } elseif ($Url -eq "https://nodejs.org/dist/v24.22.3/$script:zipName" -and $Limit -eq 134217728) { [IO.File]::Copy($script:sourceZip, $Path) }
            else { throw "Unexpected dependency download source: $Url" }
        }
        Install-SsmcpBrowserDependencies $script:client $arch $work $download
        Assert-Fixture ($env:SSMCP_NODE.StartsWith($env:LOCALAPPDATA) -and [IO.File]::Exists($env:SSMCP_NODE)) "$arch installs the verified Node distribution under the user runtime directory"
        $installs = @($script:calls | Where-Object { $_.arguments -contains 'playwright@1.65.1' })
        Assert-Fixture ($installs.Count -eq 1 -and $installs[0].file -eq $env:SSMCP_NODE) "$arch installs pinned Playwright through the selected Node and its adjacent npm"
        Assert-Fixture (@($script:calls | Where-Object { $_.file -eq $script:client -and $_.arguments -notcontains '--native' }).Count -eq 0) "$arch never installs, probes or registers an external harness"
    }
    Reset-Fixture '' $false $false
    $work = Join-Path $root 'bad-hash'
    [IO.Directory]::CreateDirectory($work) | Out-Null
    $badHashDownload = {
        param($Url, $Path, $Limit)
        if ($Url.EndsWith('SHASUMS256.txt')) { [IO.File]::WriteAllText($Path, ('0' * 64) + '  node-v24.22.3-win-x64.zip') }
        else { [IO.File]::WriteAllBytes($Path, [byte[]]@(1, 2, 3)) }
    }
    Assert-FixtureFailure { Install-SsmcpBrowserDependencies $script:client 'x64' $work $badHashDownload } 'SHA-256 mismatch' 'An invalid Node checksum is rejected before extraction or execution'
    Assert-Fixture ($script:calls.Count -eq 0) 'Rejected Node bytes never reach native execution'
    $unsafe = Join-Path $root 'unsafe.zip'
    Write-NodeZip $unsafe 'node-safe' @('node-safe/../outside')
    Assert-FixtureFailure { Expand-SsmcpNodeArchive $unsafe (Join-Path $root 'unsafe-output') 'node-safe' } 'Unsafe Node.js archive path' 'Node archive traversal cannot escape its owned extraction directory'
    $result.ok = $true
    Write-Host "Windows browser bootstrap passed: $($checks.Count) checks; $($result.scope)."
} finally {
    foreach ($name in $original.Keys) { [Environment]::SetEnvironmentVariable($name, $original[$name], 'Process') }
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    if ($Out) {
        $parent = Split-Path ([IO.Path]::GetFullPath($Out)) -Parent
        [IO.Directory]::CreateDirectory($parent) | Out-Null
        [IO.File]::WriteAllText($Out, ($result | ConvertTo-Json -Depth 6))
    }
}
