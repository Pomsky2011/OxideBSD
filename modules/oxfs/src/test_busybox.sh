#!/bin/sh
# BusyBox applet self-test -- runs INSIDE OxideBSD, not on the host.
#
# Usage, at the hush prompt:
#   sh /test_busybox.sh
#
# Written for this kernel's actual hush build, not a generic POSIX shell: `sh`'s own .config
# (target/busybox-sh/.config after a real build.rs run) has only CONFIG_HUSH/HUSH_INTERACTIVE/
# HUSH_JOB set -- HUSH_IF/HUSH_LOOPS/HUSH_CASE/HUSH_FUNCTIONS/HUSH_TICK are all off. So: no
# if/for/while/case, no shell functions, no $(...)/`...` command substitution, no shell-builtin
# test/echo/printf/kill (real standalone applet binaries are used for all of those instead, via
# PATH). What's used here: plain command execution, pipes, redirection, `&&`/`||`/`;`, `$?`/`$$`,
# and variable assignment -- all unconditional core hush grammar.
#
# Every check redirects a real applet's output to a fresh file and compares it (via `cmp` or the
# command's own exit status) against a hand-written expected result, printing PASS/FAIL straight
# to the console and recording a `ok_<name>`/`bad_<name>` marker file (via `echo 1 > ...`, not
# `touch` -- see below) for the final tally. **Every file this script writes to is a brand-new
# name, written exactly once**: oxfs's own `open()` has a documented simplification (see
# `oxfs_open`'s doc comment in modules/oxfs/src/lib.rs) where re-opening an *existing* file for
# writing silently downgrades to a read-only fd instead of erroring or truncating -- an earlier
# version of this script accumulated PASS/FAIL lines into one shared results.txt via repeated
# `tee -a`, which hit exactly that gap on every line after the first.
#
# That earlier version also hit a real, separate, now-fixed kernel bug: `oxidebsd_sys_exit`
# (src/syscall.rs) used to store a userland `exit(code)` value as the wait4() status *unshifted*,
# instead of real `wait(2)`'s actual encoding (`WEXITSTATUS` in bits 8-15, low 7 bits zero for a
# normal exit). Any applet exiting normally with a nonzero code -- extremely common, e.g. any
# command reporting its own ordinary error -- had that raw low byte misread by hush's real
# `WIFSIGNALED`/`WTERMSIG` macros as "terminated by signal N" (`exit(1)` decoded as `SIGHUP`,
# hence a spurious "Hangup" after nearly any failing command, and real signal-death handling then
# corrupting later commands in the same session). Fixed at the source; this script no longer needs
# to work around it, just noted here since it's why the marker-file approach below still avoids
# `touch` specifically for an unrelated reason (next paragraph), not because of the old cascade.
#
# Markers use `echo 1 > file`, not `touch file`: this kernel doesn't implement `utimensat`
# (BusyBox's `touch` tries it first and, since it gets `ENOSYS` rather than the `ENOENT` it knows
# how to recover from by falling back to creating the file, `touch` always fails here -- a real,
# separate, unfixed gap this script's own "touch creates a file" check is expected to legitimately
# FAIL until `utimensat` exists). Everything else in this script (`echo`/`printf`/redirection) has
# no such dependency.

rm -rf /test_busybox_scratch
mkdir /test_busybox_scratch
cd /test_busybox_scratch

echo "=== OxideBSD BusyBox applet self-test ==="

# --- echo / printf ---
echo "hello world" > act_echo.txt
printf 'hello world\n' > exp_echo.txt
cmp -s act_echo.txt exp_echo.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: echo" || echo "FAIL: echo"
test "$STATUS" -eq 0 && echo 1 > ok_echo || echo 1 > bad_echo

printf '%s-%d\n' foo 7 > act_printf.txt
printf 'foo-7\n' > exp_printf.txt
cmp -s act_printf.txt exp_printf.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: printf formatting" || echo "FAIL: printf formatting"
test "$STATUS" -eq 0 && echo 1 > ok_printf || echo 1 > bad_printf

# --- touch / test -f ---
touch a.txt
test -f a.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: touch creates a file" || echo "FAIL: touch creates a file"
test "$STATUS" -eq 0 && echo 1 > ok_touch || echo 1 > bad_touch

