# Only the dedicated diagnostic ConPTY runs this bootstrap. Production-shell
# cases in the same test keep their normal launcher, profiles and key bindings.
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
public static class CwdConsoleProbe {
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int number);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool GetConsoleMode(IntPtr handle, out uint mode);
    public static uint Mode(int number) {
        uint mode;
        if (!GetConsoleMode(GetStdHandle(number), out mode))
            throw new Win32Exception(Marshal.GetLastWin32Error());
        return mode;
    }
    // Rust creates sample.request after seeing the complete command echo.
    // Read the same console handles while its editor is waiting for Enter.
    static FileSystemWatcher watcher;
    public static void Arm(string dir) {
        watcher = new FileSystemWatcher(dir, "sample.request");
        watcher.Created += delegate {
            string value;
            try { value = Mode(-10) + "," + Mode(-11); }
            catch (Exception e) { value = "ERROR " + e.Message; }
            string loaded = "not present";
            foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies()) {
                var name = assembly.GetName();
                if (name.Name.IndexOf("PSReadLine", StringComparison.OrdinalIgnoreCase) >= 0)
                    loaded = name.FullName;
            }
            File.WriteAllText(Path.Combine(dir, "editor.txt"), loaded);
            string tmp = Path.Combine(dir, "modes.tmp");
            File.WriteAllText(tmp, value);
            File.Move(tmp, Path.Combine(dir, "modes.txt"));
        };
        watcher.EnableRaisingEvents = true;
    }
}
'@
[CwdConsoleProbe]::Arm($env:SSMCP_CWD_PROBE)
function global:prompt {
    $dest = Join-Path $env:SSMCP_CWD_PROBE 'prompt.json'
    if (Test-Path -LiteralPath $dest) { return "PS $($PWD.Path)> " }
    $mod = Get-Module -Name PSReadLine | Select-Object -First 1
    $info = [ordered]@{
        engine = $PSVersionTable.PSVersion.ToString()
        hostName = $Host.Name
        cwd = $PWD.Path
        promptInput = [CwdConsoleProbe]::Mode(-10)
        promptOutput = [CwdConsoleProbe]::Mode(-11)
        psReadLine = 'unloaded'
        enter = $null
    }
    if ($mod) {
        $info.psReadLine = $mod.Version.ToString()
        $info.enter = @(Get-PSReadLineKeyHandler -Bound | Where-Object { $_.Key -eq 'Enter' } |
            ForEach-Object { $_.Function })
    }
    [IO.File]::WriteAllText($dest,
        ($info | ConvertTo-Json -Compress))
    'PS ' + $PWD.Path + '> '
}
