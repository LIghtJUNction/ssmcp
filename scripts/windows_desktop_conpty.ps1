# Loaded by e2e_windows_desktop.ps1 after the runner has verified its interactive desktop.
function Test-DesktopConPty {
    param([string]$Binary, [string]$Root, [string]$Evidence)

    $directory = Join-Path $Root 'conpty'
    New-Item -ItemType Directory -Path $directory | Out-Null
    $desktop = Join-Path $directory 'ssmcp-desktop.exe'
    $probe = Join-Path $directory 'ssmcp.exe'
    Copy-Item -LiteralPath $Binary -Destination $desktop
    & rustc --edition=2024 (Join-Path $PSScriptRoot 'fixtures/windows_desktop_conpty.rs') -o $probe
    if ($LASTEXITCODE -ne 0) { throw 'The console probe did not build.' }
    Add-Type -AssemblyName System.Windows.Forms

    $app = [Diagnostics.Process]::new()
    $app.StartInfo.UseShellExecute = $false
    $app.StartInfo.FileName = $desktop
    $app.StartInfo.WorkingDirectory = $directory
    foreach ($item in @{
        HOME = "$Root/home"; USERPROFILE = "$Root/home"; APPDATA = "$Root/roaming"
        LOCALAPPDATA = "$Root/local"; SSMCP_SESSION_HOME = "$Root/sessions"
        SSMCP_AUTH_HOME = "$Root/auth"; SSMCP_CLIENT_CONFIG = "$Root/client.toml"
        SSMCP_TEST_CONPTY_DIR = $directory
    }.GetEnumerator()) { $app.StartInfo.Environment[$item.Key] = $item.Value }
    $child = $null
    $started = $false
    function Read-ConPtyRecord([string]$Stage) {
        $record = Join-Path $directory $Stage
        $clock = [Diagnostics.Stopwatch]::StartNew()
        while (-not [IO.File]::Exists($record)) {
            if ($app.HasExited -or $clock.Elapsed.TotalSeconds -ge 15) { throw "Missing ConPTY record: $Stage" }
            Start-Sleep -Milliseconds 80
        }
        $value = [IO.File]::ReadAllText($record).Trim()
        if ($value -notmatch '^(\d+) ([1-9]\d*) ([1-9]\d*)$') { throw "Invalid ConPTY record: $Stage" }
        return @{ pid = [int]$Matches[1]; cols = [int]$Matches[2]; rows = [int]$Matches[3] }
    }
    function Send-OwnedKeys([string]$Text) {
        if ([SsmcpWindowProbe]::GetForegroundWindow() -ne $window) { throw 'The owned window lost keyboard focus.' }
        [Windows.Forms.SendKeys]::SendWait($Text)
        [Windows.Forms.SendKeys]::SendWait('{ENTER}')
    }
    function Get-MemoryReceipt([Diagnostics.Process]$Owner) {
        $Owner.Refresh()
        return @{ working_set_bytes = $Owner.WorkingSet64; private_bytes = $Owner.PrivateMemorySize64 }
    }

    try {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        if (-not $app.Start()) { throw 'The bare desktop did not launch.' }
        $started = $true
        do {
            $app.Refresh()
            $window = $app.MainWindowHandle
            if ($window -ne [IntPtr]::Zero -and [SsmcpWindowProbe]::IsWindowVisible($window)) {
                [void][SsmcpWindowProbe]::SetForegroundWindow($window)
                if ([SsmcpWindowProbe]::GetForegroundWindow() -eq $window) { break }
            }
            if ($app.HasExited -or $clock.Elapsed.TotalSeconds -ge 15) { throw 'The bare desktop did not present a focused window.' }
            Start-Sleep -Milliseconds 80
        } while ($true)
        $visibleAt = $clock.ElapsedMilliseconds
        $firstMemory = Get-MemoryReceipt $app
        Send-OwnedKeys 'fixture@http://127.0.0.1:9'
        $ready = Read-ConPtyRecord 'ready'
        $child = [Diagnostics.Process]::GetProcessById($ready.pid)
        if ($child.MainModule.FileName -ne $probe) { $child.Dispose(); $child = $null; throw 'Unexpected child executable.' }
        if ($child.SessionId -ne $app.SessionId) { throw 'The probe belongs to another desktop session.' }
        Send-OwnedKeys 'keys-first'
        $first = Read-ConPtyRecord 'first'
        if ($first.pid -ne $ready.pid) { throw 'Changing child PID before resize.' }
        $memory = @{ desktop = Get-MemoryReceipt $app; child = Get-MemoryReceipt $child }

        # Keep the same position and focus; change the grid substantially.
        $bounds = [SsmcpWindowProbe+Rect]::new()
        if (-not [SsmcpWindowProbe]::GetWindowRect($window, [ref]$bounds)) { throw 'Missing owned window geometry.' }
        $width = if ($bounds.Right - $bounds.Left -gt 720) { 630 } else { 810 }
        if (-not [SsmcpWindowProbe]::SetWindowPos($window, [IntPtr]::Zero, 0, 0, $width, 430, 0x0016)) { throw 'Window resize failed.' }
        # A renderer-visible child output marker also establishes the terminal drew before the second input.
        $clientOrigin = [SsmcpWindowProbe+Point]::new()
        if (-not [SsmcpWindowProbe]::ClientToScreen($window,[ref]$clientOrigin)) { throw 'Missing client origin.' }
        if (-not [SsmcpWindowProbe]::GetWindowRect($window, [ref]$bounds)) { throw 'Missing post-resize window geometry.' }
        $region = [SsmcpWindowProbe+Rect]::new()
        $region.Left = $clientOrigin.X; $region.Top = $clientOrigin.Y
        $region.Right = $region.Left + 180; $region.Bottom = $region.Top + 60
        $pixels = [Drawing.Bitmap]::new(180,60)
        try {
            $graphics = [Drawing.Graphics]::FromImage($pixels)
            try {
                $seen = $false
                $watch = [Diagnostics.Stopwatch]::StartNew()
                do {
                    if ([SsmcpWindowProbe]::CheckCapture($window, $app.Id, $bounds, $region) -ne 0) { throw 'The captured area is not an unobscured part of the owned window.' }
                    $graphics.CopyFromScreen($clientOrigin.X, $clientOrigin.Y, 0, 0, $pixels.Size)
                    if ([SsmcpWindowProbe]::CheckCapture($window, $app.Id, $bounds, $region) -ne 0) { throw 'Window ownership changed during the capture.' }
                    for ($y = 2; $y -lt 60 -and -not $seen; $y += 2) {
                        $hits = 0
                        for ($x = 3; $x -lt 180; $x += 3) {
                            $c = $pixels.GetPixel($x,$y)
                            if ($c.G -gt 130 -and $c.G - $c.R -gt 70 -and $c.G - $c.B -gt 50) { $hits++ }
                        }
                        $seen = $hits -ge 12
                    }
                    if (-not $seen) { Start-Sleep -Milliseconds 70 }
                } until ($seen -or $watch.Elapsed.TotalSeconds -gt 8)
                if (-not $seen) { throw 'The window did not paint the VT color sent by its ConPTY child.' }
            } finally { $graphics.Dispose() }
            $pixels.Save((Join-Path $Evidence 'conpty-output.png'), [Drawing.Imaging.ImageFormat]::Png)
        } finally { $pixels.Dispose() }
        $changed = Read-ConPtyRecord 'grid-changed'
        if ($changed.pid -ne $ready.pid -or $changed.cols -eq $first.cols) { throw 'The new terminal size was not delivered.' }
        Send-OwnedKeys 'keys-resized'
        $resized = Read-ConPtyRecord 'resized'
        if ($resized.pid -ne $ready.pid -or $first.cols -eq $resized.cols) { throw 'The same child did not observe a changed PTY width.' }
        if (-not $app.CloseMainWindow() -or -not $app.WaitForExit(10000) -or $app.ExitCode -ne 0) { throw 'Windows desktop did not exit cleanly.' }
        if (-not $child.WaitForExit(10000)) { throw 'The ConPTY child survived its desktop.' }
        return @{ status = 'passed'; desktop_argv = @(); window_visible_ms = $visibleAt; standalone_memory = $firstMemory
            connected_memory = $memory; grid_before = $first; grid_after = $resized; input_round_trips = 2
            child_exit_with_window = $true; desktop_exit = $app.ExitCode; output_image = 'conpty-output.png'
            sibling = 'Disposable Rust console probe'; probe_sha256 = (Get-FileHash -LiteralPath $probe).Hash.ToLowerInvariant() }
    } finally {
        if ($started -and -not $app.HasExited) { $app.Kill($true); [void]$app.WaitForExit(5000) }
        if ($null -ne $child) { if (-not $child.HasExited) { $child.Kill(); [void]$child.WaitForExit(5000) }; $child.Dispose() }
        $app.Dispose()
    }
}