# --- write/read via redirection + cat ---
echo "line one" > multi.txt
echo "line two" >> multi.txt
cat multi.txt > act_cat.txt
printf 'line one\nline two\n' > exp_cat.txt
cmp -s act_cat.txt exp_cat.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: cat reads back written content" || echo "FAIL: cat reads back written content"
test "$STATUS" -eq 0 && echo 1 > ok_cat || echo 1 > bad_cat

# --- wc ---
wc -l < multi.txt > act_wc.txt
grep -q '^ *2 *$' act_wc.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: wc -l counts lines" || echo "FAIL: wc -l counts lines"
test "$STATUS" -eq 0 && echo 1 > ok_wc || echo 1 > bad_wc

# --- head / tail ---
head -n 1 multi.txt > act_head.txt
printf 'line one\n' > exp_head.txt
cmp -s act_head.txt exp_head.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: head -n 1" || echo "FAIL: head -n 1"
test "$STATUS" -eq 0 && echo 1 > ok_head || echo 1 > bad_head

tail -n 1 multi.txt > act_tail.txt
printf 'line two\n' > exp_tail.txt
cmp -s act_tail.txt exp_tail.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: tail -n 1" || echo "FAIL: tail -n 1"
test "$STATUS" -eq 0 && echo 1 > ok_tail || echo 1 > bad_tail

# --- cp / mv / rm ---
cp multi.txt copy.txt
test -f copy.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: cp created the destination" || echo "FAIL: cp created the destination"
test "$STATUS" -eq 0 && echo 1 > ok_cp || echo 1 > bad_cp

mv copy.txt moved.txt
test -f moved.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: mv created the new name" || echo "FAIL: mv created the new name"
test "$STATUS" -eq 0 && echo 1 > ok_mv_new || echo 1 > bad_mv_new

test -f copy.txt
STATUS=$?
test "$STATUS" -ne 0 && echo "PASS: mv removed the old name" || echo "FAIL: mv left the old name behind"
test "$STATUS" -ne 0 && echo 1 > ok_mv_old || echo 1 > bad_mv_old

rm moved.txt
test -f moved.txt
STATUS=$?
test "$STATUS" -ne 0 && echo "PASS: rm removed the file" || echo "FAIL: rm did not remove the file"
test "$STATUS" -ne 0 && echo 1 > ok_rm || echo 1 > bad_rm

# --- mkdir -p / rmdir (nested) ---
mkdir -p nest/deep
test -d nest/deep
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: mkdir -p created nested dirs" || echo "FAIL: mkdir -p created nested dirs"
test "$STATUS" -eq 0 && echo 1 > ok_mkdirp || echo 1 > bad_mkdirp

rmdir nest/deep
rmdir nest
test -d nest
STATUS=$?
test "$STATUS" -ne 0 && echo "PASS: rmdir removed nested dirs" || echo "FAIL: rmdir left a directory behind"
test "$STATUS" -ne 0 && echo 1 > ok_rmdir || echo 1 > bad_rmdir

# --- cut / sort / uniq ---
printf 'b:2\na:1\na:1\nc:3\n' > kv.txt

cut -d: -f1 kv.txt > act_cut.txt
printf 'b\na\na\nc\n' > exp_cut.txt
cmp -s act_cut.txt exp_cut.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: cut -d: -f1" || echo "FAIL: cut -d: -f1"
test "$STATUS" -eq 0 && echo 1 > ok_cut || echo 1 > bad_cut

sort kv.txt > act_sort.txt
printf 'a:1\na:1\nb:2\nc:3\n' > exp_sort.txt
cmp -s act_sort.txt exp_sort.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: sort" || echo "FAIL: sort"
test "$STATUS" -eq 0 && echo 1 > ok_sort || echo 1 > bad_sort

sort -u kv.txt > act_sortu.txt
printf 'a:1\nb:2\nc:3\n' > exp_sortu.txt
cmp -s act_sortu.txt exp_sortu.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: sort -u" || echo "FAIL: sort -u"
test "$STATUS" -eq 0 && echo 1 > ok_sortu || echo 1 > bad_sortu

