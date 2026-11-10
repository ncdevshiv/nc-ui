#!/usr/bin/env bash
# Runs every libFuzzer target for a bounded duration and reports what each covered.
#
# A script rather than a command line, because `wsl.exe ... bash -lc "for t in ..."` does not
# expand `$t` — the variable is consumed by the Windows shell before WSL sees it.
#
# Usage: run_fuzzers.sh [seconds-per-target]

set -uo pipefail

SECONDS_PER_TARGET="${1:-60}"
REPO=/mnt/f/nc-ui

source "$HOME/.cargo/env"
cd "$REPO" || exit 1

failures=0
for target in fuzz_frame_decode fuzz_message_decode fuzz_semver_parse \
              fuzz_path_resolve fuzz_token_hex fuzz_registry_parse; do
    echo "=== $target ==="
    log=$(mktemp)
    if cargo +nightly fuzz run "$target" -- \
        -max_total_time="$SECONDS_PER_TARGET" \
        -rss_limit_mb=2560 \
        -print_final_stats=1 >"$log" 2>&1; then
        # `Done` plus the executed-unit count is the evidence the engine really ran, rather
        # than starting and immediately exiting.
        units=$(grep -oE 'stat::number_of_executed_units: *[0-9]+' "$log" | tail -1 | grep -oE '[0-9]+')
        cov=$(grep -oE 'cov: *[0-9]+' "$log" | tail -1 | grep -oE '[0-9]+')
        echo "    OK  executed_units=${units:-?} coverage=${cov:-?}"
        grep -q "Done " "$log" && echo "    engine reported Done" || echo "    WARNING: no Done marker"
    else
        failures=$((failures + 1))
        echo "    FAILED"
        tail -25 "$log"
    fi
    rm -f "$log"
done

echo
echo "fuzz run: $SECONDS_PER_TARGET s per target, $failures failure(s)"
exit $failures
