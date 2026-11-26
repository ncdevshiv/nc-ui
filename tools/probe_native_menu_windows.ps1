param(
    [string]$Application = 'examples/counter',
    [string]$WindowTitle = 'NC-UI Counter*',
    [string]$ItemLabel = 'Increment',
    [string]$ExpectedText = 'native menu selected;increment'
)
$ErrorActionPreference = 'Stop'
$expectedFragments = @($ExpectedText -split ';' | Where-Object { $_ })
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root 'target/release/ncui.exe'
if (-not (Test-Path -LiteralPath $app)) {
    throw "Build the Windows host first: cargo build --release -p ncui-cli --features ncui-cli/wry-host"
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class NativeMenuProbe {
    [DllImport("user32.dll")] public static extern IntPtr GetMenu(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetSubMenu(IntPtr menu, int index);
    [DllImport("user32.dll")] public static extern int GetMenuItemCount(IntPtr menu);
    [DllImport("user32.dll")] public static extern uint GetMenuItemID(IntPtr menu, int index);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetMenuString(IntPtr menu, int id, StringBuilder text, int max, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
}
'@

$process = Start-Process -FilePath $app -ArgumentList @('run', $Application) -WorkingDirectory $root -PassThru
try {
    $automationRoot = [System.Windows.Automation.AutomationElement]::RootElement
    $window = $null
    for ($attempt = 0; $attempt -lt 150 -and $null -eq $window; $attempt++) {
        $condition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
            $process.Id
        )
        $candidates = $automationRoot.FindAll([System.Windows.Automation.TreeScope]::Children, $condition)
        foreach ($candidate in $candidates) {
            if ($candidate.Current.Name -like $WindowTitle) {
                $window = $candidate
                break
            }
        }
        if ($null -eq $window) { Start-Sleep -Milliseconds 100 }
    }
    if ($null -eq $window) { throw 'The counter window did not open' }

    $hwnd = $window.Current.NativeWindowHandle
    $menu = [NativeMenuProbe]::GetMenu($hwnd)
    for ($attempt = 0; $attempt -lt 100 -and $menu -eq [IntPtr]::Zero; $attempt++) {
        Start-Sleep -Milliseconds 100
        $menu = [NativeMenuProbe]::GetMenu($hwnd)
    }
    if ($menu -eq [IntPtr]::Zero) { throw 'The live window has no native menu' }
    $commandId = [uint32]0
    for ($menuIndex = 0; $menuIndex -lt [NativeMenuProbe]::GetMenuItemCount($menu); $menuIndex++) {
        $submenu = [NativeMenuProbe]::GetSubMenu($menu, $menuIndex)
        if ($submenu -eq [IntPtr]::Zero) { continue }
        for ($itemIndex = 0; $itemIndex -lt [NativeMenuProbe]::GetMenuItemCount($submenu); $itemIndex++) {
            $label = New-Object System.Text.StringBuilder(256)
            [NativeMenuProbe]::GetMenuString($submenu, $itemIndex, $label, 256, 1024) | Out-Null
            if ($label.ToString() -eq $ItemLabel) {
                $commandId = [NativeMenuProbe]::GetMenuItemID($submenu, $itemIndex)
                break
            }
        }
        if ($commandId -ne 0) { break }
    }
    if ($commandId -eq 0) { throw 'The native Increment item was not found' }

    Start-Sleep -Seconds 1
    [NativeMenuProbe]::SendMessage($hwnd, 0x0111, [IntPtr]$commandId, [IntPtr]::Zero) | Out-Null
    $handled = $false
    for ($attempt = 0; $attempt -lt 100 -and -not $handled; $attempt++) {
        if ($attempt -gt 0 -and $attempt % 5 -eq 0) {
            [NativeMenuProbe]::SendMessage($hwnd, 0x0111, [IntPtr]$commandId, [IntPtr]::Zero) | Out-Null
        }
        $candidates = $automationRoot.FindAll([System.Windows.Automation.TreeScope]::Children, $condition)
        foreach ($candidate in $candidates) {
            if ($candidate.Current.Name -like $WindowTitle) { $window = $candidate; break }
        }
        if ($null -ne $window) {
            $texts = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
            foreach ($text in $texts) {
                if ($text.Current.ControlType.ProgrammaticName -eq 'ControlType.Text') {
                    $allExpected = $true
                    foreach ($expected in $expectedFragments) {
                        if (-not $text.Current.Name.Contains($expected)) { $allExpected = $false; break }
                    }
                    if ($allExpected) { $handled = $true; break }
                }
            }
        }
        if (-not $handled) { Start-Sleep -Milliseconds 100 }
    }
    if (-not $handled) { throw "The native WM_COMMAND did not reach the page menu handler with: $($ExpectedText -join ', ')" }
    Write-Output "Native menu item '$ItemLabel' (command $commandId) reached $Application through Win32 and the runtime bridge."
} finally {
    if (-not $process.HasExited) {
        $process.CloseMainWindow() | Out-Null
        if (-not $process.WaitForExit(5000)) { $process.Kill() }
    }
}
