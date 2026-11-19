# Exercise the real installer helpers without downloads, packages, PATH or user files.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repository = Split-Path $PSScriptRoot -Parent
$source = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'install.ps1'))
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ('Installer has syntax errors: ' + ($errors | Out-String)) }
$helpers = @('Wait-DownloadTask', 'Receive-BoundedStream', 'Invoke-BoundedDownload', 'Expand-VerifiedRelease')
foreach ($name in $helpers) {
    $function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    if (-not $function) { throw "Missing production helper: $name" }
    . ([ScriptBlock]::Create($function.Extent.Text))
}

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
public sealed class SsmcpDelayedStream : MemoryStream {
    readonly int delay;
    readonly int perRead;
    public SsmcpDelayedStream(byte[] bytes, int delayMs, int perReadBytes) : base(bytes) {
        delay = delayMs; perRead = perReadBytes;
    }
    public override async Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken token) {
        await Task.Delay(delay, token).ConfigureAwait(false);
        return await base.ReadAsync(buffer, offset, Math.Min(count, perRead), token).ConfigureAwait(false);
    }
}
'@

$root = Join-Path ([IO.Path]::GetTempPath()) ('ssmcp-helper-fixture-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($root) | Out-Null
function Assert-Failure {
    param([ScriptBlock]$Action, [string]$Contains)
    $failed = $false
    try { & $Action } catch {
        $failed = $true
        if ($_.Exception.Message -notlike "*$Contains*") { throw "Unexpected failure: $($_.Exception.Message)" }
    }
    if (-not $failed) { throw "Expected failure containing: $Contains" }
}
function Write-FixtureZip {
    param([string]$Path, [string[]]$Names)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Add-Type -AssemblyName System.IO.Compression
    $zip = [IO.Compression.ZipFile]::Open($Path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($name in $Names) {
            $entry = $zip.CreateEntry($name)
            $stream = $entry.Open()
            try { $stream.WriteByte(42) } finally { $stream.Dispose() }
        }
    } finally { $zip.Dispose() }
}
try {
    $file = Join-Path $root 'body'
    $cancellation = New-Object Threading.CancellationTokenSource
    $bytes = New-Object byte[] 1024
    $stream = [IO.MemoryStream]::new($bytes)
    try {
        Receive-BoundedStream $stream $file 1024 ([Diagnostics.Stopwatch]::StartNew()) $cancellation.Token
        if ((Get-Item -LiteralPath $file).Length -ne 1024) { throw 'Exact-limit download was truncated.' }
    } finally { $stream.Dispose() }

    $stream = [IO.MemoryStream]::new((New-Object byte[] 1025))
    try {
        Assert-Failure { Receive-BoundedStream $stream $file 1024 ([Diagnostics.Stopwatch]::StartNew()) $cancellation.Token } 'size limit'
        if ((Get-Item -LiteralPath $file).Length -gt 1024) { throw 'Streaming size bound was exceeded on disk.' }
    } finally { $stream.Dispose() }

    foreach ($kind in @('idle', 'total', 'rate')) {
        $cancellation.Dispose()
        $cancellation = New-Object Threading.CancellationTokenSource
        $delay = if ($kind -eq 'rate') { 10 } else { 1000 }
        $stream = [SsmcpDelayedStream]::new((New-Object byte[] 100), $delay, 1)
        $clock = [Diagnostics.Stopwatch]::StartNew()
        try {
            if ($kind -eq 'idle') {
                Assert-Failure { Receive-BoundedStream $stream $file 1024 $clock $cancellation.Token -IdleMs 40 -TotalMs 2000 } 'deadline'
            } elseif ($kind -eq 'total') {
                Assert-Failure { Receive-BoundedStream $stream $file 1024 $clock $cancellation.Token -IdleMs 2000 -TotalMs 40 } 'deadline'
            } else {
                Assert-Failure { Receive-BoundedStream $stream $file 1024 $clock $cancellation.Token -RateWindowMs 50 -MinimumRate 1000 } 'minimum transfer rate'
            }
            if ($clock.ElapsedMilliseconds -gt 1500) { throw "$kind abort waited too long." }
        } finally {
            $cancellation.Cancel()
            $stream.Dispose()
        }
    }
    $cancellation.Dispose()
    $never = New-Object 'Threading.Tasks.TaskCompletionSource[int]'
    Assert-Failure { Wait-DownloadTask $never.Task ([Diagnostics.Stopwatch]::StartNew()) 40 2000 'connection/headers' } 'deadline'

    # Only the exact root executable is extracted; unused path aliases never become destinations.
    $zipPath = Join-Path $root 'release.zip'
    Write-FixtureZip $zipPath @('ssmcp.exe', '../escape', ' .. /escape', 'folder/../../escape')
    $destination = Join-Path $root 'release-out'
    Expand-VerifiedRelease $zipPath $destination
    if ((Get-Item -LiteralPath (Join-Path $destination 'ssmcp.exe')).Length -ne 1) { throw 'Exact root entry was not extracted.' }
    if (Test-Path -LiteralPath (Join-Path $root 'escape')) { throw 'Archive escaped its owned directory.' }
    foreach ($alias in @('ssmcp.exe. ', 'SSMCP.EXE', './ssmcp.exe', 'folder/../ssmcp.exe')) {
        $path = Join-Path $root ([Guid]::NewGuid().ToString('N') + '.zip')
        Write-FixtureZip $path @($alias)
        Assert-Failure { Expand-VerifiedRelease $path (Join-Path $root 'rejected') } 'exactly one root'
    }
    $duplicate = Join-Path $root 'duplicate.zip'
    Write-FixtureZip $duplicate @('ssmcp.exe', 'ssmcp.exe')
    Assert-Failure { Expand-VerifiedRelease $duplicate (Join-Path $root 'rejected') } 'exactly one root'

    Write-Host 'Installer helpers passed: exact byte cap, idle/total/header/low-rate deadlines, root-entry extraction, and duplicate rejection.'
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force
}
