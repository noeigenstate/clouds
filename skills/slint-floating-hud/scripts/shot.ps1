# Starts xiaobaiyun (optionally with --translate), hit-tests the cloud window
# and saves a DPI-aware screenshot of it next to this script.
param([Parameter(Mandatory)][string]$Exe, [string]$Text = "", [int]$Wait = 5, [string]$Out = "cloud.png")
$dir = Split-Path $MyInvocation.MyCommand.Path
Get-Process xiaobaiyun -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
if ($Text) { $p = Start-Process $Exe -ArgumentList @("--translate", "`"$Text`"") -PassThru } else { $p = Start-Process $Exe -PassThru }
Start-Sleep -Seconds $Wait
Add-Type @"
using System; using System.Runtime.InteropServices;
public class HudShot {
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 public struct RECT { public int L,T,R,B; }
 public struct P { public int X,Y; }
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(P p);
 [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint f);
}
"@
[HudShot]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id $p.Id).MainWindowHandle
$r = New-Object HudShot+RECT; [HudShot]::GetWindowRect($h, [ref]$r) | Out-Null
"window: $($r.L),$($r.T) - $($r.R),$($r.B)"
foreach ($pt in @(@(($r.L + 5), ($r.T + 5), "top-left corner"), @(($r.L + 260), ($r.T + 160), "cloud body"), @(($r.R - 5), ($r.B - 5), "bottom-right corner"))) {
  $hit = [HudShot]::GetAncestor([HudShot]::WindowFromPoint((New-Object HudShot+P -Property @{X=$pt[0]; Y=$pt[1]})), 2)
  "  $($pt[2]): " + $(if ($hit -eq $h) { "cloud" } else { "passes through" })
}
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L + 40), ($r.B - $r.T + 40)
$g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L - 20, $r.T - 20, 0, 0, $bmp.Size); $bmp.Save("$dir\$Out")
