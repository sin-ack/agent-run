mod seccomp;
#[macro_use]
mod tracing;

use std::{
    borrow::Cow,
    ffi::{CStr, CString},
    io::{PipeWriter, Write},
    os::{
        fd::{AsFd as _, AsRawFd as _, FromRawFd, IntoRawFd as _, OwnedFd},
        unix::ffi::OsStringExt as _,
    },
    path::PathBuf,
    process::ExitCode,
};

use nix::unistd::ForkResult;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "schema")]
use schemars::{JsonSchema, json_schema, schema_for};

static BUBBLEWRAP_BINARY: &[u8] = include_bytes!(env!("BUBBLEWRAP_PATH"));

fn cstring(value: impl Into<Vec<u8>>, description: &str) -> anyhow::Result<CString> {
    CString::new(value)
        .map_err(|e| anyhow::anyhow!("Failed to convert {description} to a C string: {e}"))
}

#[derive(Error, Debug)]
enum BubblewrapError {
    #[error("Failed to create memfd for bwrap: {0}")]
    MemfdCreate(nix::errno::Errno),
    #[error("Failed to write bwrap binary to memfd: {0}")]
    Write(std::io::Error),
    #[error("Failed to set permissions on bwrap memfd: {0}")]
    Fchmod(nix::errno::Errno),
    #[error("Failed to seal bwrap memfd: {0}")]
    Fcntl(nix::errno::Errno),
    #[error("Failed to fork for bwrap: {0}")]
    Fork(nix::errno::Errno),
    #[error("Failed to execute bwrap: {0}")]
    Execveat(nix::errno::Errno),
    #[error("Failed to wait for bwrap: {0}")]
    Waitpid(nix::errno::Errno),
    #[error("Failed to close bwrap memfd: {0}")]
    MemfdClose(nix::errno::Errno),
    #[error("bwrap was terminated by signal: {0:?}")]
    BwrapSignaled(nix::sys::signal::Signal),
    #[error("bwrap exited with unexpected status: {0:?}")]
    BwrapUnexpectedStatus(nix::sys::wait::WaitStatus),
}

fn exec_bwrap<SA: AsRef<CStr>, SE: AsRef<CStr>>(
    argv: &[SA],
    envp: &[SE],
) -> Result<ExitCode, BubblewrapError> {
    let fd = match nix::sys::memfd::memfd_create(
        "bubblewrap",
        nix::sys::memfd::MFdFlags::from_bits_retain(
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | libc::MFD_EXEC,
        ),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            if e == nix::errno::Errno::EINVAL {
                // MEMFD_EXEC is not supported in this kernel, so we fall back to a regular memfd.
                match nix::sys::memfd::memfd_create(
                    "bubblewrap",
                    nix::sys::memfd::MFdFlags::MFD_CLOEXEC
                        | nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
                ) {
                    Ok(fd) => fd,
                    Err(e) => {
                        return Err(BubblewrapError::MemfdCreate(e));
                    }
                }
            } else {
                return Err(BubblewrapError::MemfdCreate(e));
            }
        }
    };

    let fd = {
        let mut writer = PipeWriter::from(fd);
        writer
            .write_all(BUBBLEWRAP_BINARY)
            .map_err(BubblewrapError::Write)?;

        nix::sys::stat::fchmod(
            writer.as_fd(),
            nix::sys::stat::Mode::from_bits_truncate(0o500),
        )
        .map_err(BubblewrapError::Fchmod)?;

        nix::fcntl::fcntl(
            writer.as_fd(),
            nix::fcntl::FcntlArg::F_ADD_SEALS(
                nix::fcntl::SealFlag::F_SEAL_WRITE
                    | nix::fcntl::SealFlag::F_SEAL_GROW
                    | nix::fcntl::SealFlag::F_SEAL_SHRINK
                    | nix::fcntl::SealFlag::F_SEAL_SEAL,
            ),
        )
        .map_err(BubblewrapError::Fcntl)?;

        // SAFETY: We just created this fd and are not using it anywhere else,
        //         so it's guaranteed to be valid.
        unsafe { OwnedFd::from_raw_fd(writer.into_raw_fd()) }
    };

    // SAFETY: Nothing else is happening so it's safe.
    match unsafe { nix::unistd::fork() } {
        Ok(ForkResult::Parent { child }) => {
            nix::unistd::close(fd).map_err(BubblewrapError::MemfdClose)?;
            let status =
                nix::sys::wait::waitpid(Some(child), None).map_err(BubblewrapError::Waitpid)?;
            match status {
                nix::sys::wait::WaitStatus::Exited(_, code) => Ok(ExitCode::from(code as u8)),
                nix::sys::wait::WaitStatus::Signaled(_, signal, _) => {
                    Err(BubblewrapError::BwrapSignaled(signal))
                }
                _ => Err(BubblewrapError::BwrapUnexpectedStatus(status)),
            }
        }
        Ok(ForkResult::Child) => {
            nix::unistd::execveat(fd, c"", argv, envp, nix::fcntl::AtFlags::AT_EMPTY_PATH)
                .map_err(BubblewrapError::Execveat)?;

            unreachable!();
        }
        Err(e) => Err(BubblewrapError::Fork(e)),
    }
}

