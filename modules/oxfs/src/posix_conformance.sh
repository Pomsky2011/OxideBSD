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
#   1. Compile it on-target with `tcc` (the real on-target C compiler -- see CLAUDE.md's TinyCC
#      section), the same real toolchain any other on-target C program uses, against
#      `/posix-tests/include/posixtest.h` (every pilot file's own `#include "posixtest.h"` target).
#   2. Run the result through `/posix-tests/t0` (the suite's own real timeout-wrapper utility,
#      compiled once at the top of this script from `/posix-tests/t0.c`) with a real bounded
#      timeout -- load-bearing, not a nicety: this kernel has no preemption, so a test that blocks
#      forever on something genuinely unimplemented (a `sem_wait` that can never wake without real
#      `futex(2)`, say) would otherwise hang this whole script -- and the shell driving it --
#      permanently. `t0` sets a real `alarm(n)` then `execvp`s the test directly (see its own
#      source comment) -- an expired timeout surfaces as the test dying to an uncaught real
#      `SIGALRM`, which this kernel's own real `wait(2)`-encoded exit status reports as `128 +
#      SIGALRM` (14) = 142 (see CLAUDE.md's process/scheduler section on the real
#      signal-vs-normal-exit status encoding) -- classified below as TIMEOUT, not a crash.
#   3. Classify the real exit status against the suite's own standardized result codes
#      (`/posix-tests/include/posixtest.h`): PASS(0)/FAIL(1)/UNRESOLVED(2)/UNSUPPORTED(4)/
#      UNTESTED(5), plus this script's own TIMEOUT/CRASH/COMPILE_FAIL buckets for everything the
#      suite's own convention doesn't cover (a real signal-terminated crash, e.g. a genuine
#      SIGSEGV, is exactly as informative a result as a clean FAIL -- it means something, just not
#      "the assertion itself was checked and failed").
#
# A COMPILE_FAIL is a real, separate signal from a runtime result -- could mean `tcc`'s own more
# limited C support (versus the real GCC/musl-gcc every pilot file was pre-verified to compile
# clean against, see `write_posix_test_manifest`'s own doc comment) rather than a kernel gap. Both
# are worth knowing, but don't conflate them when reading results.

PASS=0
FAIL=0
UNRESOLVED=0
UNSUPPORTED=0
UNTESTED=0
TIMEOUT=0
CRASH=0
COMPILE_FAIL=0

echo "=== posix conformance pilot: compiling t0 (timeout wrapper) ==="
if ! tcc -o /posix-tests/t0 /posix-tests/t0.c 2>/posix-tests/t0-build-err.txt; then
    echo "FATAL: could not compile t0.c itself -- aborting"
    cat /posix-tests/t0-build-err.txt
    exit 2
fi

echo "=== posix conformance pilot: running ==="
for rel in $(cat /posix-tests/manifest.txt); do
    src="/posix-tests/src/$rel"
    bin="/posix-tests/bin-tmp"

    if ! tcc -o "$bin" -I /posix-tests/include "$src" 2>/posix-tests/compile-err.txt; then
        echo "COMPILE_FAIL: $rel"
        COMPILE_FAIL=$((COMPILE_FAIL + 1))
        continue
    fi

    /posix-tests/t0 5 "$bin" >/posix-tests/run-out.txt 2>&1
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
echo "compile_fail: $COMPILE_FAIL"
echo "total: $((PASS + FAIL + UNRESOLVED + UNSUPPORTED + UNTESTED + TIMEOUT + CRASH + COMPILE_FAIL))"
