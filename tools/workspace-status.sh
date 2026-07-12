#!/usr/bin/env bash

set -euo pipefail

version=$(
    awk '
        /^\[package\]$/ { in_package = 1; next }
        /^\[/ { in_package = 0 }
        in_package && $1 == "version" {
            gsub(/^"|"$/, "", $3)
            print $3
            exit
        }
    ' Cargo.toml
)

if [[ -z $version ]]; then
    echo "Failed to determine package version" >&2
    exit 1
fi

printf 'STABLE_AGENT_RUN_VERSION %s\n' "$version"
