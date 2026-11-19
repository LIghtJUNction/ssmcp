# Real Windows release smoke: execute only the production dependency functions.
# The parent owns the deadline and restores the two persistent user variables.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ClientPath,
    [Parameter(Mandatory = $true)][ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$Target,
    [Parameter(Mandatory = $true)][ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$')][string]$ExpectedVersion,
    [Parameter(Mandatory = $true)][string]$Out,
    [ValidateRange(60, 1800)][int]$TimeoutSeconds = 1800,
    [Parameter(DontShow = $true)][switch]$Worker,
    [Parameter(DontShow = $true)][string]$WorkerRoot
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Write-SmokeReport([string]$Path, $Value) {
    $destination = [IO.Path]::GetFullPath($Path)
    [IO.Directory]::CreateDirectory((Split-Path $destination -Parent)) | Out-Null
    [IO.File]::WriteAllText($destination, ($Value | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
}

function Get-SmokePeArchitecture([string]$Path) {
    $inputFile = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($inputFile)
    try {
        if ($inputFile.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) { throw 'Expected a native PE executable.' }
        $inputFile.Position = 60
        $offset = $reader.ReadInt32()
        if ($offset -lt 64 -or $offset -gt $inputFile.Length - 6) { throw 'Invalid PE header offset.' }
        $inputFile.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550) { throw 'Invalid PE signature.' }
        switch ($reader.ReadUInt16()) {
            0x8664 { return 'x64' }
            0xaa64 { return 'arm64' }
            default { throw 'Expected a supported 64-bit PE executable.' }
        }
    } finally { $reader.Dispose(); $inputFile.Dispose() }
}

function Get-SmokeTextHash([string]$Text) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text)))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Initialize-SmokeJobType {
    if ('SsmcpWindowsSmokeJob' -as [type]) { return }
    # A job keeps descendants owned even after an intermediate Node process exits.
    # The worker waits on stdin until assignment; no installer process can escape that race.
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;

public sealed class SsmcpWindowsSmokeJob : IDisposable
{
    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimits
    {
        public long ProcessUserTime, JobUserTime;
        public uint Flags;
        public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters
    {
        public ulong ReadOperations, WriteOperations, OtherOperations;
        public ulong ReadBytes, WriteBytes, OtherBytes;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimits
    {
        public BasicLimits Basic;
        public IoCounters Io;
        public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct Accounting
    {
        public long UserTime, KernelTime, PeriodUserTime, PeriodKernelTime;
        public uint PageFaults, TotalProcesses, ActiveProcesses, TerminatedProcesses;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
    private static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetInformationJobObject(IntPtr job, int kind, ref ExtendedLimits limits, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool QueryInformationJobObject(IntPtr job, int kind, out Accounting accounting, uint size, IntPtr returned);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr handle);

    private IntPtr handle;
    public SsmcpWindowsSmokeJob()
    {
        handle = CreateJobObjectW(IntPtr.Zero, null);
        if (handle == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        var limits = new ExtendedLimits();
        limits.Basic.Flags = 0x00002000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE; no breakaway.
        if (!SetInformationJobObject(handle, 9, ref limits, (uint)Marshal.SizeOf<ExtendedLimits>()))
        {
            int error = Marshal.GetLastWin32Error();
            Dispose();
            throw new Win32Exception(error);
        }
    }
    public void Assign(Process process)
    {
        if (!AssignProcessToJobObject(handle, process.Handle))
            throw new Win32Exception(Marshal.GetLastWin32Error());
    }
    public bool TerminateAndWait(int milliseconds)
    {
        if (!TerminateJobObject(handle, 1)) throw new Win32Exception(Marshal.GetLastWin32Error());
        var clock = Stopwatch.StartNew();
        while (true)
        {
            Accounting accounting;
            if (!QueryInformationJobObject(handle, 1, out accounting, (uint)Marshal.SizeOf<Accounting>(), IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            if (accounting.ActiveProcesses == 0) return true;
            if (clock.ElapsedMilliseconds >= milliseconds) return false;
            Thread.Sleep(25);
        }
    }
    public void Dispose()
    {
        if (handle != IntPtr.Zero)
        {
            CloseHandle(handle);
            handle = IntPtr.Zero;
        }
    }
}
'@
}

function Invoke-SmokeWorker {
    $phase = 'windows_guard'
    $checks = [Collections.Generic.List[string]]::new()
    $downloads = [Collections.Generic.List[object]]::new()
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $report = [ordered]@{
        schema = 'ssmcp.windows-native-browser-smoke.worker.v1'
        ok = $false
        target = $Target
        checks = $checks
        downloads = $downloads
    }
    function Assert-Smoke([bool]$Condition, [string]$Description) {
        if (-not $Condition) { throw $Description }
        $checks.Add($Description)
    }
    function Owned-SmokePath([string]$Path) {
        $prefix = [IO.Path]::GetFullPath($WorkerRoot).TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
        $full = [IO.Path]::GetFullPath($Path)
        if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Dependency path escaped the temporary smoke directory.' }
        return '<temporary>/' + $full.Substring($prefix.Length).Replace('\', '/')
    }
    try {
        Assert-Smoke ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Actual Windows runtime is required'
        Assert-Smoke ($PSVersionTable.PSVersion.Major -ge 7) 'The release smoke runs under native PowerShell 7'
        Assert-Smoke ($WorkerRoot -and [IO.Directory]::Exists($WorkerRoot)) 'The parent created the temporary smoke directory'
        $architecture = if ($Target -eq 'aarch64-pc-windows-msvc') { 'arm64' } else { 'x64' }
        Assert-Smoke ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant() -ceq $architecture) 'Runner architecture matches the release target'
        $phase = 'owned_process_job'
        Assert-Smoke ([Console]::ReadLine() -ceq 'SSMCP_WINDOWS_SMOKE_START') 'The parent assigned the worker to its owned Windows job before installation'
        $repository = Split-Path $PSScriptRoot -Parent

        $phase = 'production_helpers'
        $source = [IO.File]::ReadAllText((Join-Path $repository 'scripts/install.ps1'))
        $other = [IO.File]::ReadAllText((Join-Path $repository 'src/client_downloads/install.ps1'))
        $pattern = '(?s)# BEGIN SSMCP WINDOWS BROWSER DEPENDENCIES\r?\n.*?# END SSMCP WINDOWS BROWSER DEPENDENCIES'
        $blocks = @([regex]::Matches($source, $pattern))
        $otherBlocks = @([regex]::Matches($other, $pattern))
        Assert-Smoke ($blocks.Count -eq 1 -and $otherBlocks.Count -eq 1) 'Both installers have one browser dependency block'
        $helper = $blocks[0].Value
        Assert-Smoke ($helper -ceq $otherBlocks[0].Value) 'Both self-contained installers use the identical production browser implementation'
        $tokens = $null; $parseErrors = $null
        $ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
        Assert-Smoke ($parseErrors.Count -eq 0) 'The production release installer parses'
        foreach ($name in @('Wait-DownloadTask', 'Receive-BoundedStream', 'Invoke-BoundedDownload')) {
            $definitions = @($ast.EndBlock.Statements | Where-Object { $_ -is [Management.Automation.Language.FunctionDefinitionAst] -and $_.Name -ceq $name })
            Assert-Smoke ($definitions.Count -eq 1) "The bounded downloader function $name is unambiguous"
            . ([ScriptBlock]::Create($definitions[0].Extent.Text))
        }
        $helperAst = [Management.Automation.Language.Parser]::ParseInput($helper, [ref]$tokens, [ref]$parseErrors)
        Assert-Smoke ($parseErrors.Count -eq 0 -and @($helperAst.EndBlock.Statements | Where-Object { $_ -isnot [Management.Automation.Language.FunctionDefinitionAst] }).Count -eq 0) 'Only function definitions are imported from the browser block'
        . ([ScriptBlock]::Create($helper))
        $report.helper_sha256 = Get-SmokeTextHash $helper
        $report.installer_sha256 = [ordered]@{
            'scripts/install.ps1' = (Get-FileHash -LiteralPath (Join-Path $repository 'scripts/install.ps1') -Algorithm SHA256).Hash.ToLowerInvariant()
            'src/client_downloads/install.ps1' = (Get-FileHash -LiteralPath (Join-Path $repository 'src/client_downloads/install.ps1') -Algorithm SHA256).Hash.ToLowerInvariant()
        }

        $phase = 'client_identity'
        $client = [IO.Path]::GetFullPath($ClientPath)
        Assert-Smoke ((Get-SmokePeArchitecture $client) -ceq $architecture) 'The client is the native executable for this target'
        $clientVersion = Invoke-SsmcpBrowserCommand $client @('--version')
        Assert-Smoke ($clientVersion -ceq "ssmcp $ExpectedVersion") 'The client version matches the validated release'
        $report.client = [ordered]@{
            version = $clientVersion
            target = $Target
            architecture = $architecture
            sha256 = (Get-FileHash -LiteralPath $client -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        $restrictedPath = $env:Path
        Assert-Smoke (-not (Get-Command node.exe -CommandType Application -ErrorAction SilentlyContinue)) 'The clean-install pass cannot reuse a preinstalled Node from PATH'
        Assert-Smoke (-not $env:SSMCP_NODE -and -not $env:SSMCP_BROWSER_EXECUTABLE -and
            -not (Get-SsmcpUserEnvironment 'SSMCP_NODE') -and -not (Get-SsmcpUserEnvironment 'SSMCP_BROWSER_EXECUTABLE')) 'Explicit runtime overrides are absent for the clean-install pass'
        $download = {
            param($Url, $Path, $Limit)
            $uri = [Uri]$Url
            if ($uri.Scheme -cne 'https' -or $uri.Host -cne 'nodejs.org' -or $uri.UserInfo -or -not $uri.AbsolutePath.StartsWith('/dist/', [StringComparison]::Ordinal)) {
                throw 'The Node bootstrap requested an unexpected download source.'
            }
            Invoke-BoundedDownload $Url $Path $Limit
            $downloads.Add([ordered]@{
                host = $uri.Host
                resource = [IO.Path]::GetFileName($uri.AbsolutePath)
                bytes = (Get-Item -LiteralPath $Path).Length
                sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
            })
        }
        $work = Join-Path $WorkerRoot 'download-first'
        [IO.Directory]::CreateDirectory($work) | Out-Null
        $phase = 'clean_dependency_install'
        Install-SsmcpBrowserDependencies -ClientPath $client -Architecture $architecture -TemporaryDirectory $work -Download $download
        Assert-Smoke ($downloads.Count -eq 2) 'The clean pass downloaded the official Node checksum list and archive'
        Assert-Smoke ($env:Path -ceq $restrictedPath) 'Dependency setup restored PATH before native client verification'

        $phase = 'installed_identity'
        $node = Get-SsmcpUsableNode $env:SSMCP_NODE
        Assert-Smoke ($null -ne $node -and $node.version -match '^24\.[0-9]+\.[0-9]+$') 'The official managed Node 24 LTS installation runs with npm'
        $nodePath = $node.executable
        $nodeRelative = Owned-SmokePath $nodePath
        Assert-Smoke ((Get-SmokePeArchitecture $nodePath) -ceq $architecture) 'Node PE architecture matches the release target'
        $nodeIdentity = (Invoke-SsmcpBrowserCommand $nodePath @('--eval', 'console.log(JSON.stringify({arch:process.arch,platform:process.platform,version:process.versions.node}))')) | ConvertFrom-Json
        Assert-Smoke ($nodeIdentity.arch -ceq $architecture -and $nodeIdentity.platform -ceq 'win32') 'Node executes as the expected Windows architecture'
        $packageDirectory = Join-Path $env:LOCALAPPDATA 'ssmcp/browser/playwright-1.65.1'
        $playwright = Get-SsmcpPlaywright $nodePath $packageDirectory '1.65.1'
        Assert-Smoke ($null -ne $playwright -and $playwright.version -ceq '1.65.1') 'The actual installed Playwright version is pinned to 1.65.1'
        $browserPath = $env:SSMCP_BROWSER_EXECUTABLE
        $browserRelative = Owned-SmokePath $browserPath
        Assert-Smoke (Test-SsmcpSamePath $browserPath $playwright.executable) 'The selected browser is the downloaded Playwright Chromium'
        $browserArchitecture = Get-SmokePeArchitecture $browserPath
        $nodeHash = (Get-FileHash -LiteralPath $nodePath -Algorithm SHA256).Hash.ToLowerInvariant()
        $browserHash = (Get-FileHash -LiteralPath $browserPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $packageFile = Join-Path $packageDirectory 'node_modules/playwright/package.json'
        $packageHash = (Get-FileHash -LiteralPath $packageFile -Algorithm SHA256).Hash.ToLowerInvariant()

        $phase = 'native_client_check'
        $runtime = (Invoke-SsmcpBrowserCommand $client @('setup', 'browser', '--native', '--check', '--json') 120) | ConvertFrom-Json
        Assert-Smoke ($runtime.mode -ceq 'native' -and $runtime.ready -eq $true) 'The new client reports native browser readiness'
        Assert-Smoke ((Test-SsmcpSamePath $runtime.runtime.node $nodePath) -and (Test-SsmcpSamePath $runtime.runtime.browser_executable $browserPath)) 'The client resolves the exact selected Node and Chromium without a modified PATH'

        $phase = 'headless_page'
        $pageScript = @'
const r=require("node:module").createRequire(require("node:path").join(process.argv[1],"package.json"));
const timer=setTimeout(()=>{process.stderr.write("Headless smoke deadline exceeded\n");process.exit(1)},45000);
(async()=>{
  let browser,result;
  try{
    browser=await r("playwright").chromium.launch({executablePath:process.argv[2],headless:true,timeout:30000});
    const page=await browser.newPage({viewport:{width:320,height:240}});
    await page.setContent("<!doctype html><title>ssmcp-native-browser-smoke</title><h1 id='ready'>bootstrap-ready</h1>",{waitUntil:"load",timeout:10000});
    const title=await page.title(),text=await page.locator("#ready").textContent();
    if(title!=="ssmcp-native-browser-smoke"||text!=="bootstrap-ready")throw new Error("Local page verification failed");
    result={version:browser.version(),title,text,headless:true};
  }finally{
    if(browser)await browser.close();
    clearTimeout(timer);
  }
  console.log(JSON.stringify(result));
})().catch(()=>{process.stderr.write("Headless page smoke failed\n");process.exit(1)});
'@
        $page = (Invoke-SsmcpBrowserCommand $nodePath @('--eval', $pageScript, $packageDirectory, $browserPath) 60) | ConvertFrom-Json
        Assert-Smoke ($page.headless -eq $true -and $page.title -ceq 'ssmcp-native-browser-smoke' -and $page.text -ceq 'bootstrap-ready' -and $page.version) 'Chromium starts headlessly, renders a local page and closes successfully'

        $phase = 'reuse_dependencies'
        $reuseWork = Join-Path $WorkerRoot 'download-reuse'
        [IO.Directory]::CreateDirectory($reuseWork) | Out-Null
        $downloadCount = $downloads.Count
        Install-SsmcpBrowserDependencies -ClientPath $client -Architecture $architecture -TemporaryDirectory $reuseWork -Download $download
        Assert-Smoke ($downloads.Count -eq $downloadCount) 'The second real setup pass reuses Node without another download'
        Assert-Smoke ((Test-SsmcpSamePath $env:SSMCP_NODE $nodePath) -and (Test-SsmcpSamePath $env:SSMCP_BROWSER_EXECUTABLE $browserPath)) 'The second setup pass preserves the selected runtime paths'
        Assert-Smoke ((Get-FileHash -LiteralPath $nodePath -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $nodeHash -and
            (Get-FileHash -LiteralPath $browserPath -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $browserHash -and
            (Get-FileHash -LiteralPath $packageFile -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $packageHash) 'Reusing dependencies preserves Node, Chromium and the pinned package bytes'
        Assert-Smoke ((Test-SsmcpSamePath (Get-SsmcpUserEnvironment 'SSMCP_NODE') $nodePath) -and
            (Test-SsmcpSamePath (Get-SsmcpUserEnvironment 'SSMCP_BROWSER_EXECUTABLE') $browserPath)) 'Production persistence selects the verified runtime paths'
        $report.node = [ordered]@{ version = $node.version; architecture = $architecture; path = $nodeRelative; sha256 = $nodeHash }
        $report.playwright = [ordered]@{ version = $playwright.version; package_sha256 = $packageHash }
        $report.chromium = [ordered]@{ version = $page.version; architecture = $browserArchitecture; path = $browserRelative; sha256 = $browserHash; architecture_matches_target = ($browserArchitecture -ceq $architecture); headless_local_page = $true }
        $report.runtime = [ordered]@{ mode = $runtime.mode; ready = $runtime.ready; exact_paths_match = $true; independent_of_path = $true }
        $report.reused = $true
        $report.ok = $true
    } catch {
        # Native/npm stderr may contain environment-derived values; never serialize it.
        $report.failure = [ordered]@{ phase = $phase; exception_type = $_.Exception.GetType().Name }
    } finally {
        $report.elapsed_seconds = [Math]::Round($clock.Elapsed.TotalSeconds, 3)
        Write-SmokeReport $Out $report
    }
    return [bool]$report.ok
}

function Invoke-SmokeParent {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $phase = 'windows_guard'
    $temporaryRoot = $null
    $process = $null
    $job = $null
    $started = $false
    $userBefore = @{}
    $processBefore = @{}
    $cleanupErrors = [Collections.Generic.List[string]]::new()
    $report = [ordered]@{
        schema = 'ssmcp.windows-native-browser-smoke.v1'
        ok = $false
        target = $Target
        expected_client_version = $ExpectedVersion
        platform = [Environment]::OSVersion.Platform.ToString()
        powershell_version = $PSVersionTable.PSVersion.ToString()
        scope = 'Real production dependency helpers, official Node installation, pinned Playwright, headless Chromium and the newly built native client on Windows'
        started_at = [DateTime]::UtcNow.ToString('o')
    }
    try {
        if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'A real Windows runner is required; this smoke cannot pass on another OS.' }
        if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'This release smoke requires PowerShell 7.' }
        $phase = 'isolated_worker'
        $client = [IO.Path]::GetFullPath($ClientPath)
        if (-not [IO.File]::Exists($client)) { throw 'The newly built client is missing.' }
        foreach ($name in @('Path', 'SSMCP_NODE', 'SSMCP_BROWSER_EXECUTABLE', 'LOCALAPPDATA', 'APPDATA', 'USERPROFILE')) {
            $processBefore[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        }
        foreach ($name in @('SSMCP_NODE', 'SSMCP_BROWSER_EXECUTABLE')) {
            $userBefore[$name] = [Environment]::GetEnvironmentVariable($name, 'User')
            [Environment]::SetEnvironmentVariable($name, $null, 'User')
        }
        $temporaryBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
        $temporaryRoot = Join-Path ([IO.Path]::GetFullPath($temporaryBase)) ('ssmcp-native-browser-smoke-' + [Guid]::NewGuid().ToString('N'))
        [IO.Directory]::CreateDirectory($temporaryRoot) | Out-Null
        $directories = [ordered]@{
            LOCALAPPDATA = 'local'
            APPDATA = 'roaming'
            USERPROFILE = 'profile'
            TEMP = 'tmp'
            TMP = 'tmp'
            PLAYWRIGHT_BROWSERS_PATH = 'browsers'
            NPM_CONFIG_CACHE = 'npm-cache'
        }
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = (Get-Process -Id $PID).Path
        $start.WorkingDirectory = $temporaryRoot
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.RedirectStandardInput = $true
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
        $start.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
        foreach ($name in @($start.EnvironmentVariables.Keys)) {
            if ($name -imatch '^(NPM_CONFIG_|PLAYWRIGHT_)' -or $name -iin @('SSMCP_NODE', 'SSMCP_BROWSER_EXECUTABLE', 'NODE_OPTIONS', 'NODE_PATH', 'NODE_AUTH_TOKEN', 'NPM_TOKEN')) {
                $start.EnvironmentVariables.Remove($name)
            }
        }
        foreach ($name in $directories.Keys) {
            $path = Join-Path $temporaryRoot $directories[$name]
            [IO.Directory]::CreateDirectory($path) | Out-Null
            $start.EnvironmentVariables[$name] = $path
        }
        foreach ($name in @('NPM_CONFIG_USERCONFIG', 'NPM_CONFIG_GLOBALCONFIG')) {
            $path = Join-Path $temporaryRoot ($name.ToLowerInvariant() + '.npmrc')
            [IO.File]::WriteAllText($path, '', [Text.UTF8Encoding]::new($false))
            $start.EnvironmentVariables[$name] = $path
        }
        $start.EnvironmentVariables['NPM_CONFIG_REGISTRY'] = 'https://registry.npmjs.org'
        $start.EnvironmentVariables['NPM_CONFIG_YES'] = 'true'
        $start.EnvironmentVariables['Path'] = (@(
            (Join-Path $env:SystemRoot 'System32'),
            $env:SystemRoot,
            (Join-Path $env:SystemRoot 'System32/Wbem')
        ) -join ';')
        $workerReport = Join-Path $temporaryRoot 'worker-result.json'
        foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $PSCommandPath,
                '-Worker', '-WorkerRoot', $temporaryRoot, '-ClientPath', $client, '-Target', $Target,
                '-ExpectedVersion', $ExpectedVersion, '-Out', $workerReport)) {
            $start.ArgumentList.Add($argument)
        }
        $phase = 'owned_process_job'
        Initialize-SmokeJobType
        $job = [SsmcpWindowsSmokeJob]::new()
        $process = [Diagnostics.Process]::new()
        $process.StartInfo = $start
        if (-not $process.Start()) { throw 'Cannot start the isolated smoke worker.' }
        $started = $true
        $job.Assign($process)
        $process.StandardInput.WriteLine('SSMCP_WINDOWS_SMOKE_START')
        $process.StandardInput.Close()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $phase = 'native_dependency_worker'
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            throw 'The isolated smoke worker exceeded its deadline.'
        }
        $report.worker_exit_code = $process.ExitCode
        if (-not $job.TerminateAndWait(15000)) { throw 'Owned browser descendants did not stop before their deadline.' }
        # Bound pipe completion too: an orphan must not retain a diagnostic handle forever.
        if (-not $stdout.Wait(5000) -or -not $stderr.Wait(5000)) { throw 'Worker diagnostics did not close before their deadline.' }
        # Count, then discard output. The worker report contains only selected metadata.
        if ($stdout.GetAwaiter().GetResult().Length -gt 1048576 -or $stderr.GetAwaiter().GetResult().Length -gt 1048576) {
            throw 'The smoke worker exceeded its diagnostic output limit.'
        }
        if (-not [IO.File]::Exists($workerReport) -or (Get-Item -LiteralPath $workerReport).Length -gt 65536) { throw 'The bounded smoke report is missing.' }
        $details = [IO.File]::ReadAllText($workerReport) | ConvertFrom-Json
        if ($details.schema -cne 'ssmcp.windows-native-browser-smoke.worker.v1' -or $details.target -cne $Target) { throw 'Unexpected smoke report identity.' }
        foreach ($name in @('checks', 'downloads', 'client', 'node', 'playwright', 'chromium', 'runtime', 'reused', 'helper_sha256', 'installer_sha256', 'failure')) {
            if ($details.PSObject.Properties[$name]) { $report[$name] = $details.$name }
        }
        $report.ok = ($process.ExitCode -eq 0 -and $details.ok -eq $true)
        if (-not $report.ok -and -not $report.Contains('failure')) {
            $report.failure = [ordered]@{ phase = $phase; exception_type = 'WorkerFailed' }
        }
    } catch {
        $report.ok = $false
        $report.failure = [ordered]@{ phase = $phase; exception_type = $_.Exception.GetType().Name }
    } finally {
        if ($job) {
            try {
                if (-not $job.TerminateAndWait(15000)) { throw 'Owned process job remained active.' }
                $report.owned_processes_stopped = $true
            } catch { $cleanupErrors.Add('owned_process_job_cleanup_failed'); $report.owned_processes_stopped = $false }
            finally { $job.Dispose() }
        }
        # Covers a worker that started but could not be assigned; it is still waiting on stdin.
        if ($started -and -not $process.HasExited) {
            try { $process.Kill($true); if (-not $process.WaitForExit(15000)) { throw 'Worker remained alive.' } }
            catch { $cleanupErrors.Add('owned_worker_cleanup_failed') }
        }
        if ($process) { $process.Dispose() }
        foreach ($name in $userBefore.Keys) {
            try {
                [Environment]::SetEnvironmentVariable($name, $userBefore[$name], 'User')
                if ([Environment]::GetEnvironmentVariable($name, 'User') -cne $userBefore[$name]) { throw 'User environment restore failed.' }
            } catch { $cleanupErrors.Add('user_environment_restore_failed') }
        }
        foreach ($name in $processBefore.Keys) {
            if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $processBefore[$name]) {
                try { [Environment]::SetEnvironmentVariable($name, $processBefore[$name], 'Process') }
                catch { $cleanupErrors.Add('process_environment_restore_failed') }
                if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $processBefore[$name]) { $cleanupErrors.Add('process_environment_restore_failed') }
            }
        }
        if ($temporaryRoot -and (Test-Path -LiteralPath $temporaryRoot)) {
            try { Remove-Item -LiteralPath $temporaryRoot -Recurse -Force }
            catch { $cleanupErrors.Add('temporary_directory_cleanup_failed') }
        }
        $report.environment_restored = ($cleanupErrors.Count -eq 0)
        $report.temporary_directory_removed = (-not $temporaryRoot -or -not (Test-Path -LiteralPath $temporaryRoot))
        $report.cleanup_errors = $cleanupErrors
        if ($cleanupErrors.Count -ne 0) { $report.ok = $false }
        $report.finished_at = [DateTime]::UtcNow.ToString('o')
        $report.elapsed_seconds = [Math]::Round($clock.Elapsed.TotalSeconds, 3)
        Write-SmokeReport $Out $report
    }
    if ($report.ok) {
        Write-Host "Real Windows native browser smoke passed for $Target; environment restored and temporary dependencies removed."
    } else {
        Write-Host "Real Windows native browser smoke failed for $Target. See the bounded JSON report; raw environment and subprocess output were not recorded."
    }
    return [bool]$report.ok
}

$ok = if ($Worker) { Invoke-SmokeWorker } else { Invoke-SmokeParent }
if (-not $ok) { exit 1 }
