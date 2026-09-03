#!/bin/sh
# Host-side supervisor for the POSIX conformance pilot (tests/posix_conformance_smoke.rs).
#
# Why this exists: the pilot's own userspace timeout (`t0`, a real `alarm(40)` per test file, see
# modules/oxfs/src/posix_conformance.sh) can only rescue a test that's merely *slow* -- it can't
# rescue a genuine kernel-level wedge, where the stuck process is blocked somewhere no wake hook
# ever re-checks it (an unhandled BlockReason, a stale scheduler entry, a deadlocked futex chain --
# see CLAUDE.md's own history of exactly this bug class). When that happens, `t0`'s own alarm
# either never fires or fires and is never acted on, and the whole rest of the sequential pilot
# script -- and every file after the stuck one -- hangs right along with it. Nothing *inside* the
# guest can rescue that; only something watching from the host, able to kill the QEMU process
# itself, can. This script is that: run the pilot, watch its own progress (a real PASS/FAIL/etc.
# classification line per file), and if nothing new appears for a while, kill QEMU, figure out
# which file was stuck, exclude it, and retry -- automating the "found live, fixed forward"
# discipline this whole file's own `POSIX_KNOWN_HANGS`/`POSIX_EXTRA_EXCLUDE_FILE` (see build.rs)
# already document doing by hand across many separate sessions.
#
# Usage:
#   scripts/run_posix_pilot_supervised.sh [--reset] [--stall-seconds N] [--max-iterations N]
#
#   --reset            start with a clean exclude list (default: resume/accumulate across runs)
#   --stall-seconds N  seconds with no new PASS/FAIL/etc. line before treating the run as wedged
#                       (default 120 -- well past t0's own 40s per-file rescue bound, so this only
#                       fires on a real kernel-level hang, not a legitimately slow test)
#   --max-iterations N give up after this many retries (default 30)
#
# On a clean run, prints the final summary and exits 0. Every file this script excludes along the
# way is a REAL FINDING worth investigating and root-causing -- see
# target/posix_extra_excludes.txt afterward, and graduate each one into a real, documented
# POSIX_KNOWN_HANGS entry once understood, the same way every other exclusion in build.rs already
# is. This script's own exclude list is a fast-iteration aid, not a permanent record.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

STALL_SECONDS=120
MAX_ITERATIONS=30
RESET=0

while [ $# -gt 0 ]; do
    case "$1" in
        --reset) RESET=1; shift ;;
        --stall-seconds) STALL_SECONDS="$2"; shift 2 ;;
        --max-iterations) MAX_ITERATIONS="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

EXCLUDE_FILE="$REPO_ROOT/target/posix_extra_excludes.txt"
LOG_DIR="$REPO_ROOT/target/posix-pilot-logs"
MANIFEST="$REPO_ROOT/target/generated/posix_test_manifest.txt"

mkdir -p "$LOG_DIR"
if [ "$RESET" -eq 1 ] || [ ! -f "$EXCLUDE_FILE" ]; then
    : > "$EXCLUDE_FILE"
fi

# Finds the manifest entry immediately after the last file this log run actually classified --
# that's the one it was in the middle of (or about to start) when progress stopped. If nothing was
# classified yet, the first manifest entry is the culprit (a stall during module/boot init itself,
# before the pilot script even started, would also land here -- worth noticing if it ever happens).
find_stuck_file() {
    log="$1"
    last_classified=$(grep -oE '^(PASS|FAIL|UNRESOLVED|UNSUPPORTED|UNTESTED|TIMEOUT|CRASH): .*$' "$log" 2>/dev/null | tail -1 | sed -E 's/^[A-Z]+: //')
    if [ -z "$last_classified" ]; then
        head -1 "$MANIFEST"
        return
    fi
    awk -v target="$last_classified" '
        found { print; exit }
        $0 == target { found=1 }
    ' "$MANIFEST"
}

iteration=0
while [ "$iteration" -lt "$MAX_ITERATIONS" ]; do
    iteration=$((iteration + 1))
    ts=$(date +%Y%m%dT%H%M%S)
    log="$LOG_DIR/supervised_iter${iteration}_${ts}.log"
    excluded_count=$(grep -c . "$EXCLUDE_FILE" 2>/dev/null || true)
    excluded_count=${excluded_count:-0}
    echo "=== iteration $iteration (excluding $excluded_count file(s) so far): starting, log at $log ==="

    # Defensive: a leftover QEMU instance from a prior run (this script killed on a stall, a
    # manually-run test, an interrupted previous invocation) can still hold a write lock on
    # target/oxfs_test_disk.img even after this script's own `wait` returns, making the *next*
    # iteration's own QEMU fail outright with "Failed to get \"write\" lock" -- a real, found-live
    # failure mode, not a stall (so the stall-detection loop below never catches it). Clear the
    # decks unconditionally before every iteration, not just after a detected stall.
    pkill -f "bootimage-posix_conformance_smoke" 2>/dev/null || true
    sleep 1

    POSIX_EXTRA_EXCLUDE_FILE="$EXCLUDE_FILE" cargo test --test posix_conformance_smoke \
        > "$log" 2>&1 &
    test_pid=$!

    last_progress=$(date +%s)
    last_line_count=0
    stalled=0
    while kill -0 "$test_pid" 2>/dev/null; do
        sleep 5
        line_count=$(grep -cE '^(PASS|FAIL|UNRESOLVED|UNSUPPORTED|UNTESTED|TIMEOUT|CRASH): ' "$log" 2>/dev/null || true)
        line_count=${line_count:-0}
        now=$(date +%s)
        if [ "$line_count" -gt "$last_line_count" ]; then
            last_line_count=$line_count
            last_progress=$now
        elif [ $((now - last_progress)) -ge "$STALL_SECONDS" ]; then
            stalled=1
            break
        fi
    done

    if [ "$stalled" -eq 1 ]; then
        echo "=== iteration $iteration: no new result for ${STALL_SECONDS}s -- treating as a real kernel-level wedge, killing QEMU ==="
        pkill -f "bootimage-posix_conformance_smoke" 2>/dev/null || true
        wait "$test_pid" 2>/dev/null || true

        stuck=$(find_stuck_file "$log")
        if [ -z "$stuck" ]; then
            echo "=== iteration $iteration: couldn't identify the stuck file (manifest exhausted?) -- inspect $log manually ===" >&2
            exit 1
        fi
        echo "$stuck" >> "$EXCLUDE_FILE"
        echo "=== iteration $iteration: stuck on '$stuck' -- added to $EXCLUDE_FILE, retrying ==="
        continue
    fi

    wait "$test_pid"
    status=$?
    if grep -q "posix_conformance_smoke: driver reported PASS" "$log"; then
        echo "=== clean run achieved after $iteration iteration(s), $excluded_count file(s) excluded along the way ==="
        grep -A 20 "^=== summary ===" "$log" || true
        [ -s "$EXCLUDE_FILE" ] && { echo "--- excluded files (real findings, still need root-causing) ---"; cat "$EXCLUDE_FILE"; }
        exit 0
    fi
    echo "=== iteration $iteration: run finished (exit=$status) but not a clean PASS -- not a stall, something else is wrong. Inspect $log manually. ===" >&2
    exit 1
done

echo "=== hit --max-iterations ($MAX_ITERATIONS) without a clean run -- inspect $LOG_DIR and $EXCLUDE_FILE ===" >&2
exit 1
