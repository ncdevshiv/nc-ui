param([Parameter(ValueFromRemainingArguments=$true)][string[]]$Args)
$root = (git rev-parse --show-toplevel)
Push-Location $root
try { & python scripts/file-size.py @Args } finally { Pop-Location }
exit $LASTEXITCODE
