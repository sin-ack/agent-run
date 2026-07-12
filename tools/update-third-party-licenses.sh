#!/usr/bin/env bash

set -euo pipefail

cargo_about=${CARGO_ABOUT:-cargo-about}
output=$(mktemp)
trap 'rm -f "$output"' EXIT

"$cargo_about" generate --locked --fail --output-file "$output" about.hbs

append_license() {
    local heading=$1
    local path=$2

    {
        printf '\n### %s\n\n```text\n' "$heading"
        cat "$path"
        printf '\n```\n'
    } >>"$output"
}

append_license "bubblewrap 0.11.2" third_party/licenses/bubblewrap-LGPL-2.0-or-later.txt
append_license "libcap 2.27" third_party/licenses/libcap-2.27.txt
append_license "Rust standard library copyright notices" third_party/licenses/rust-COPYRIGHT.txt
append_license "Rust standard library MIT license" third_party/licenses/rust-LICENSE-MIT.txt
append_license "Rust standard library Apache 2.0 license" third_party/licenses/rust-LICENSE-APACHE.txt

mv "$output" THIRD_PARTY_LICENSES.md
trap - EXIT
