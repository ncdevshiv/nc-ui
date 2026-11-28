# Fetches the wasm-bindgen CLI matching the pinned crate version (Cargo.toml).
# The binary is tooling, not source: it is gitignored and fetched on demand.
$ErrorActionPreference = 'Stop'
$version = '0.2.129'
$zip = "$env:TEMP/wasm-bindgen-$version.tar.gz"
Invoke-WebRequest -Uri "https://github.com/rustwasm/wasm-bindgen/releases/download/$version/wasm-bindgen-$version-x86_64-pc-windows-msvc.tar.gz" -OutFile $zip
$extract = "$env:TEMP/wasm-bindgen-cli-$version"
if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
New-Item -ItemType Directory $extract | Out-Null
tar -xzf $zip -C $extract
New-Item -ItemType Directory -Force "$PSScriptRoot/bin" | Out-Null
Copy-Item "$extract/wasm-bindgen-$version-x86_64-pc-windows-msvc/wasm-bindgen.exe" "$PSScriptRoot/bin/"
Write-Output "wasm-bindgen $version installed to gate/bin"
