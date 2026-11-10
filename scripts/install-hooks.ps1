param([Parameter(ValueFromRemainingArguments=$true)][string[]]$Args)
$root = (git rev-parse --show-toplevel)
Push-Location $root
try {
  git config core.hooksPath .githooks
  Write-Output "core.hooksPath=.githooks"
} finally {
  Pop-Location
}
exit $LASTEXITCODE
