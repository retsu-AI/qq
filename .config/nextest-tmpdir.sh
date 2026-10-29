#!/usr/bin/env bash
# Session tests commit SQLite with `synchronous = FULL`; on a disk-backed
# TMPDIR, fsync is most of their wall time. Point tests at tmpfs when the
# host has a usable one. Set QQ_TEST_DISK_TMPDIR=1 to keep the default.
set -euo pipefail

[[ -n ${NEXTEST_ENV:-} ]] || exit 1
[[ -z ${QQ_TEST_DISK_TMPDIR:-} ]] || exit 0
[[ -d /dev/shm && -w /dev/shm ]] || exit 0
# Containers often mount a 64 MiB /dev/shm; session fixtures need far more.
available_kib=$(df -Pk /dev/shm 2>/dev/null | awk 'NR == 2 { print $4 }')
[[ ${available_kib:-0} -ge 2097152 ]] || exit 0

dir="/dev/shm/qq-test-$(id -u)"
mkdir -p -m 700 "$dir" 2>/dev/null || exit 0
[[ -O $dir ]] || exit 0
# Tempdirs of killed test processes are never dropped; reclaim old ones.
find "$dir" -mindepth 1 -maxdepth 1 -mmin +720 -exec rm -rf {} + 2>/dev/null || true

# Tests run scripts from TMPDIR; a noexec mount would break them.
probe=$(mktemp "$dir/probe.XXXXXX")
printf '#!/bin/sh\nexit 0\n' >"$probe"
chmod +x "$probe"
if ! "$probe" 2>/dev/null; then
  rm -f "$probe"
  exit 0
fi
rm -f "$probe"

echo "TMPDIR=$dir" >>"$NEXTEST_ENV"