#[derive(Debug)]
enum EnvironmentVariableValue {
    /// Inherit from the host environment.  If the variable is not set in the
    /// host environment, it will be unset for the tool.
    Inherit,
    /// Set it to a literal value.  The value can be the empty string, in which
    /// case the variable will be set to the empty string for the tool.
    Literal(String),
}

/// An environment variable to set for a tool.
#[derive(Debug)]
struct EnvironmentVariable {
    /// The key of the environment variable.
    key: String,
    /// The value of the environment variable.
    value: EnvironmentVariableValue,
}

impl Serialize for EnvironmentVariable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.value {
            EnvironmentVariableValue::Inherit => serializer.serialize_str(&self.key),
            EnvironmentVariableValue::Literal(value) => {
                let s = format!("{}={}", self.key, value);
                serializer.serialize_str(&s)
            }
        }
    }
}

impl<'de> Deserialize<'de> for EnvironmentVariable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.contains('\0') {
            return Err(serde::de::Error::custom(
                "Environment variable entry cannot contain a NUL byte",
            ));
        }

        let parts: Vec<&str> = s.splitn(2, '=').collect();
        let (key, value) = match parts.len() {
            1 => (parts[0].to_string(), EnvironmentVariableValue::Inherit),
            2 => (
                parts[0].to_string(),
                EnvironmentVariableValue::Literal(parts[1].to_string()),
            ),
            _ => unreachable!(),
        };

        if key.is_empty() {
            return Err(serde::de::Error::custom(
                "Environment variable key cannot be empty",
            ));
        }

        Ok(EnvironmentVariable { key, value })
    }
}

#[cfg(feature = "schema")]
impl JsonSchema for EnvironmentVariable {
    fn schema_name() -> Cow<'static, str> {
        "EnvironmentVariable".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "string",
            "description": "An environment variable to set for a tool.  \
                            Two formats are supported: `KEY=VALUE` sets the environment variable `KEY` to `VALUE`.  \
                            `KEY` sets the environment variable `KEY` to the value of the environment variable `KEY` that agent-run sees.  \
                            If `KEY` is not set in the environment, it will be unset for the tool.",
            "pattern": r"^[^=\u0000]+(=[^\u0000]*)?$",
        })
    }
}

/// Configuration for a specific tool.  Tools are matched based on the basename
/// of the first argument passed to the agent-run command.  If no tool matches,
/// the global configuration is used.
#[derive(Serialize, Deserialize, Default, Debug)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
struct ToolConfig {
    /// Whether to enable network access.  If false, the tool will not be able
    /// to use the network.  Default is true.
    #[serde(default)]
    network: Option<bool>,

    /// Whether to inherit the default environment variables from host.
    /// If false, the tool will only expose environment variables explicitly
    /// set in the [`env`][Self::env] field.  Default is true.
    #[serde(default)]
    inherit_env: Option<bool>,

    /// Directories to mount as read-write to the sandbox.  Currently, each
    /// directory is identity-mapped.
    ///
    /// The full filesystem is always visible as read-only to the tool so that
    /// running commands from the host etc. works.  This is useful for tools
    /// that need to write to the filesystem as well.
    ///
    /// Tilde expansion for `~/` works.  Environment variables are currently not
    /// expanded.  Relative paths are relative to the configuration file;
    /// absolute paths are left as-is.  No globs for the time being.  If a
    /// directory does not exist it will not be mounted and a diagnostic will be
    /// printed.
    #[serde(default)]
    mount: Vec<std::path::PathBuf>,

