#requires -Version 7.0
<#
Launch the real Windows executable in a disposable home. This checks window
creation, captures its visible pixels, and requires the application's bounded
normal exit. It does not connect to a live session or certify visual correctness.
Exit codes: 0 passed, 1 failed, 2 blocked by the execution environment.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-fA-F]{40}$')][string]$SourceCommit
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$evidence = [IO.Path]::GetFullPath($EvidenceDirectory)
if ((Test-Path -LiteralPath $evidence) -and @(Get-ChildItem -LiteralPath $evidence -Force).Count) {
    throw 'Use an empty evidence directory so a previous screenshot cannot become new evidence.'
}
New-Item -ItemType Directory -Force -Path $evidence | Out-Null
$report = [ordered]@{
    source_commit = $SourceCommit.ToLowerInvariant()
    platform = [Environment]::OSVersion.VersionString
    powershell = $PSVersionTable.PSVersion.ToString()
    status = 'failed'
    scope = 'Actual owned desktop process, visible window capture, and bounded normal exit; no live session or visual correctness assertion.'
    process_started = $false
    forced_termination = $false
    window = $null
    exit_code = $null
}
$process = $null
$stdoutTask = $null
$stderrTask = $null
$testRoot = $null
$previousDpi = [IntPtr]::Zero
try {
    if (-not $IsWindows) {
        $report.status = 'blocked'
        throw 'A Windows desktop runner is required.'
    }
    $repository = Split-Path -Parent $PSScriptRoot
    $actualCommit = (& git -C $repository rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $actualCommit -ine $SourceCommit) {
        throw 'The checked-out source does not match SourceCommit.'
    }
    & git -C $repository diff --quiet HEAD --
    if ($LASTEXITCODE -ne 0) { throw 'Tracked source changed after the requested commit.' }
    $binaryPath = (Get-Item -LiteralPath $Binary -ErrorAction Stop).FullName
    $report.executable = [ordered]@{
        file = [IO.Path]::GetFileName($binaryPath)
        bytes = (Get-Item -LiteralPath $binaryPath).Length
        sha256 = (Get-FileHash -LiteralPath $binaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
    }

    Add-Type -AssemblyName System.Drawing
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SsmcpWindowProbe {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool GetWindowRect(IntPtr window, out Rect bounds);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
    [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
}
'@
    $inputDesktop = [SsmcpWindowProbe]::OpenInputDesktop(0, $false, 1)
    $report.input_desktop_error = if ($inputDesktop -eq [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::GetLastWin32Error() } else { 0 }
    $report.interactive_desktop = [Environment]::UserInteractive -and $inputDesktop -ne [IntPtr]::Zero
    if ($inputDesktop -ne [IntPtr]::Zero) { [void][SsmcpWindowProbe]::CloseDesktop($inputDesktop) }

    $testRoot = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-windows-desktop-' + [guid]::NewGuid().ToString('N'))
    foreach ($directory in @($testRoot, "$testRoot/home", "$testRoot/roaming", "$testRoot/local")) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }
    $configuration = Join-Path $testRoot 'client.toml'
    [IO.File]::WriteAllText($configuration, '')
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $binaryPath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.WorkingDirectory = $testRoot
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.ArgumentList.Add('--exit-after-ms')
    $process.StartInfo.ArgumentList.Add('10000')
    foreach ($entry in @{
        HOME = "$testRoot/home"; USERPROFILE = "$testRoot/home"
        APPDATA = "$testRoot/roaming"; LOCALAPPDATA = "$testRoot/local"
        SSMCP_SESSION_HOME = "$testRoot/sessions"; SSMCP_AUTH_HOME = "$testRoot/auth"
        SSMCP_CLIENT_CONFIG = $configuration
    }.GetEnumerator()) { $process.StartInfo.Environment[$entry.Key] = $entry.Value }
    [void]$process.StartInfo.Environment.Remove('SSMCP_UI_CONFIG')
    if (-not $process.Start()) { throw 'Process.Start did not start the desktop executable.' }
    $report.process_started = $true
    $report.pid = $process.Id
    $report.arguments = @('--exit-after-ms', '10000')
    # Drain both streams concurrently; waiting first can deadlock a noisy child.
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    if (-not $report.interactive_desktop) {
        $report.status = 'blocked'
        $report.error = 'The executable was started, but this runner has no accessible interactive desktop.'
    } else {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $window = [IntPtr]::Zero
        while ($clock.Elapsed.TotalSeconds -lt 20) {
            $process.Refresh()
            if ($process.HasExited) { break }
            $window = $process.MainWindowHandle
            if ($window -ne [IntPtr]::Zero -and [SsmcpWindowProbe]::IsWindowVisible($window)) { break }
            Start-Sleep -Milliseconds 100
        }
        if ($window -eq [IntPtr]::Zero -or -not [SsmcpWindowProbe]::IsWindowVisible($window)) {
            throw 'The desktop executable did not expose a visible window within the bound.'
        }
        [uint32]$windowOwner = 0
        [void][SsmcpWindowProbe]::GetWindowThreadProcessId($window, [ref]$windowOwner)
        if ($windowOwner -ne $process.Id -or -not $process.MainWindowTitle.StartsWith('ssmcp-desktop')) {
            throw 'The observed window does not identify the owned ssmcp-desktop process.'
        }
        $previousDpi = [SsmcpWindowProbe]::SetThreadDpiAwarenessContext([IntPtr](-4))
        $bounds = [SsmcpWindowProbe+Rect]::new()
        if (-not [SsmcpWindowProbe]::GetWindowRect($window, [ref]$bounds)) { throw 'GetWindowRect failed.' }
        $report.window = [ordered]@{
            handle = $window.ToInt64(); pid = $windowOwner; title = $process.MainWindowTitle
            left = $bounds.Left; top = $bounds.Top; right = $bounds.Right; bottom = $bounds.Bottom
        }
        for ($attempt = 0; $attempt -lt 5; $attempt++) {
            [void][SsmcpWindowProbe]::SetForegroundWindow($window)
            Start-Sleep -Milliseconds 100
            if ([SsmcpWindowProbe]::GetForegroundWindow() -eq $window) { break }
        }
        if ([SsmcpWindowProbe]::GetForegroundWindow() -ne $window) {
            $report.status = 'blocked'
            throw 'The runner did not grant foreground capture of the observed owned window.'
        }
        $left = [Math]::Max($bounds.Left, [SsmcpWindowProbe]::GetSystemMetrics(76))
        $top = [Math]::Max($bounds.Top, [SsmcpWindowProbe]::GetSystemMetrics(77))
        $right = [Math]::Min($bounds.Right, [SsmcpWindowProbe]::GetSystemMetrics(76) + [SsmcpWindowProbe]::GetSystemMetrics(78))
        $bottom = [Math]::Min($bounds.Bottom, [SsmcpWindowProbe]::GetSystemMetrics(77) + [SsmcpWindowProbe]::GetSystemMetrics(79))
        if ($right - $left -lt 200 -or $bottom - $top -lt 150) { throw 'Too little of the owned window is on screen.' }
        $bitmap = [Drawing.Bitmap]::new($right - $left, $bottom - $top)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
            $image = Join-Path $evidence 'window.png'
            $bitmap.Save($image, [Drawing.Imaging.ImageFormat]::Png)
        } finally { $graphics.Dispose(); $bitmap.Dispose() }
        $report.screenshot = [ordered]@{
            file = 'window.png'; method = 'CopyFromScreen of the foreground owned window'
            left = $left; top = $top; right = $right; bottom = $bottom
            clipped_to_screen = ($left -ne $bounds.Left -or $top -ne $bounds.Top -or $right -ne $bounds.Right -or $bottom -ne $bounds.Bottom)
            sha256 = (Get-FileHash -LiteralPath $image -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    if (-not $process.WaitForExit(15000)) { throw 'The bounded desktop process did not exit normally.' }
    $report.exit_code = $process.ExitCode
    if ($report.status -ne 'blocked') {
        if ($process.ExitCode -ne 0) { throw "The desktop executable exited with $($process.ExitCode)." }
        $report.status = 'passed'
    }
} catch {
    $report.error = $_.Exception.Message
    if ($report.status -ne 'blocked') { $report.status = 'failed' }
} finally {
    if ($previousDpi -ne [IntPtr]::Zero) { [void][SsmcpWindowProbe]::SetThreadDpiAwarenessContext($previousDpi) }
    if ($null -ne $process -and $report.process_started) {
        try {
            if (-not $process.HasExited) {
                $report.forced_termination = $true
                $process.Kill($true)
                if (-not $process.WaitForExit(5000)) { throw 'The owned process did not stop during cleanup.' }
                if ($report.status -eq 'passed') { $report.status = 'failed' }
            }
            $report.exit_code = $process.ExitCode
            foreach ($stream in @{ stdout = $stdoutTask; stderr = $stderrTask }.GetEnumerator()) {
                if ($null -eq $stream.Value -or -not $stream.Value.Wait(5000)) { throw 'An owned output stream did not close.' }
                [IO.File]::WriteAllText((Join-Path $evidence ($stream.Key + '.log')), $stream.Value.GetAwaiter().GetResult())
            }
        } catch { $report.cleanup_error = $_.Exception.Message; $report.status = 'failed' }
        finally { $process.Dispose() }
    }
    if ($null -ne $testRoot) {
        try { Remove-Item -LiteralPath $testRoot -Recurse -Force }
        catch { $report.cleanup_error = $_.Exception.Message; $report.status = 'failed' }
    }
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $evidence 'result.json') -Encoding utf8
}
$report | ConvertTo-Json -Depth 8
if ($report.status -eq 'blocked') { exit 2 }
if ($report.status -ne 'passed') { exit 1 }
exit 0
