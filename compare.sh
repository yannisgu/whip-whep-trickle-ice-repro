#!/bin/sh
# Runs both scenarios against upstream gst-plugins-rs main and the fork
# branch, and prints the summary of each run
set -e
cd "$(dirname "$0")"

for plugin in upstream fork; do
    cargo build --quiet --features "$plugin" --target-dir "target/$plugin"
    for mode in whip whep; do
        "target/$plugin/debug/whip-whep-trickle-ice-repro" "$mode" 2>/dev/null \
            | sed -n '/^===/,$p'
        echo
    done
done
