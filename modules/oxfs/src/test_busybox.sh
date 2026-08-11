#!/bin/sh
# BusyBox applet self-test -- runs INSIDE OxideBSD, not on the host.
#
# Usage, at the hush prompt:
#   sh /test_busybox.sh
#
# A real POSIX-shell test harness now, not a flat sequence of hand-unrolled PASS/FAIL lines.
# `sh`'s own .config (target/busybox-sh/.config after a real build.rs run) used to have only
# CONFIG_HUSH/HUSH_INTERACTIVE/HUSH_JOB set -- HUSH_IF/HUSH_LOOPS/HUSH_CASE/HUSH_FUNCTIONS/
# HUSH_TICK/HUSH_TEST/... were all off (an `allnoconfig` artifact: it writes an explicit
# "not set" line for every symbol in the whole Kconfig tree up front, so the later `make
# oldconfig` pass -- which only fills in symbols with *no* prior answer -- never got a chance to
# apply hush's real `default y` for any of them). Fixed in `build.rs`'s
# `configure_busybox_single_applet` (see its own doc comment) by flipping those lines directly,
# the same way it already did for `HUSH_INTERACTIVE`/`HUSH_JOB`. This script both exercises real
# applets *and* doubles as the regression check for that fix: the "control flow smoke test"
# section below asserts `if`/`for`/`while`/`until`/`case`/functions/`local`/arithmetic/command
# substitution/brace expansion/`$RANDOM` all actually work, not just that they compiled in.
#
# `check`/`check_status`/`report` (below) accumulate into real `PASS`/`FAIL` shell variables via
# real `$((...))` arithmetic -- no longer marker files (`ok_<name>`/`bad_<name>`) tallied with
# `ls | wc -l` at the end, since that workaround existed only because hush had no working
# arithmetic or persistent variables across... itself (it always ran in one process; the marker
# files were purely a stand-in for a counter that didn't work yet). Most checks now capture a
# real applet's output directly via `$(...)` and compare it to an inline expected string, instead
# of writing both sides out to scratch files and running `cmp` -- real command substitution
# strips all trailing newlines on both the actual and hand-written-expected side uniformly, so a
# plain string comparison is exact without needing a byte-for-byte file compare.
#
# Two real, separate kernel bugs this script (or an earlier version of it) found live, both now
# fixed, still worth remembering here since nothing in `cargo test`/`clippy` would have caught
# either:
#
# 1. `oxidebsd_sys_exit` used to store a userland `exit(code)` value as the `wait4()` status
#    *unshifted*, instead of real `wait(2)`'s actual encoding (`WEXITSTATUS` in bits 8-15, low 7
#    bits zero for a normal exit). Any applet exiting normally with a nonzero code -- extremely
#    common -- had that raw low byte misread by hush's real `WIFSIGNALED`/`WTERMSIG` macros as
#    "terminated by signal N" (`exit(1)` decoded as `SIGHUP`), printing a spurious "Hangup" after
#    nearly any failing command and corrupting later commands in the same session. Fixed at the
#    source (see CLAUDE.md's Process/scheduler section) -- this script no longer needs to work
#    around it at all.
# 2. `oxfs_open` used to always return a read-only fd for a path that already existed, regardless
#    of `O_WRONLY`/`O_RDWR`/`O_APPEND`/`O_TRUNC` -- a file could only ever be written once, for
#    its entire lifetime. An earlier version of *this exact script* found it live (accumulating
#    PASS/FAIL lines into one shared results file via repeated `tee -a` started failing on the
#    second line). Fixed in `oxfs_open`/`oxfs_close` (see CLAUDE.md's Permission model section) --
#    the "overwrite an existing file" / "append to an existing file" checks below are the real
#    regression coverage for that fix, something the old file-per-check design could never have
#    exercised even by accident.
#
# `touch` on a path that already exists now succeeds for real too (`utimensat`'s own real
# existence-check semantics -- see CLAUDE.md's musl-port section: no per-inode timestamp fields
# exist to actually update, but a real `ENOENT`-vs-success distinction is enough to satisfy
# `touch.c`'s own fallback logic either way).

