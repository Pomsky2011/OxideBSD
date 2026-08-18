#!/bin/sh
# A real POSIX conformance baseline -- runs INSIDE OxideBSD, not on the host.
#
# Usage, at the hush prompt:
#   sh /posix_conformance.sh
#
# See docs/POSIX_COMPLIANCE_CHECKLIST.md's own "Verification" section for why this exists: every
# other doc in this tree (docs/MISSING_POSIX_SYSCALLS.md, that checklist itself) is
# self-assessment against a hand-written list. This is a real, independent conformance suite (the
# Open POSIX Test Suite, vendored at third_party/posixtestsuite) actually run against this kernel.
#
# What this script does, for each `relative/path.c` line in `/posix-tests/manifest.txt` (a curated
# pilot subset -- see build.rs's `write_posix_test_manifest` for exactly which files and why; NOT
# the whole suite, which is ~1750 files in `conformance/interfaces` alone):
#   1. Run `/posix-tests/bin/<relative/path.c>` (a real ELF, pre-compiled on the host with
#      `musl-gcc` -- see `write_posix_test_manifest`'s own doc comment for why on-target `tcc`
#      compilation was tried first and abandoned: a real `tcc` GOT/PLT linker bug produced binaries
#      that null-pointer-faulted calling into specific musl functions -- `sigaction`/`fflush`
#      confirmed -- found live investigating exactly this pilot's own early crashes) through
#      `/posix-tests/t0` (the suite's own real timeout-wrapper utility, also pre-compiled) with a
#      real bounded timeout -- load-bearing, not a nicety: this kernel has no preemption, so a test
#      that blocks forever on something genuinely unimplemented (a `sem_wait` that can never wake
#      without real `futex(2)`, say) would otherwise hang this whole script -- and the shell
#      driving it -- permanently. `t0` sets a real `alarm(n)` then `execvp`s the test directly (see
#      its own source comment) -- an expired timeout surfaces as the test dying to an uncaught real
#      `SIGALRM`, which this kernel's own real `wait(2)`-encoded exit status reports as `128 +
#      SIGALRM` (14) = 142 (see CLAUDE.md's process/scheduler section on the real
#      signal-vs-normal-exit status encoding) -- classified below as TIMEOUT, not a crash.
#   2. Classify the real exit status against the suite's own standardized result codes
#      (real POSIX values every pilot binary was built against): PASS(0)/FAIL(1)/UNRESOLVED(2)/
#      UNSUPPORTED(4)/UNTESTED(5), plus this script's own TIMEOUT/CRASH buckets for everything the
#      suite's own convention doesn't cover (a real signal-terminated crash, e.g. a genuine
#      SIGSEGV, is exactly as informative a result as a clean FAIL -- it means something, just not
#      "the assertion itself was checked and failed").

PASS=0
FAIL=0
UNRESOLVED=0
UNSUPPORTED=0
UNTESTED=0
TIMEOUT=0
CRASH=0

echo "=== posix conformance pilot: running ==="
for rel in $(cat /posix-tests/manifest.txt); do
    bin="/posix-tests/bin/$rel"

    # 40s, not 5s: nanosleep/10000-1.c alone legitimately sleeps through ~27 real seconds of valid
    # durations (0+1+1+2+10+13s) before it even reaches its invalid-parameter checks -- a real,
    # working nanosleep() genuinely needs that long, not a hang. Found live: an earlier 5s bound
    # timed it out and misclassified a passing test as TIMEOUT.
    /posix-tests/t0 40 "$bin" >/posix-tests/run-out.txt 2>&1
    status=$?

    case "$status" in
        0)
            echo "PASS: $rel"
            PASS=$((PASS + 1))
            ;;
        1)
            echo "FAIL: $rel"
            FAIL=$((FAIL + 1))
            ;;
        2)
            echo "UNRESOLVED: $rel"
            UNRESOLVED=$((UNRESOLVED + 1))
            ;;
        4)
            echo "UNSUPPORTED: $rel"
            UNSUPPORTED=$((UNSUPPORTED + 1))
            ;;
        5)
            echo "UNTESTED: $rel"
            UNTESTED=$((UNTESTED + 1))
            ;;
        142)
            echo "TIMEOUT: $rel"
            TIMEOUT=$((TIMEOUT + 1))
            ;;
        *)
            echo "CRASH($status): $rel"
            CRASH=$((CRASH + 1))
            ;;
    esac
done

echo "=== summary ==="
echo "pass: $PASS"
echo "fail: $FAIL"
echo "unresolved: $UNRESOLVED"
echo "unsupported: $UNSUPPORTED"
echo "untested: $UNTESTED"
echo "timeout: $TIMEOUT"
echo "crash: $CRASH"
echo "total: $((PASS + FAIL + UNRESOLVED + UNSUPPORTED + UNTESTED + TIMEOUT + CRASH))"
