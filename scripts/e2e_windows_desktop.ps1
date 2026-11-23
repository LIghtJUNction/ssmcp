#requires -Version 7.0
<#
Launch the real Windows executable in a disposable home. This checks window
creation, waits for a presented frame matching the resized client, requires nonuniform
client-area pixels, captures the owned window, and checks bounded normal exit.
It does not connect to a live session or certify the complete visual layout.
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
    scope = 'Owned Windows process, first-present and resized-present receipts, nonuniform client pixels, screenshot, and bounded normal exit; no live session or complete layout assertion.'
    process_started = $false
    forced_termination = $false
    window = $null
    exit_code = $null
}
$process = $null
$stdoutTask = $null
$stderrTask = $null
$testRoot = $null
$frameReceiptsPath = $null
$previousDpi = [IntPtr]::Zero
$script:presentReadDiagnostics = [ordered]@{ io_errors = 0; last_error = $null }
function Read-PresentReceipts {
    param([string]$Path, [int]$OwnedProcess)
    if (-not (Test-Path -LiteralPath $Path)) { return @() }
    $reader = $null
    try {
        # The application keeps its writer open. Windows requires the new reader
        # to share that existing write access as well as allowing other readers.
        $reader = [IO.FileStream]::new($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
        $length = $reader.Length
        if ($length -gt 1048576) { throw 'The presented-frame journal exceeded its size bound.' }
        $buffer = [byte[]]::new([int]$length)
        $read = 0
        while ($read -lt $buffer.Length) {
            $count = $reader.Read($buffer, $read, $buffer.Length - $read)
            if ($count -eq 0) { break }
            $read += $count
        }
        # Snapshot the opened length; concurrent appends cannot extend this read.
        # Decode only complete records, including when the trailing UTF-8 is partial.
        $end = $read - 1
        while ($end -ge 0 -and $buffer[$end] -ne 10) { $end-- }
        if ($end -lt 0) { return @() }
        $raw = [Text.UTF8Encoding]::new($false, $true).GetString($buffer, 0, $end + 1)
    } catch [IO.IOException] {
        $script:presentReadDiagnostics.io_errors = [Math]::Min(4096, $script:presentReadDiagnostics.io_errors + 1)
        $script:presentReadDiagnostics.last_error = @{
            type = $_.Exception.GetType().FullName; hresult = $_.Exception.HResult
        }
        return @()
    } finally {
        if ($null -ne $reader) { $reader.Dispose() }
    }
    # Ignore an incomplete trailing write; complete malformed records fail.
    $completeEnd = $raw.LastIndexOf([char]10)
    if ($completeEnd -lt 0) { return @() }
    $lines = $raw.Substring(0, $completeEnd).Split([char]10)
    if ($lines.Length -gt 4096) { throw 'The presented-frame journal exceeded its record bound.' }
    [long]$previousFrame = 0
    foreach ($line in $lines) {
        $row = $line | ConvertFrom-Json -AsHashtable
        if ($row['schema'] -ne 'ssmcp-desktop-present-v1' -or
            $row['stage'] -ne 'present_returned' -or $row['pid'] -ne $OwnedProcess -or
            $row['frame_index'] -le $previousFrame -or $row['width'] -le 0 -or $row['height'] -le 0) {
            throw 'The frame journal did not identify ordered present calls from the owned process.'
        }
        $previousFrame = $row['frame_index']
        $row
    }
}
function Test-PresentJournalReader {
    param([string]$Path)
    $writer = [IO.FileStream]::new($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
    try {
        $record = @{ schema = 'ssmcp-desktop-present-v1'; stage = 'present_returned'; pid = $PID
            frame_index = 1; width = 1008; height = 681; elapsed_ms = 1 } | ConvertTo-Json -Compress
        $bytes = [Text.Encoding]::UTF8.GetBytes($record + [char]10)
        $writer.Write($bytes, 0, $bytes.Length); $writer.Flush()
        $rows = @(Read-PresentReceipts -Path $Path -OwnedProcess $PID)
        if ($rows.Count -ne 1 -or $rows[0]['width'] -ne 1008) { throw 'A live writer blocked the complete frame receipt.' }
        $partial = [byte[]]@(123, 34, 120, 34, 58, 34, 240, 159)
        $writer.Write($partial, 0, $partial.Length); $writer.Flush()
        $rows = @(Read-PresentReceipts -Path $Path -OwnedProcess $PID)
        if ($rows.Count -ne 1) { throw 'A partial trailing UTF-8 record hid a complete receipt.' }
        $writer.SetLength(0); [void]$writer.Seek(0, [IO.SeekOrigin]::Begin)
        $bad = [Text.Encoding]::UTF8.GetBytes('invalid-json' + [char]10)
        $writer.Write($bad, 0, $bad.Length); $writer.Flush()
        $rejected = $false
        try { $null = @(Read-PresentReceipts -Path $Path -OwnedProcess $PID) } catch { $rejected = $true }
        if (-not $rejected) { throw 'A malformed complete frame was accepted.' }
        $writer.SetLength(1048577); $writer.Flush()
        $rejected = $false
        try { $null = @(Read-PresentReceipts -Path $Path -OwnedProcess $PID) } catch { $rejected = $true }
        if (-not $rejected) { throw 'An oversized frame journal was accepted.' }
        return @{ status = 'passed'; writer_open = $true; complete_frame = $true
            partial_utf8_ignored = $true; malformed_rejected = $true; byte_bound = $true }
    } finally {
        $writer.Dispose()
        [IO.File]::Delete($Path)
    }
}
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
    [StructLayout(LayoutKind.Sequential)] public struct Point { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct MonitorInfo { public int Size; public Rect Monitor, Work; public uint Flags; }
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool GetWindowRect(IntPtr window, out Rect bounds);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool GetClientRect(IntPtr window, out Rect bounds);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool ClientToScreen(IntPtr window, ref Point point);
    [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr window, uint flags);
    [DllImport("user32.dll", EntryPoint="GetMonitorInfoW", SetLastError=true)] public static extern bool GetMonitorInfo(IntPtr monitor, ref MonitorInfo info);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr window, uint command);
    // Zero means that the current geometry and unobscured screen region belong
    // to the exact owned foreground window. Check on both sides of the capture.
    public static int CheckCapture(IntPtr window, uint process, Rect expected, Rect region) {
        uint owner;
        if (!IsWindowVisible(window) || GetWindowThreadProcessId(window, out owner) == 0 || owner != process) return 1;
        if (GetForegroundWindow() != window) return 2;
        Rect current;
        if (!GetWindowRect(window, out current) || current.Left != expected.Left || current.Top != expected.Top ||
            current.Right != expected.Right || current.Bottom != expected.Bottom) return 3;
        IntPtr above = GetWindow(window, 3); // GW_HWNDPREV: windows above this one.
        for (int count = 0; above != IntPtr.Zero; count++) {
            if (count >= 256) return 5;
            Rect overlay;
            if (IsWindowVisible(above) && GetWindowRect(above, out overlay) &&
                overlay.Left < region.Right && overlay.Right > region.Left &&
                overlay.Top < region.Bottom && overlay.Bottom > region.Top) return 4;
            above = GetWindow(above, 3);
        }
        return 0;
    }
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
    $report.frame_reader_probe = Test-PresentJournalReader -Path (Join-Path $testRoot 'reader-probe.jsonl')
    $configuration = Join-Path $testRoot 'client.toml'
    [IO.File]::WriteAllText($configuration, '')
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $binaryPath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.WorkingDirectory = $testRoot
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.ArgumentList.Add('--exit-after-ms')
    $process.StartInfo.ArgumentList.Add('20000')
    $firstFramePath = Join-Path $testRoot 'first-frame.json'
    $process.StartInfo.ArgumentList.Add('--first-frame-receipt')
    $process.StartInfo.ArgumentList.Add($firstFramePath)
    $frameReceiptsPath = Join-Path $testRoot 'presented-frames.jsonl'
    $process.StartInfo.ArgumentList.Add('--frame-receipts')
    $process.StartInfo.ArgumentList.Add($frameReceiptsPath)
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
    $report.arguments = @('--exit-after-ms', '20000', '--first-frame-receipt', '<owned-test-root>/first-frame.json', '--frame-receipts', '<owned-test-root>/presented-frames.jsonl')
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
        # Fit the owned window inside its monitor work area. This excludes the
        # taskbar; native decorations are excluded from the client-pixel test.
        $monitor = [SsmcpWindowProbe]::MonitorFromWindow($window, 2)
        $monitorInfo = [SsmcpWindowProbe+MonitorInfo]::new()
        $monitorInfo.Size = [Runtime.InteropServices.Marshal]::SizeOf($monitorInfo)
        if ($monitor -eq [IntPtr]::Zero -or -not [SsmcpWindowProbe]::GetMonitorInfo($monitor, [ref]$monitorInfo)) {
            throw 'Could not obtain the owned window monitor work area.'
        }
        $work = $monitorInfo.Work
        $fitWidth = [Math]::Min($bounds.Right - $bounds.Left, $work.Right - $work.Left)
        $fitHeight = [Math]::Min($bounds.Bottom - $bounds.Top, $work.Bottom - $work.Top)
        if (-not [SsmcpWindowProbe]::SetWindowPos($window, [IntPtr]::Zero, $work.Left, $work.Top, $fitWidth, $fitHeight, 0x4014)) {
            throw 'Could not fit the owned window inside the monitor work area.'
        }
        $frameClock = [Diagnostics.Stopwatch]::StartNew()
        $firstFrame = $null
        while ($frameClock.Elapsed.TotalSeconds -lt 10) {
            $process.Refresh()
            if ($process.HasExited) { break }
            if (Test-Path -LiteralPath $firstFramePath) {
                $candidate = $null
                # The application creates a new file and writes once. A partial
                # read is retried; a complete receipt must identify this PID.
                try { $candidate = Get-Content -LiteralPath $firstFramePath -Raw | ConvertFrom-Json -AsHashtable }
                catch { $candidate = $null }
                if ($null -ne $candidate) {
                    if ($candidate['schema'] -ne 'ssmcp-desktop-first-frame-v1' -or
                        $candidate['stage'] -ne 'present_returned' -or
                        $candidate['pid'] -ne $process.Id -or $candidate['frame_index'] -ne 1 -or
                        $candidate['width'] -le 0 -or $candidate['height'] -le 0) {
                        throw 'The first-frame receipt did not identify this owned process and a completed present call.'
                    }
                    $firstFrame = $candidate
                    break
                }
            }
            Start-Sleep -Milliseconds 50
        }
        if ($null -eq $firstFrame) { throw 'The application did not provide its first-present receipt within the bound.' }
        $receiptPath = Join-Path $evidence 'first-frame.json'
        Copy-Item -LiteralPath $firstFramePath -Destination $receiptPath
        $report.first_frame = [ordered]@{
            receipt = $firstFrame
            file = 'first-frame.json'
            sha256 = (Get-FileHash -LiteralPath $receiptPath -Algorithm SHA256).Hash.ToLowerInvariant()
            wait_ms = [int]$frameClock.Elapsed.TotalMilliseconds
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
        # SWP_ASYNCWINDOWPOS avoids blocking this harness on a hung app thread.
        # A queued resize is not evidence that the requested geometry applied.
        $geometryClock = [Diagnostics.Stopwatch]::StartNew()
        $geometryApplied = $false
        do {
            $process.Refresh()
            if ($process.HasExited) { break }
            if ([SsmcpWindowProbe]::GetWindowRect($window, [ref]$bounds) -and
                $bounds.Left -eq $work.Left -and $bounds.Top -eq $work.Top -and
                $bounds.Right - $bounds.Left -eq $fitWidth -and $bounds.Bottom - $bounds.Top -eq $fitHeight) {
                $geometryApplied = $true
                break
            }
            Start-Sleep -Milliseconds 50
        } while ($geometryClock.Elapsed.TotalSeconds -lt 2)
        if (-not $geometryApplied) { throw 'The asynchronously requested window geometry did not apply within the bound.' }
        $report.window = [ordered]@{
            handle = $window.ToInt64(); pid = $windowOwner; title = $process.MainWindowTitle
            left = $bounds.Left; top = $bounds.Top; right = $bounds.Right; bottom = $bounds.Bottom
            monitor_work_area = @{ left = $work.Left; top = $work.Top; right = $work.Right; bottom = $work.Bottom }
        }
        $left = [Math]::Max($bounds.Left, $work.Left)
        $top = [Math]::Max($bounds.Top, $work.Top)
        $right = [Math]::Min($bounds.Right, $work.Right)
        $bottom = [Math]::Min($bounds.Bottom, $work.Bottom)
        if ($right - $left -lt 200 -or $bottom - $top -lt 150) { throw 'Too little of the owned window is on screen.' }
        $client = [SsmcpWindowProbe+Rect]::new()
        $clientOrigin = [SsmcpWindowProbe+Point]::new()
        if (-not [SsmcpWindowProbe]::GetClientRect($window, [ref]$client) -or
            -not [SsmcpWindowProbe]::ClientToScreen($window, [ref]$clientOrigin)) {
            throw 'Could not map the owned client area to screen pixels.'
        }
        $report.expected_client = [ordered]@{
            width = $client.Right - $client.Left; height = $client.Bottom - $client.Top
            screen_x = $clientOrigin.X; screen_y = $clientOrigin.Y
        }
        # Geometry alone can precede the resize repaint. Wait for an actual
        # completed present of this client size, rather than capture its old frame.
        $renderClock = [Diagnostics.Stopwatch]::StartNew()
        $captureFrame = $null
        do {
            $process.Refresh()
            if ($process.HasExited) { break }
            $frames = @(Read-PresentReceipts -Path $frameReceiptsPath -OwnedProcess $process.Id)
            foreach ($frame in $frames) {
                if ($frame['width'] -eq $client.Right - $client.Left -and
                    $frame['height'] -eq $client.Bottom - $client.Top) {
                    $captureFrame = $frame
                }
            }
            if ($null -ne $captureFrame) { break }
            Start-Sleep -Milliseconds 50
        } while ($renderClock.Elapsed.TotalSeconds -lt 5)
        if ($null -eq $captureFrame) { throw 'The application did not present the resized client dimensions within the bound.' }
        $captureReceiptPath = Join-Path $evidence 'capture-frame.json'
        $captureFrame | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $captureReceiptPath -Encoding utf8
        $report.capture_frame = [ordered]@{
            receipt = $captureFrame; file = 'capture-frame.json'
            client_width = $client.Right - $client.Left; client_height = $client.Bottom - $client.Top
            sha256 = (Get-FileHash -LiteralPath $captureReceiptPath -Algorithm SHA256).Hash.ToLowerInvariant()
            wait_ms = [int]$renderClock.Elapsed.TotalMilliseconds
        }
        $inset = 12
        $clientLeft = [Math]::Max($clientOrigin.X + $inset, $left)
        $clientTop = [Math]::Max($clientOrigin.Y + $inset, $top)
        $clientRight = [Math]::Min($clientOrigin.X + $client.Right - $inset, $right)
        $clientBottom = [Math]::Min($clientOrigin.Y + $client.Bottom - $inset, $bottom)
        if ($clientRight - $clientLeft -lt 160 -or $clientBottom - $clientTop -lt 100) {
            throw 'Too little application client area is available for a content check.'
        }
        $clientRegion = [SsmcpWindowProbe+Rect]::new()
        $clientRegion.Left = $clientLeft; $clientRegion.Top = $clientTop
        $clientRegion.Right = $clientRight; $clientRegion.Bottom = $clientBottom
        $captureClock = [Diagnostics.Stopwatch]::StartNew()
        $captureAttempts = 0
        $nonblank = $false
        $image = Join-Path $evidence 'window.png'
        do {
            $process.Refresh()
            if ($process.HasExited) { throw 'The owned process exited before client-pixel capture.' }
            $captureState = [SsmcpWindowProbe]::CheckCapture($window, $windowOwner, $bounds, $clientRegion)
            if ($captureState -ne 0) {
                $report.status = 'blocked'
                throw "The owned client region is unavailable or obscured before capture (state $captureState)."
            }
            $bitmap = [Drawing.Bitmap]::new($right - $left, $bottom - $top)
            $graphics = [Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
                $captureState = [SsmcpWindowProbe]::CheckCapture($window, $windowOwner, $bounds, $clientRegion)
                if ($captureState -ne 0) {
                    $report.status = 'blocked'
                    throw "The owned client region changed or became obscured during capture (state $captureState)."
                }
                $captureAttempts++
                $histogram = [Collections.Generic.Dictionary[int,int]]::new()
                $samples = [Collections.Generic.List[int]]::new()
                for ($row = 0; $row -lt 48; $row++) {
                    $y = $clientTop - $top + [int][Math]::Floor(($row + 0.5) * ($clientBottom - $clientTop) / 48)
                    for ($column = 0; $column -lt 64; $column++) {
                        $x = $clientLeft - $left + [int][Math]::Floor(($column + 0.5) * ($clientRight - $clientLeft) / 64)
                        $rgb = $bitmap.GetPixel($x, $y).ToArgb() -band 0xFFFFFF
                        $samples.Add($rgb)
                        if ($histogram.ContainsKey($rgb)) { $histogram[$rgb]++ } else { $histogram[$rgb] = 1 }
                    }
                }
                $dominant = ($histogram.GetEnumerator() | Sort-Object -Property Value -Descending | Select-Object -First 1).Key
                $contrasting = 0
                foreach ($rgb in $samples) {
                    $red = [Math]::Abs((($rgb -shr 16) -band 255) - (($dominant -shr 16) -band 255))
                    $green = [Math]::Abs((($rgb -shr 8) -band 255) - (($dominant -shr 8) -band 255))
                    $blue = [Math]::Abs(($rgb -band 255) - ($dominant -band 255))
                    if ([Math]::Max($red, [Math]::Max($green, $blue)) -ge 24) { $contrasting++ }
                }
                $requiredContrast = [Math]::Max(12, [int][Math]::Ceiling($samples.Count * 0.005))
                $nonblank = $histogram.Count -ge 3 -and $contrasting -ge $requiredContrast
                $report.client_pixels = [ordered]@{
                    ready = $nonblank; sample_count = $samples.Count; distinct_colors = $histogram.Count
                    dominant_rgb = ('{0:X6}' -f $dominant); contrasting_samples = $contrasting
                    minimum_channel_difference = 24; required_contrasting_samples = $requiredContrast
                    inset = $inset; left = $clientLeft; top = $clientTop; right = $clientRight; bottom = $clientBottom
                    attempts = $captureAttempts; owned_unobscured_checks = 'before_and_after_capture'
                }
                # Keep the last real frame even on a failed nonblank check.
                $bitmap.Save($image, [Drawing.Imaging.ImageFormat]::Png)
            } finally { $graphics.Dispose(); $bitmap.Dispose() }
            if ($nonblank) { break }
            Start-Sleep -Milliseconds 100
        } while ($captureClock.Elapsed.TotalSeconds -lt 6)
        $report.screenshot = [ordered]@{
            file = 'window.png'; method = 'CopyFromScreen of the foreground owned window inside the monitor work area'
            left = $left; top = $top; right = $right; bottom = $bottom
            clipped_to_work_area = ($left -ne $bounds.Left -or $top -ne $bounds.Top -or $right -ne $bounds.Right -or $bottom -ne $bounds.Bottom)
            sha256 = (Get-FileHash -LiteralPath $image -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        if (-not $nonblank) { throw 'The application client area remained blank after its first-present receipt.' }
    }
    if (-not $process.WaitForExit(25000)) { throw 'The bounded desktop process did not exit normally.' }
    $report.exit_code = $process.ExitCode
    if ($report.status -ne 'blocked') {
        if ($process.ExitCode -ne 0) { throw "The desktop executable exited with $($process.ExitCode)." }
        $report.status = 'passed'
    }
} catch {
    $report.error = $_.Exception.Message
    if ($report.status -ne 'blocked') { $report.status = 'failed' }
} finally {
    $report.frame_reader_io = $script:presentReadDiagnostics
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
    if ($null -ne $frameReceiptsPath -and (Test-Path -LiteralPath $frameReceiptsPath)) {
        try {
            $journal = Join-Path $evidence 'presented-frames.jsonl'
            Copy-Item -LiteralPath $frameReceiptsPath -Destination $journal
            $report.frame_receipts = @{
                file = 'presented-frames.jsonl'; bytes = (Get-Item -LiteralPath $journal).Length
                sha256 = (Get-FileHash -LiteralPath $journal -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        } catch { $report.cleanup_error = $_.Exception.Message; $report.status = 'failed' }
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