PASS=0
FAIL=0

# report NAME STATUS -- STATUS is a real 0/nonzero shell exit-status value (typically `$?` from
# the line just above, or 0/1 computed inline). Tallies into PASS/FAIL and prints straight away.
report() {
    name=$1
    status=$2
    if [ "$status" -eq 0 ]; then
        echo "PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $name"
        FAIL=$((FAIL + 1))
    fi
}

# check NAME ACTUAL EXPECTED -- compares two strings directly (captured via $(...) at the call
# site), no scratch files or `cmp` needed.
check() {
    name=$1
    actual=$2
    expected=$3
    if [ "$actual" = "$expected" ]; then
        report "$name" 0
    else
        report "$name" 1
        echo "  actual:   $actual"
        echo "  expected: $expected"
    fi
}

# check_status NAME STATUS -- thin alias of report(), used at call sites that are asserting a
# real command's own exit status rather than comparing two strings.
check_status() {
    report "$1" "$2"
}

rm -rf /test_busybox_scratch
mkdir /test_busybox_scratch
cd /test_busybox_scratch

echo "=== OxideBSD BusyBox applet self-test ==="

echo "--- control flow smoke test (what build.rs's HUSH config fix actually turned on) ---"

# if / elif / else
if [ 1 -eq 2 ]; then
    ctrl_if="wrong branch"
elif [ 2 -eq 2 ]; then
    ctrl_if="elif branch"
else
    ctrl_if="wrong branch"
fi
check "if/elif/else" "$ctrl_if" "elif branch"

# for loop
ctrl_for=""
for word in one two three; do
    ctrl_for="$ctrl_for$word,"
done
check "for loop" "$ctrl_for" "one,two,three,"

# while loop
ctrl_while=0
i=0
while [ "$i" -lt 5 ]; do
    ctrl_while=$((ctrl_while + i))
    i=$((i + 1))
done
check "while loop" "$ctrl_while" "10"

# until loop
ctrl_until=0
until [ "$ctrl_until" -ge 3 ]; do
    ctrl_until=$((ctrl_until + 1))
done
check "until loop" "$ctrl_until" "3"

# case / esac, driven by a for loop over several inputs
ctrl_case=""
for word in apple banana cherry; do
    case "$word" in
        apple) ctrl_case="${ctrl_case}A" ;;
        banana) ctrl_case="${ctrl_case}B" ;;
        *) ctrl_case="${ctrl_case}?" ;;
    esac
done
check "case/esac" "$ctrl_case" "AB?"

# functions, local variables, arithmetic, and a real return value read back via $(...)
add_one() {
    local n=$1
    echo $((n + 1))
}
check "function + local + arithmetic" "$(add_one 41)" "42"

# command substitution -- both real forms
check 'command substitution $(...)' "$(echo nested)" "nested"
check 'command substitution `...`' "`echo nested`" "nested"

# arithmetic expansion on its own
check 'arithmetic $((...))' "$((6 * 7))" "42"

# brace expansion (a bash-compat extension, CONFIG_HUSH_BRACE_EXPANSION)
check "brace expansion" "$(echo {a,b,c})" "a b c"

# $RANDOM -- value is nondeterministic, so only assert it's a real nonempty decimal number
r1=$RANDOM
case "$r1" in
    ''|*[!0-9]*) check_status '$RANDOM produces a decimal number' 1 ;;
    *) check_status '$RANDOM produces a decimal number' 0 ;;
esac

echo "--- individual applets ---"

# --- echo / printf ---
check "echo" "$(echo hello world)" "hello world"
check "printf formatting" "$(printf '%s-%d' foo 7)" "foo-7"

# --- touch / test -f, including touch on an already-existing path ---
touch a.txt
check_status "touch creates a file" $?
[ -f a.txt ]
check_status "the touched file really exists" $?
touch a.txt
check_status "touch on an already-existing path (utimensat)" $?

# --- overwrite / append an existing file (the real regression check for the oxfs write-existing-
#     file fix noted above -- impossible to have exercised under the old one-write-per-file design) ---
echo "first" > overwrite.txt
echo "second" > overwrite.txt
check "overwrite an existing file" "$(cat overwrite.txt)" "second"

