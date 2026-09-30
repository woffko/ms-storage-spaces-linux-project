#!/bin/bash
# The long fuzzing run of the release checklist: every cargo-fuzz target for
# SECONDS (default 24 h), one worker each at nice 19 and at most 2 GiB of
# memory each, from and into fuzz/corpus/<target>. OUT (default
# testdata/work/fuzz-long) gets a log per target, the commit, the start and
# end times and each target's exit status in `results`; findings land in
# fuzz/artifacts/<target>. A run cut short (a host crash) is resumed by
# running again with the remaining seconds: the corpus keeps what it found.
# Usage: tools/fuzz-long.sh [SECONDS] [OUT] [TARGET...]
set -uo pipefail
cd "$(dirname "$0")/.."
seconds=${1:-86400}
out=${2:-testdata/work/fuzz-long}
shift $(($# < 2 ? $# : 2))
targets=("$@")
((${#targets[@]})) || mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' fuzz/Cargo.toml | grep -v '^storage-spaces-fuzz$')
mkdir -p "$out"
git rev-parse --short HEAD > "$out/commit"
git status --porcelain --untracked-files=no | grep -q . && echo "(with uncommitted changes)" >> "$out/commit"
for t in "${targets[@]}"; do
  (cd fuzz && nice -n 19 cargo +nightly fuzz build "$t" >/dev/null 2>&1) || { echo "cannot build $t" >&2; exit 1; }
done
date -Iseconds > "$out/started"
: > "$out/results"
for t in "${targets[@]}"; do
  mkdir -p "fuzz/corpus/$t" "fuzz/artifacts/$t"
  (
    cd fuzz &&
      nice -n 19 "target/x86_64-unknown-linux-gnu/release/$t" -artifact_prefix="artifacts/$t/" \
        -max_total_time="$seconds" -rss_limit_mb=2048 -timeout=300 -print_final_stats=1 "corpus/$t" \
        > "../$out/$t.log" 2>&1
    echo "$t exit $?" >> "../$out/results"
  ) &
done
wait
date -Iseconds > "$out/finished"
cat "$out/results"
