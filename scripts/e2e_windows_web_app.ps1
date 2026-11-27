[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [Parameter(Mandatory)][string]$SourceCommit
)
$ErrorActionPreference = 'Stop'
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$core = Join-Path (Split-Path $binaryPath) 'ssmcp.exe'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ("ssmcp-web-app-" + [guid]::NewGuid().ToString('N'))
$evidence = New-Item -ItemType Directory -Force -Path $EvidenceDirectory
$owned = [Collections.Generic.List[Diagnostics.Process]]::new()
$relay = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$report = [ordered]@{ source_commit = $SourceCommit; status = 'failed'; windows = @() }

function Open-Workbench([bool]$fromCli) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = if ($fromCli) { $core } else { $binaryPath }
    $info.UseShellExecute = $false
    if ($fromCli) { $info.ArgumentList.Add('gui') }
    $info.WorkingDirectory = $fixture
    foreach ($name in @('HOME','USERPROFILE','APPDATA','LOCALAPPDATA')) {
        $info.Environment[$name] = Join-Path $fixture 'home'
    }
    $info.Environment['SSMCP_SESSION_HOME'] = Join-Path $fixture 'sessions'
    $info.Environment['SSMCP_AUTH_HOME'] = Join-Path $fixture 'auth'
    $start = [Diagnostics.Process]::Start($info)
    $owned.Add($start)
    return $start
}

function Find-Form($owner, $previous) {
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while ($deadline.Elapsed.TotalSeconds -lt 45) {
        if ($owner.HasExited) { throw 'Desktop launcher exited before rendering its form.' }
        $browsers = @(Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" |
            Where-Object {
                $_.CommandLine -like "*$fixture*" -and $_.CommandLine -match '--app=' -and
                $_.CommandLine -notmatch '--type=' -and $_.ProcessId -ne $previous
            })
        foreach ($row in $browsers) {
            if ($row.CommandLine -match '#[a-f0-9]{64}') { throw 'Pairing credential appeared in browser argv.' }
            $browser = Get-Process -Id $row.ProcessId -ErrorAction SilentlyContinue
            if (!$browser) { continue }
            $handle = $browser.MainWindowHandle
            if ($handle -eq [IntPtr]::Zero) { continue }
            $root = [Windows.Automation.AutomationElement]::FromHandle($handle)
            $desc = [Windows.Automation.TreeScope]::Descendants
            $edits = $root.FindAll($desc, [Windows.Automation.PropertyCondition]::new(
                [Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Edit))
            $labels = @($edits | ForEach-Object { $_.Current.Name })
            if ('用户名' -notin $labels -or '服务器地址' -notin $labels) { continue }
            $buttons = $root.FindAll($desc, [Windows.Automation.PropertyCondition]::new(
                [Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Button))
            if ('连接' -notin @($buttons | ForEach-Object { $_.Current.Name })) { continue }
            $gateway = Get-CimInstance Win32_Process -Filter "ProcessId=$($row.ParentProcessId)"
            if (!$gateway -or $gateway.ExecutablePath -ine $core) { throw 'Web window did not originate from the packaged sibling core.' }
            return @{ browser = $browser; handle = $handle; gateway = Get-Process -Id $gateway.ProcessId }
        }
        Start-Sleep -Milliseconds 250
    }
    throw 'The actual system Edge window did not render the shared Chinese connection form.'
}

function Control($window, $kind, [string]$name) {
    $root = [Windows.Automation.AutomationElement]::FromHandle($window.handle)
    $condition = [Windows.Automation.AndCondition]::new(
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, $kind),
        [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $name))
    return $root.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
}

function Owned-Core($gateway) {
    return @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($gateway.Id) AND Name='ssmcp.exe'" |
        Where-Object { $_.ExecutablePath -ieq $core -and $_.CommandLine -match '__frontend-run' })
}

function Submit-Loopback($window, [string]$origin) {
    $edit = [Windows.Automation.ControlType]::Edit
    $button = [Windows.Automation.ControlType]::Button
    $value = [Windows.Automation.ValuePattern]::Pattern
    $invoke = [Windows.Automation.InvokePattern]::Pattern
    (Control $window $edit '用户名').GetCurrentPattern($value).SetValue('desktop-test')
    $address = (Control $window $edit '服务器地址').GetCurrentPattern($value)
    if ($address.Current.Value -cne 'ssmcp.lmm.best') { throw 'Unexpected default relay.' }
    (Control $window $button '连接').GetCurrentPattern($invoke).Invoke()
    $wait = [Diagnostics.Stopwatch]::StartNew()
    do {
        $root = [Windows.Automation.AutomationElement]::FromHandle($window.handle)
        $checks = $root.FindAll([Windows.Automation.TreeScope]::Descendants,
            [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::CheckBox))
        if ($checks.Count -ge 2) { break }
        Start-Sleep -Milliseconds 150
    } until ($wait.Elapsed.TotalSeconds -ge 8)
    if ($checks.Count -lt 2 -or @(Owned-Core $window.gateway).Count) {
        throw 'Hosted connection did not require terms and privacy consent before core launch.'
    }
    $address.SetValue($origin)
    (Control $window $button '连接').GetCurrentPattern($invoke).Invoke()
    $wait.Restart()
    do {
        $starts = @(Owned-Core $window.gateway)
        if ($starts.Count) { break }
        Start-Sleep -Milliseconds 200
    } until ($wait.Elapsed.TotalSeconds -ge 20)
    if ($starts.Count -ne 1 -or !$starts[0].CommandLine.Contains("desktop-test@$origin")) {
        throw 'The window did not create exactly one core for the selected loopback relay.'
    }
    return Get-Process -Id $starts[0].ProcessId
}

try {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    New-Item -ItemType Directory -Path "$fixture/home" -Force | Out-Null
    $relay.Start()
    $origin = 'http://127.0.0.1:' + $relay.LocalEndpoint.Port
    $first = Open-Workbench $false
    $a = Find-Form $first 0
    $second = Open-Workbench $true
    $b = Find-Form $second $a.browser.Id
    if ($a.gateway.Id -eq $b.gateway.Id) { throw 'Independent windows shared a gateway.' }
    $report.windows = @(
        @{ entry = 'ssmcp-desktop.exe'; browser_pid = $a.browser.Id; gateway_pid = $a.gateway.Id }
        @{ entry = 'ssmcp.exe gui'; browser_pid = $b.browser.Id; gateway_pid = $b.gateway.Id }
    )
    $session = Submit-Loopback $a $origin
    $report.hosted_consent_before_start = $true
    $report.loopback_core_pid = $session.Id
    if (!$a.browser.CloseMainWindow() -or !$first.WaitForExit(12000) -or $first.ExitCode -ne 0) {
        throw 'Closing the first Edge app did not end its launcher cleanly.'
    }
    if (!$session.WaitForExit(8000) -or !$a.gateway.HasExited -or $second.HasExited -or $b.browser.HasExited -or $b.gateway.HasExited) {
        throw 'Window closure did not release exactly its own gateway.'
    }
    if (!$b.browser.CloseMainWindow() -or !$second.WaitForExit(12000) -or $second.ExitCode -ne 0) {
        throw 'Closing the other application did not end its launcher cleanly.'
    }
    $report.status = 'passed'
} finally {
    $relay.Stop()
    foreach ($process in $owned) {
        if (!$process.HasExited) { $process.Kill($true); [void]$process.WaitForExit(5000) }
        $process.Dispose()
    }
    $report | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 -LiteralPath (Join-Path $evidence.FullName 'result.json')
    if (Test-Path $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