echo "first" > append.txt
echo "second" >> append.txt
check "append to an existing file" "$(cat append.txt)" "$(printf 'first\nsecond')"

# --- write/read via redirection + cat ---
echo "line one" > multi.txt
echo "line two" >> multi.txt
check "cat reads back written content" "$(cat multi.txt)" "$(printf 'line one\nline two')"

# --- wc (word-split to strip any column-padding whitespace real wc adds) ---
set -- $(wc -l < multi.txt)
check "wc -l counts lines" "$1" "2"

# --- head / tail ---
check "head -n 1" "$(head -n 1 multi.txt)" "line one"
check "tail -n 1" "$(tail -n 1 multi.txt)" "line two"

# --- cp / mv / rm ---
cp multi.txt copy.txt
[ -f copy.txt ]
check_status "cp created the destination" $?

mv copy.txt moved.txt
[ -f moved.txt ]
check_status "mv created the new name" $?
[ ! -f copy.txt ]
check_status "mv removed the old name" $?

rm moved.txt
[ ! -f moved.txt ]
check_status "rm removed the file" $?

# --- mkdir -p / rmdir (nested) ---
mkdir -p nest/deep
[ -d nest/deep ]
check_status "mkdir -p created nested dirs" $?

rmdir nest/deep
rmdir nest
[ ! -d nest ]
check_status "rmdir removed nested dirs" $?

# --- cut / sort / uniq ---
printf 'b:2\na:1\na:1\nc:3\n' > kv.txt

check "cut -d: -f1" "$(cut -d: -f1 kv.txt)" "$(printf 'b\na\na\nc')"
check "sort" "$(sort kv.txt)" "$(printf 'a:1\na:1\nb:2\nc:3')"
check "sort -u" "$(sort -u kv.txt)" "$(printf 'a:1\nb:2\nc:3')"
check "sort | uniq" "$(sort kv.txt | uniq)" "$(sort -u kv.txt)"

# --- grep ---
check "grep matches" "$(grep 'a:1' kv.txt)" "$(printf 'a:1\na:1')"
if grep -q zzz kv.txt; then
    check_status "grep -q reports no match" 1
else
    check_status "grep -q reports no match" 0
fi

# --- sed ---
echo abc > sed_in.txt
check "sed substitution" "$(sed 's/b/X/' sed_in.txt)" "aXc"

# --- tr ---
check "tr uppercases" "$(tr a-z A-Z < sed_in.txt)" "ABC"

# --- basename / dirname ---
check "basename" "$(basename /a/b/c.txt)" "c.txt"
check "dirname" "$(dirname /a/b/c.txt)" "/a/b"

# --- seq ---
check "seq" "$(seq 1 3)" "$(printf '1\n2\n3')"

# --- xargs ---
check "xargs -n 1 (default echo)" "$(printf 'one\ntwo\n' | xargs -n 1)" "$(printf 'one\ntwo')"

# --- chmod ---
chmod 600 a.txt
check_status "chmod succeeds" $?

# --- whoami (real uid/passwd-db path, see CLAUDE.md's Permission model section) ---
check "whoami reports root" "$(whoami)" "root"

# --- kill -0 on our own pid (real POSIX existence-check convention) ---
kill -0 $$
check_status "kill -0 finds a live process" $?

# --- informational only (output shape not asserted -- shown so you can eyeball it) ---
echo "--- find . -name a.txt (informational) ---"
find . -name a.txt

echo "--- stat a.txt (informational) ---"
stat a.txt

echo "--- du (informational) ---"
du multi.txt

echo "--- diff on a changed line (informational) ---"
printf 'a\nb\n' > d1.txt
printf 'a\nc\n' > d2.txt
diff d1.txt d2.txt

echo "=== summary ==="
echo "pass: $PASS  fail: $FAIL  total: $((PASS + FAIL))"
if [ "$FAIL" -eq 0 ]; then
    echo "ALL PASS"
    exit 0
else
    echo "$FAIL CHECK(S) FAILED"
    exit 1
fi
