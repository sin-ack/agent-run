# agent-run

Run a coding agent in a sandboxed environment.

## Features

- Tiny standalone binary (<1MB).  Runs on any recent GNU/Linux system[^1].
- Simple TOML-based configuration.
- JSON schema for config assistance.

[^1]: Currently aarch64 and x86_64 are directly supported.

## Installation

- Download the binary from the latest release: https://github.com/sin-ack/agent-run/releases/latest
- Put it somewhere in your PATH, e.g. `$HOME/.local/bin`.
- Write your [configuration](#configuration).
- Run your agent: `agent-run <pi|opencode|codex|claude>`.

That's it!

> [!NOTE]
> agent-run uses bwrap so it only works on a Linux system.  Additionally you
> need unprivileged user namespaces enabled.

> [!NOTE]
> agent-run won't work on NixOS at the moment since it embeds a dynamically-linked
> bwrap binary.  I might add a build configuration for this later.

## Configuration

### Location

Configuration is in TOML. There is a JSON schema you can use with tools like
[Taplo](https://taplo.tamasfe.dev/) to get completions in your editor (see
below).

`agent-run` finds its configuration in two ways:
- An explicit `--config` argument
- The closest `.agent-run/config.toml` in the directory hierarchy

If no configuration is found, the following default configuration is used:

``` toml
[global]
network = true
inherit_env = true
```

The tool configuration to use is selected by looking at the basename of the
first argument of the command.

Currently, no configuration merging across multiple configs is done.  This might
change later.

### Example for pi-coding-agent

``` toml
# Add this line to your TOML file to add support for completion:
#:schema https://github.com/sin-ack/agent-run/raw/master/schema.json

[global]
mount = [
    # Mount the user cache so tools that need it can write to it.
    "~/.cache",
    # Make the project directory writable.
    "..",
]

[tools.pi]
mount = [
    # Allow access to pi's own directory.
    "~/.pi",
    # Pi writes bash outputs to /tmp/pi-bash-*.log so we must make all of /tmp
    # writable.  :(
    "/tmp",
]
```

### Reference

Configuration in `agent-run` consists of "tool configs".  There is a global tool
config under `global` and one for each tool you define under `tools.<name>`.
Tool configs have the following keys:

- `inherit_env`: Whether to inherit the environment variables from host.
  Defaults to true.
- `network`: Whether to allow network access within the sandbox.
  Defaults to true.
- `mount`: Paths to mount as read-write.  The host filesystem is always mounted
  read-only; this only makes certain paths writable.  Tildes are expanded.
  Environment variables are not expanded.  If the path is relative, it is
  treated as relative to the configuration file itself.  Absolute paths are
  passed as-is.  We always identity-mount into the sandbox.  If the given path
  is not found a diagnostic warning is printed and the path is left read-only.
- `env`: Environment variables to pass to the sandbox.  If `inherit_env` is true,
  this is merged into the host environment variables.  Each argument is a string
  in one of two forms:
  - `KEY=VALUE`: Set `KEY` to `VALUE`.  `VALUE` can be the empty string in which
    case it is set to the empty string in the sandbox too.
  - `KEY`: Pass `KEY` from the host environment through.  If the variable is
    unset, it is left unset in the sandbox.  Otherwise, `KEY` is set to the
    host environment variable's value.

For a full reference see [schema.json](./schema.json).
  
## Troubleshooting

The environment variable `RUST_LOG` is respected, and supports `debug` and
`trace` for compact and verbose debug logs respectively.

## Why?

- Permission prompts are annoying.
- I want the agent to do anything it wants as long as it can't touch non-project files.
  I don't care about overlays, parallel agents or other fancy features.
- I want a small and simple tool, ideally just one binary I can put into my `bin/`.
- I want basic per-tool configurability.

Nothing satisfied all of these criteria so I made my own.

## Threat model

`agent-run` is primarily intended to catch *mistakes* made by agents.  Arbitrary
malicious code is not guaranteed to be handled perfectly safely.  In particular,
because the entire file system is exposed as read-only, information stealing
(when network access is enabled) and things like writing to user-writable socket
files is still possible.  If this is your threat model, consider using things
like microVMs.

## How?

- We build a [`bwrap`](https://github.com/containers/bwrap) binary for the target.
- We embed it inside the binary and exec into it via memfd, passing it the appropriate
  arguments from the config.
  
## Development

You need [Bazel](https://bazel.build).  I recommend using
[Bazelisk](https://github.com/bazelbuild/bazelisk).

- Run it via Bazel: `bazel run --run_in_cwd //:agent-run -- [args...]`
- Build a release binary: `bazel build --config=size --platforms=//platforms:[target] //:agent-run`
- Run tests (including Clippy): `bazel test //...`
- Run Clippy only: `bazel test //:clippy_test`
- Regenerate third-party license notices (requires
  [`cargo-about`](https://github.com/EmbarkStudios/cargo-about)):
  `tools/update-third-party-licenses.sh`
  
## License

This project is licensed under the GNU General Public License, version 3.
See [LICENSE](LICENSE) for details.  See
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) for third-party license
notices.