    /// Environment variables to set for the tool.
    ///
    /// Two formats are supported:
    ///
    /// - `KEY=VALUE` sets the environment variable `KEY` to `VALUE`.
    ///   `VALUE` can be the empty string, in which case `KEY` will be set to
    ///   the empty string.
    /// - `KEY` sets the environment variable `KEY` to the value of the
    ///   environment variable `KEY` that agent-run sees.  If `KEY`
    ///   is not set in the environment, it will be unset for the tool.
    #[serde(default)]
    env: Vec<EnvironmentVariable>,
}

/// Configuration for agent-run.
#[derive(Serialize, Deserialize, Default, Debug)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
struct Config {
    /// Global configuration for all tools.
    #[serde(default)]
    global: ToolConfig,

    /// Per-tool configuration.
    #[serde(default)]
    tools: std::collections::HashMap<String, ToolConfig>,
}

/// Command-line arguments for agent-run.
#[derive(Debug)]
struct Args {
    config: Option<std::path::PathBuf>,
    command: Vec<String>,
}

static HELP_TEXT: &str = r#"Run a coding agent in a sandboxed environment.

Usage: agent-run [OPTIONS] [--] <COMMAND>...

Arguments:
  <COMMAND>...  The command to run in the sandbox. The first argument is used to
                determine which tool configuration to use

Options:
      --config <CONFIG>  Path to the configuration file. If not specified, the closest
                         `.agent-run/config.toml` file is used
  -h, --help             Print help
"#;

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut config = None;
        let mut command = Vec::new();
        while let Some(arg) = args.next() {
            if !command.is_empty() {
                command.push(arg);
            } else if arg == "--" {
                command.extend(args);
                break;
            } else if arg == "--config" {
                config = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?
                        .into(),
                );
            } else if let Some(path) = arg.strip_prefix("--config=") {
                config = Some(path.into());
            } else if cfg!(feature = "schema") && arg == "--schema" {
                #[cfg(feature = "schema")]
                {
                    let path = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--schema requires a path"))?;

                    let schema = schema_for!(Config);
                    let mut file = std::fs::File::create(&path).map_err(|e| {
                        anyhow::anyhow!("Failed to create schema file {}: {}", path, e)
                    })?;

                    write!(file, "{}", serde_json::to_string_pretty(&schema).unwrap()).map_err(
                        |e| anyhow::anyhow!("Failed to write schema file {}: {}", path, e),
                    )?;
                    std::process::exit(0);
                }

                #[cfg(not(feature = "schema"))]
                {
                    unreachable!();
                }
            } else if arg == "-h" || arg == "--help" {
                print!("{}", HELP_TEXT);
                std::process::exit(0);
            } else if arg.starts_with('-') {
                anyhow::bail!("unknown option: {arg}");
            } else {
                command.push(arg);
            }
        }
        if command.is_empty() {
            anyhow::bail!("no command specified (try --help)");
        }
        Ok(Self { config, command })
    }
}