sort kv.txt | uniq > act_uniq.txt
cmp -s act_uniq.txt exp_sortu.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: sort | uniq" || echo "FAIL: sort | uniq"
test "$STATUS" -eq 0 && echo 1 > ok_uniq || echo 1 > bad_uniq

# --- grep ---
grep 'a:1' kv.txt > act_grep.txt
printf 'a:1\na:1\n' > exp_grep.txt
cmp -s act_grep.txt exp_grep.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: grep matches" || echo "FAIL: grep matches"
test "$STATUS" -eq 0 && echo 1 > ok_grep || echo 1 > bad_grep

grep -q zzz kv.txt
STATUS=$?
test "$STATUS" -ne 0 && echo "PASS: grep -q reports no match" || echo "FAIL: grep -q should not have matched"
test "$STATUS" -ne 0 && echo 1 > ok_grepq || echo 1 > bad_grepq

# --- sed ---
echo abc > act_sed_in.txt
sed 's/b/X/' act_sed_in.txt > act_sed.txt
printf 'aXc\n' > exp_sed.txt
cmp -s act_sed.txt exp_sed.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: sed substitution" || echo "FAIL: sed substitution"
test "$STATUS" -eq 0 && echo 1 > ok_sed || echo 1 > bad_sed

# --- tr ---
tr a-z A-Z < act_sed_in.txt > act_tr.txt
printf 'ABC\n' > exp_tr.txt
cmp -s act_tr.txt exp_tr.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: tr uppercases" || echo "FAIL: tr uppercases"
test "$STATUS" -eq 0 && echo 1 > ok_tr || echo 1 > bad_tr

# --- basename / dirname ---
basename /a/b/c.txt > act_basename.txt
printf 'c.txt\n' > exp_basename.txt
cmp -s act_basename.txt exp_basename.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: basename" || echo "FAIL: basename"
test "$STATUS" -eq 0 && echo 1 > ok_basename || echo 1 > bad_basename

dirname /a/b/c.txt > act_dirname.txt
printf '/a/b\n' > exp_dirname.txt
cmp -s act_dirname.txt exp_dirname.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: dirname" || echo "FAIL: dirname"
test "$STATUS" -eq 0 && echo 1 > ok_dirname || echo 1 > bad_dirname

# --- seq ---
seq 1 3 > act_seq.txt
printf '1\n2\n3\n' > exp_seq.txt
cmp -s act_seq.txt exp_seq.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: seq" || echo "FAIL: seq"
test "$STATUS" -eq 0 && echo 1 > ok_seq || echo 1 > bad_seq

# --- xargs ---
printf 'one\ntwo\n' | xargs -n 1 > act_xargs.txt
printf 'one\ntwo\n' > exp_xargs.txt
cmp -s act_xargs.txt exp_xargs.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: xargs -n 1 (default echo)" || echo "FAIL: xargs -n 1 (default echo)"
test "$STATUS" -eq 0 && echo 1 > ok_xargs || echo 1 > bad_xargs

# --- chmod ---
chmod 600 a.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: chmod succeeds" || echo "FAIL: chmod succeeds"
test "$STATUS" -eq 0 && echo 1 > ok_chmod || echo 1 > bad_chmod

# --- whoami (real uid/passwd-db path, see CLAUDE.md's Permission model section) ---
whoami > act_whoami.txt
printf 'root\n' > exp_whoami.txt
cmp -s act_whoami.txt exp_whoami.txt
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: whoami reports root" || echo "FAIL: whoami reports root"
test "$STATUS" -eq 0 && echo 1 > ok_whoami || echo 1 > bad_whoami

# --- kill -0 on our own pid (real POSIX existence-check convention) ---
kill -0 $$
STATUS=$?
test "$STATUS" -eq 0 && echo "PASS: kill -0 finds a live process" || echo "FAIL: kill -0 finds a live process"
test "$STATUS" -eq 0 && echo 1 > ok_kill || echo 1 > bad_kill

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
echo "pass count:"
ls ok_* 2>/dev/null | wc -l
echo "fail count:"
ls bad_* 2>/dev/null | wc -l

echo "scratch files left at /test_busybox_scratch (act_*.txt/exp_*.txt/ok_*/bad_*) for inspection"
