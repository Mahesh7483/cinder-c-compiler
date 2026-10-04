#!/usr/bin/env bash
# Benchmark cinder at -O0/-O1/-O2 (and gcc as a yardstick) on tests/bench/*.c.
#
#   scripts/bench.sh [-n RUNS] [-c CINDER_BIN] [-o RESULTS.md] [benchmark ...]
#
# Every binary must print exactly what the gcc -O0 build prints (a wrong answer
# is reported as FAIL instead of a time). Times are the best of RUNS runs of
# wall-clock seconds. Needs gcc and a built cinder (default /target/debug/cinder
# in the dev container, otherwise target/release/cinder).
set -euo pipefail
export LC_ALL=C

runs=3
out=""
root="$(cd "$(dirname "$0")/.." && pwd)"
cinder=""
while getopts "n:c:o:" opt; do
    case "$opt" in
        n) runs="$OPTARG" ;;
        c) cinder="$OPTARG" ;;
        o) out="$OPTARG" ;;
        *) echo "usage: $0 [-n RUNS] [-c CINDER_BIN] [-o RESULTS.md] [benchmark ...]" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))

if [ -z "$cinder" ]; then
    for c in /target/release/cinder "$root/target/release/cinder" /target/debug/cinder "$root/target/debug/cinder"; do
        if [ -x "$c" ]; then cinder="$c"; break; fi
    done
fi
[ -x "$cinder" ] || { echo "no cinder binary found (build with cargo build --release, or pass -c)" >&2; exit 2; }
command -v gcc >/dev/null || { echo "gcc is needed as the reference" >&2; exit 2; }

if [ "$#" -gt 0 ]; then names=("$@"); else names=(); for f in "$root"/tests/bench/*.c; do names+=("$(basename "$f" .c)"); done; fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# best-of-N wall time in seconds (3 decimals) of: $1 = executable
best_time() {
    local best="" t0 t1 ms
    for _ in $(seq "$runs"); do
        t0=$(date +%s%N)
        "$1" > /dev/null
        t1=$(date +%s%N)
        ms=$(( (t1 - t0) / 1000000 ))
        if [ -z "$best" ] || [ "$ms" -lt "$best" ]; then best="$ms"; fi
    done
    awk -v ms="$best" 'BEGIN { printf "%.3f", ms / 1000 }'
}

fmt_ratio() { awk -v a="$1" -v b="$2" 'BEGIN { if (b > 0) printf "%.2f", a / b; else printf "-" }'; }

report=""
add() { report+="$1"$'\n'; }

add "| benchmark | cinder -O0 | cinder -O1 | cinder -O2 | -O0 → -O2 | gcc -O0 | gcc -O2 | cinder -O2 / gcc -O2 |"
add "|-----------|-----------:|-----------:|-----------:|----------:|--------:|--------:|---------------------:|"

declare -A sum_ratio
for n in "${names[@]}"; do
    src="$root/tests/bench/$n.c"
    [ -f "$src" ] || { echo "no such benchmark: $n" >&2; exit 2; }
    gcc -O0 -w -fno-builtin -o "$work/$n.gcc0" "$src" -lm
    gcc -O2 -w -o "$work/$n.gcc2" "$src" -lm
    "$work/$n.gcc0" > "$work/$n.expected"
    row="| $n |"
    declare -A t=()
    for lvl in 0 1 2; do
        if "$cinder" "-O$lvl" -o "$work/$n.c$lvl" "$src" -lm 2> "$work/$n.err" && "$work/$n.c$lvl" > "$work/$n.out$lvl" && cmp -s "$work/$n.out$lvl" "$work/$n.expected"; then
            t[$lvl]=$(best_time "$work/$n.c$lvl")
        else
            t[$lvl]="FAIL"
            echo "FAIL: $n at -O$lvl" >&2
            head -5 "$work/$n.err" >&2 || true
        fi
        row+=" ${t[$lvl]} s |"
    done
    if [ "${t[0]}" != FAIL ] && [ "${t[2]}" != FAIL ]; then row+=" $(fmt_ratio "${t[0]}" "${t[2]}")× |"; else row+=" - |"; fi
    g0=$(best_time "$work/$n.gcc0")
    g2=$(best_time "$work/$n.gcc2")
    row+=" $g0 s | $g2 s |"
    if [ "${t[2]}" != FAIL ]; then row+=" $(fmt_ratio "${t[2]}" "$g2")× |"; else row+=" - |"; fi
    add "$row"
    echo "done: $n" >&2
done

add ""
add "Best of $runs runs, wall-clock seconds; $(gcc --version | head -1); cinder $("$cinder" --version | head -1)."
printf '%s' "$report"
if [ -n "$out" ]; then printf '%s' "$report" > "$out"; fi
