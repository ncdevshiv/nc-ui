param(
    [Parameter(Mandatory=$true)][string]$WindowTitle,
    [Parameter(Mandatory=$true)][string]$OutPng,
    [Parameter(Mandatory=$true)][string]$SamplesFile
)
# Captures the client area of the top-level window whose title contains -WindowTitle to
# -OutPng, then samples the client-relative physical-pixel points named in -SamplesJson
# (a JSON array of {"x":..,"y":..}) and prints the result as JSON. F5 evidence: the PNG is
# the artifact; the samples are what the test asserts on.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;
public static class WindowCapture {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lparam);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    public struct POINT { public int X; public int Y; }
    public static IntPtr FindByTitle(string fragment) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, lp) => {
            if (!IsWindowVisible(hwnd)) return true;
            var sb = new StringBuilder(512);
            GetWindowText(hwnd, sb, 512);
            if (sb.ToString().Contains(fragment)) { found = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
'@
$hwnd = [WindowCapture]::FindByTitle($WindowTitle)
if ($hwnd -eq [IntPtr]::Zero) { throw "no visible window titled like '$WindowTitle'" }
# Occlusion is the enemy, not focus: a screen capture reads whatever pixels are on top,
# so the window is made topmost for the capture's duration and then put back. SetWindowPos
# with NOACTIVATE is not subject to the foreground lock that makes SetForegroundWindow a
# no-op for a process that did not receive the last input event.
$HWND_TOPMOST = [IntPtr]::new(-1)
$HWND_NOTOPMOST = [IntPtr]::new(-2)
$SWP_NOMOVE = 0x0002; $SWP_NOSIZE = 0x0001; $SWP_NOACTIVATE = 0x0010
[WindowCapture]::SetWindowPos($hwnd, $HWND_TOPMOST, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_NOACTIVATE)) | Out-Null
[WindowCapture]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 350
$rect = New-Object WindowCapture+RECT
[WindowCapture]::GetClientRect($hwnd, [ref]$rect) | Out-Null
$origin = New-Object WindowCapture+POINT
$origin.X = 0; $origin.Y = 0
[WindowCapture]::ClientToScreen($hwnd, [ref]$origin) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) { throw "client area is empty (${width}x${height})" }
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($origin.X, $origin.Y, 0, 0, (New-Object System.Drawing.Size $width, $height))
$bitmap.Save($OutPng, [System.Drawing.Imaging.ImageFormat]::Png)
$samples = Get-Content -LiteralPath $SamplesFile -Raw | ConvertFrom-Json
$result = @()
foreach ($sample in $samples) {
    $sx = [int]$sample.x; $sy = [int]$sample.y
    if ($sx -lt 0 -or $sy -lt 0 -or $sx -ge $width -or $sy -ge $height) {
        $result += @{ x = $sx; y = $sy; outside = $true; r = -1; g = -1; b = -1 }
        continue
    }
    $pixel = $bitmap.GetPixel($sx, $sy)
    $result += @{ x = $sx; y = $sy; outside = $false; r = $pixel.R; g = $pixel.G; b = $pixel.B }
}
$graphics.Dispose()
$bitmap.Dispose()
[WindowCapture]::SetWindowPos($hwnd, $HWND_NOTOPMOST, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_NOACTIVATE)) | Out-Null
@{ width = $width; height = $height; samples = $result } | ConvertTo-Json -Compress
