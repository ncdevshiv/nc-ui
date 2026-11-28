# Real-browser gate for the web shell (ADR-021). Builds the wasm artifact,
# regenerates the wasm-bindgen JS bindings, and drives system Edge headless.
# No target is supported because it compiles: this gate is the support.
param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) -Parent
$here = $PSScriptRoot
if (-not $NoBuild) {
    cargo build -p ncui-web-shell --target wasm32-unknown-unknown --manifest-path "$root/Cargo.toml"
    if ($LASTEXITCODE -ne 0) { throw 'wasm build failed' }
}
& "$here/bin/wasm-bindgen.exe" --target web --out-dir "$here/web" `
    "$root/target/wasm32-unknown-unknown/debug/ncui_web_shell.wasm"
if ($LASTEXITCODE -ne 0) { throw 'wasm-bindgen failed' }
# The font the shell renders text with. This is the development machine's own copy,
# used in place because the gate is local evidence on that machine; what a distributed
# artifact ships is a licensing decision of its own, named in the register.
Copy-Item "C:\Windows\Fonts\arial.ttf" "$here/web/ncui-font.ttf" -Force
if (-not (Test-Path "$here/node_modules")) { npm install --prefix $here --no-audit --no-fund }
node "$here/verify.mjs"
exit $LASTEXITCODE