#[derive(Error, Debug)]
enum ConfigError {
    #[error("Failed to read configuration file: {0}")]
    Read(#[from] std::io::Error),
    #[error("Failed to parse configuration file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Duplicate environment variable entries for {0}")]
    DuplicateEnvEntries(String),
}

fn parse_config(path: Option<&std::path::Path>) -> Result<(Config, Option<PathBuf>), ConfigError> {
    let path = match path {
        Some(path) => {
            log_debug!("Using configuration file: {}", path.display());
            Some(path.to_path_buf())
        }
        None => {
            log_debug!("No configuration file specified, searching for .agent-run/config.toml");

            // Find the closest .agent-run/config.toml file.
            let mut dir = std::env::current_dir()?;
            loop {
                let config_path = dir.join(".agent-run").join("config.toml");
                log_trace!("Checking for configuration file: {}", config_path.display());

                if config_path.exists() {
                    log_debug!("Found configuration file: {}", config_path.display());
                    break Some(config_path);
                }
                if !dir.pop() {
                    // No config file found.  We will generate a default configuration below.
                    break None;
                }
            }
        }
    };

    let mut config = match &path {
        Some(path) => {
            let config_str = std::fs::read_to_string(path)?;
            let config: Config = toml::from_str(&config_str)?;
            log_debug!("Parsed configuration file: {}", path.display());
            config
        }
        None => {
            log_debug!("No configuration file found, using default configuration");
            Config::default()
        }
    };

    // Ensure we don't have duplicate env entries.
    if config.global.env.len()
        != config
            .global
            .env
            .iter()
            .map(|e| &e.key)
            .collect::<std::collections::HashSet<_>>()
            .len()
    {
        return Err(ConfigError::DuplicateEnvEntries("global".to_string()));
    }

    for (tool_name, tool_config) in &config.tools {
        if tool_config.env.len()
            != tool_config
                .env
                .iter()
                .map(|e| &e.key)
                .collect::<std::collections::HashSet<_>>()
                .len()
        {
            return Err(ConfigError::DuplicateEnvEntries(format!(
                "tools.{}",
                tool_name
            )));
        }
    }

    // Network is enabled if unspecified by the user.
    config.global.network = config.global.network.or(Some(true));
    // Inherit environment variables from host if unspecified by the user.
    config.global.inherit_env = config.global.inherit_env.or(Some(true));

    Ok((config, path))
}

fn merge_configs(global: ToolConfig, tool: ToolConfig) -> ToolConfig {
    ToolConfig {
        network: tool.network.or(global.network),
        inherit_env: tool.inherit_env.or(global.inherit_env),
        mount: {
            let mut mount = global.mount;
            mount.extend(tool.mount);
            mount
        },
        env: {
            let mut env = Vec::with_capacity(global.env.len() + tool.env.len());
            env.extend(global.env);

            for e in tool.env {
                let matching_env_index = env.iter().position(|ee| ee.key == e.key);
                if let Some(index) = matching_env_index {
                    env[index] = e;
                } else {
                    env.push(e);
                }
            }

            env
        },
    }
}

#[derive(Error, Debug)]
enum ToolNameError {
    #[error("No command specified")]
    NoCommand,
    #[error("Failed to get tool name from command")]
    InvalidCommand,
}

fn tool_name(command: &[String]) -> Result<String, ToolNameError> {
    if command.is_empty() {
        return Err(ToolNameError::NoCommand);
    }

    let tool_name = std::path::Path::new(&command[0])
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    match tool_name {
        Some(name) => Ok(name),
        None => Err(ToolNameError::InvalidCommand),
    }
}

#[derive(Error, Debug)]
enum PathResolutionError {
    #[error("Failed to resolve path {0}: {1}")]
    Resolve(PathBuf, std::io::Error),
    #[error("Failed to canonicalize path {0}: {1}")]
    Canonicalize(PathBuf, std::io::Error),
}

fn resolve_path(
    path: &std::path::Path,
    config_path: Option<&std::path::Path>,
) -> Result<Option<PathBuf>, PathResolutionError> {
    let expanded = shellexpand::tilde(path.to_str().ok_or_else(|| {
        PathResolutionError::Resolve(
            path.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"),
        )
    })?)
    .into_owned();
    log_trace!("Expanded path {} -> {}", path.display(), expanded);
    let expanded_path = PathBuf::from(expanded);

    if expanded_path.is_absolute() {
        log_trace!("Path is absolute, nothing to do");

        if expanded_path.exists() {
            Ok(Some(expanded_path))
        } else {
            Ok(None)
        }
    } else {
        log_trace!("Path is relative, resolving relative to config file");
        let expanded_path = config_path
            .map(|config_path| {
                config_path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap())
                    .join(&expanded_path)
            })
            .unwrap_or(expanded_path);
        log_trace!(
            "Resolved relative path {} -> {}",
            path.display(),
            expanded_path.display()
        );
        match std::fs::canonicalize(&expanded_path) {
            Ok(canonicalized_path) => {
                log_trace!(
                    "Canonicalized path {} -> {}",
                    expanded_path.display(),
                    canonicalized_path.display()
                );
                Ok(Some(canonicalized_path))
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    log_trace!(
                        "Path {} does not exist, skipping mount",
                        expanded_path.display()
                    );
                    Ok(None)
                } else {
                    Err(PathResolutionError::Canonicalize(expanded_path, e))
                }
            }
        }
    }
}

fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse()?;
    log_trace!("Parsed command-line arguments: {:#?}", args);

    let (mut config, config_path) = parse_config(args.config.as_deref())?;
    log_trace!("Parsed configuration: {:#?}", config);

    let tool = tool_name(&args.command)?;
    log_trace!("Determined tool name: {}", tool);

    let tool_config = match config.tools.remove(&tool) {
        Some(tool_config) => {
            log_debug!(
                "Found tool-specific configuration for {}, merging with global configuration",
                tool
            );
            merge_configs(config.global, tool_config)
        }
        None => {
            log_debug!(
                "No tool-specific configuration for {}, using global configuration",
                tool
            );
            config.global
        }
    };
    log_trace!("Merged tool configuration: {:#?}", tool_config);

    let seccomp_filter_fd = seccomp::create_tiocsti_filter()
        .map_err(|e| anyhow::anyhow!("Failed to create seccomp filter: {e}"))?;

    let mut argv: Vec<Cow<CStr>> = vec![
        Cow::Borrowed(c"bwrap"),
        Cow::Borrowed(c"--unshare-all"),
        Cow::Borrowed(c"--die-with-parent"),
        Cow::Borrowed(c"--seccomp"),
        Cow::Owned(cstring(
            seccomp_filter_fd.as_raw_fd().to_string(),
            "seccomp file descriptor",
        )?),
    ];

    let network = tool_config
        .network
        .expect("Network should have a default value");
    if network {
        log_debug!("Enabling network access");
        argv.push(Cow::Borrowed(c"--share-net"));
    } else {
        log_debug!("Disabling network access");
    }

    // Always read-only mount the root filesystem.
    argv.push(Cow::Borrowed(c"--ro-bind"));
    argv.push(Cow::Borrowed(c"/"));
    argv.push(Cow::Borrowed(c"/"));

    // Mount new proc and dev filesystems to avoid leaking host information.
    argv.push(Cow::Borrowed(c"--proc"));
    argv.push(Cow::Borrowed(c"/proc"));
    argv.push(Cow::Borrowed(c"--dev"));
    argv.push(Cow::Borrowed(c"/dev"));

    for mount in tool_config.mount {
        let Some(expanded) = resolve_path(&mount, config_path.as_deref()).map_err(|e| {
            anyhow::anyhow!("Failed to resolve mount path {}: {}", mount.display(), e)
        })?
        else {
            eprintln!(
                "Warning: Mount path {} does not exist, skipping",
                mount.display()
            );
            continue;
        };

        log_debug!("Mounting {} as read-write", expanded.display());

        argv.push(Cow::Borrowed(c"--bind"));
        let path = cstring(expanded.into_os_string().into_encoded_bytes(), "mount path")?;
        argv.push(Cow::Owned(path.clone()));
        argv.push(Cow::Owned(path));
    }

    let inherit_env = tool_config
        .inherit_env
        .expect("Inherit env should have a default value");
    if !inherit_env {
        log_debug!("Clearing environment variables");
        argv.push(Cow::Borrowed(c"--clearenv"));
    } else {
        log_debug!("Inheriting environment variables from host");
    }

    for env in tool_config.env {
        match env.value {
            EnvironmentVariableValue::Inherit => {
                match std::env::var_os(&env.key) {
                    Some(value) => {
                        log_trace!("Inheriting environment variable {}={:?}", env.key, value);

                        argv.push(Cow::Borrowed(c"--setenv"));
                        argv.push(Cow::Owned(cstring(env.key, "environment variable key")?));
                        argv.push(Cow::Owned(cstring(
                            value.into_vec(),
                            "inherited environment variable value",
                        )?));
                    }
                    None => {
                        log_trace!(
                            "Environment variable {} is not set in host environment, unsetting for tool",
                            env.key
                        );

                        // If the variable is not set in the host environment, it will be unset for the tool.
                        argv.push(Cow::Borrowed(c"--unsetenv"));
                        argv.push(Cow::Owned(cstring(env.key, "environment variable key")?));
                    }
                }
            }
            EnvironmentVariableValue::Literal(value) => {
                log_trace!("Setting environment variable {}={}", env.key, value);

                argv.push(Cow::Borrowed(c"--setenv"));
                argv.push(Cow::Owned(cstring(env.key, "environment variable key")?));
                argv.push(Cow::Owned(cstring(value, "environment variable value")?));
            }
        }
    }

    argv.push(Cow::Borrowed(c"--"));
    for argument in args.command {
        argv.push(Cow::Owned(cstring(argument, "command argument")?));
    }

    let mut envp: Vec<CString> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = key.into_vec();
        entry.push(b'=');
        entry.extend(value.into_vec());
        envp.push(cstring(entry, "host environment entry")?);
    }

    log_trace!(
        "Executing bwrap with arguments: {:?}",
        argv.iter().map(|s| s.as_ref()).collect::<Vec<_>>()
    );
    log_trace!(
        "Executing bwrap with environment: {:?}",
        envp.iter().map(|s| s.as_c_str()).collect::<Vec<_>>()
    );

    let result = exec_bwrap(argv.as_slice(), envp.as_slice());
    drop(seccomp_filter_fd);
    result.map_err(|e| anyhow::anyhow!("Failed to execute bwrap: {}", e))
}
