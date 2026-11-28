# Packaging build step (ADR-021): produces the static artifact — wasm, loader,
# html — servable by any static file server. The browser shell has no runtime
# process, so packaging is a build step, not a host.
param([string]$OutDir = "$PSScriptRoot/dist")
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) -Parent

cargo build -p ncui-web-shell --target wasm32-unknown-unknown --release --manifest-path "$root/Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw 'release wasm build failed' }

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force $OutDir | Out-Null

& "$PSScriptRoot/bin/wasm-bindgen.exe" --target web --out-dir $OutDir `
    "$root/target/wasm32-unknown-unknown/release/ncui_web_shell.wasm"
if ($LASTEXITCODE -ne 0) { throw 'wasm-bindgen failed' }

Copy-Item "$PSScriptRoot/web/index.html" "$OutDir/index.html"
# The font asset — same development-machine copy the gate uses; see run-gate.ps1.
Copy-Item "C:\Windows\Fonts\arial.ttf" "$OutDir/ncui-font.ttf" -Force
$files = Get-ChildItem $OutDir | Select-Object -ExpandProperty Name
Write-Output ("packaged: " + ($files -join ', '))
if (-not ($files -contains 'index.html' -and $files -contains 'ncui_web_shell.js' -and ($files | Where-Object { $_ -like '*.wasm' }))) {
    throw 'package incomplete'
}
Write-Output 'PACKAGE=OK'
