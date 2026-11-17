# Served from the account server. Run the client temporarily without installation.
[CmdletBinding()]
param(
    [string]$Server,
    [Parameter(Mandatory = $true)][string]$Token,
    [string]$Name,
    [switch]$NoWatchdog,
    [ValidateRange(1,3600)][int]$ReconnectDelay = 5
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$downloadOrigin = @@ORIGIN@@
if (-not $Server) { $Server = $downloadOrigin }
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Use the shell temporary command on Linux and macOS.'
}
try { $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
catch { $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
$target = switch ($architecture.ToLowerInvariant()) {
    'x64' { 'x86_64-pc-windows-msvc' }
    'amd64' { 'x86_64-pc-windows-msvc' }
    'arm64' { 'aarch64-pc-windows-msvc' }
    default { throw 'This Windows CPU architecture is not supported.' }
}
$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-join-' + [Guid]::NewGuid().ToString('N'))
$binaryPath = Join-Path $temporaryDirectory 'ssmcp.exe'
$handler = $null
$client = $null
$response = $null
$source = $null
$output = $null
$cancellation = New-Object Threading.CancellationTokenSource
$cancellation.CancelAfter(180000)
try {
    [IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
    Add-Type -AssemblyName System.Net.Http
    $handler = New-Object Net.Http.HttpClientHandler
    $handler.AllowAutoRedirect = $false
    $handler.UseCookies = $false
    $handler.UseDefaultCredentials = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(180)
    $response = $client.GetAsync("$downloadOrigin/client/bin/$target", [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancellation.Token).GetAwaiter().GetResult()
    if (-not $response.IsSuccessStatusCode) {
        throw "Cannot download the client for $target from this server. Ask its administrator to configure client_download_dir."
    }
    $declaredLength = $response.Content.Headers.ContentLength
    if ($null -ne $declaredLength -and ($declaredLength -le 0 -or $declaredLength -gt 268435456)) {
        throw 'The client download has an invalid size.'
    }
    $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
    $output = [IO.File]::Open($binaryPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $buffer = New-Object byte[] 65536
    [long]$received = 0
    while (($count = $source.ReadAsync($buffer, 0, $buffer.Length, $cancellation.Token).GetAwaiter().GetResult()) -gt 0) {
        $received += $count
        if ($received -gt 268435456) { throw 'The client download exceeds its size limit.' }
        $output.Write($buffer, 0, $count)
    }
    $output.Dispose(); $output = $null
    if ($received -eq 0 -or ($null -ne $declaredLength -and $received -ne $declaredLength)) {
        throw 'The client download is empty or incomplete.'
    }
    $joinArguments = @('join', '--server', $Server, '--token', $Token)
    if ($Name) { $joinArguments += @('--name', $Name) }
    if ($NoWatchdog) { $joinArguments += '--no-watchdog' }
    else { $joinArguments += @('--reconnect-delay', $ReconnectDelay.ToString()) }
    & $binaryPath @joinArguments
    if ($LASTEXITCODE -ne 0) { throw "The temporary client exited with code $LASTEXITCODE." }
} finally {
    $cancellation.Cancel()
    if ($output) { $output.Dispose() }
    if ($source) { $source.Dispose() }
    if ($response) { $response.Dispose() }
    if ($client) { $client.Dispose() }
    elseif ($handler) { $handler.Dispose() }
    $cancellation.Dispose()
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
