#[macro_use]
mod tracing;

use std::{
    borrow::Cow,
    ffi::{CStr, CString},
    io::{PipeWriter, Write},
    os::fd::{AsFd as _, FromRawFd, IntoRawFd as _, OwnedFd},
    path::PathBuf,
    process::ExitCode,
};

use clap::Parser;
use nix::unistd::ForkResult;
use serde::{Deserialize, Serialize};
use thiserror::Error;

static BUBBLEWRAP_BINARY: &[u8] = include_bytes!(env!("BUBBLEWRAP_PATH"));

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
                nix::sys::wait::WaitStatus::Exited(_, code) => {
                    return Ok(ExitCode::from(code as u8));
                }
                nix::sys::wait::WaitStatus::Signaled(_, signal, _) => {
                    return Err(BubblewrapError::BwrapSignaled(signal));
                }
                _ => {
                    return Err(BubblewrapError::BwrapUnexpectedStatus(status));
                }
            }
        }
        Ok(ForkResult::Child) => {
            nix::unistd::execveat(fd, c"", &argv, &envp, nix::fcntl::AtFlags::AT_EMPTY_PATH)
                .map_err(BubblewrapError::Execveat)?;

            unreachable!();
        }
        Err(e) => {
            return Err(BubblewrapError::Fork(e));
        }
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
        let parts: Vec<&str> = s.splitn(2, '=').collect();
        let (key, value) = match parts.len() {
            1 => (parts[0].to_string(), EnvironmentVariableValue::Inherit),
            2 => {
                if parts[0].is_empty() {
                    return Err(serde::de::Error::custom(
                        "Environment variable key cannot be empty",
                    ));
                }

                (
                    parts[0].to_string(),
                    EnvironmentVariableValue::Literal(parts[1].to_string()),
                )
            }
            _ => unreachable!(),
        };
        Ok(EnvironmentVariable { key, value })
    }
}

/// Configuration for a specific tool.  Tools are matched based on the basename
/// of the first argument passed to the agent-run command.  If no tool matches,
/// the global configuration is used.
#[derive(Serialize, Deserialize, Default, Debug)]
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
    /// Shell expansions such as `~` and `~user` work.  Environment variables
    /// are currently not expanded.  Relative paths are relative to the
    /// configuration file; absolute paths are left as-is. No globs for the time
    /// being.  If a directory does not exist it will not be mounted and a
    /// diagnostic will be printed.
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
struct Config {
    /// Global configuration for all tools.
    global: ToolConfig,

    /// Per-tool configuration.
    tools: std::collections::HashMap<String, ToolConfig>,
}

/// Command-line arguments for agent-run.
#[derive(Parser, Debug)]
struct Args {
    /// Path to the configuration file.  If not specified, the closest
    /// `.agent-run/config.toml` file is used.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// The command to run in the sandbox.  The first argument is used to
    /// determine which tool configuration to use.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Error, Debug)]
enum ConfigError {
    #[error("Failed to read configuration file: {0}")]
    Read(#[from] std::io::Error),
    #[error("Failed to parse configuration file: {0}")]
    Parse(#[from] toml::de::Error),
}

fn parse_config(path: Option<&std::path::Path>) -> Result<(Config, Option<PathBuf>), ConfigError> {
    let path = match path {
        Some(path) => {
            log_debug!("Using configuration file: {}", path.display());
            path.to_path_buf()
        }
        None => {
            log_debug!(
                "No configuration file specified, searching for .agent-run/config.toml"
            );

            // Find the closest .agent-run/config.toml file.
            let mut dir = std::env::current_dir()?;
            loop {
                let config_path = dir.join(".agent-run").join("config.toml");
                log_trace!("Checking for configuration file: {}", config_path.display());

                if config_path.exists() {
                    log_debug!("Found configuration file: {}", config_path.display());
                    break config_path;
                }
                if !dir.pop() {
                    // No config file found, return default config.
                    log_debug!("No configuration file found, using default configuration");
                    return Ok((Config::default(), None));
                }
            }
        }
    };

    let config_str = std::fs::read_to_string(&path)?;
    let mut config: Config = toml::from_str(&config_str)?;
    log_debug!("Parsed configuration file: {}", path.display());

    // Network is enabled if unspecified by the user.
    config.global.network = config.global.network.or(Some(true));
    // Inherit environment variables from host if unspecified by the user.
    config.global.inherit_env = config.global.inherit_env.or(Some(true));

    Ok((config, Some(path)))
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
    path: &PathBuf,
    config_path: Option<&PathBuf>,
) -> Result<PathBuf, PathResolutionError> {
    let expanded = shellexpand::tilde(path.to_str().ok_or_else(|| {
        PathResolutionError::Resolve(
            path.clone(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"),
        )
    })?)
    .into_owned();
    log_trace!("Expanded path {} -> {}", path.display(), expanded);
    let expanded_path = PathBuf::from(expanded);

    if expanded_path.is_absolute() {
        log_trace!("Path is absolute, nothing to do");
        Ok(expanded_path)
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
        let canonicalized_path = std::fs::canonicalize(&expanded_path)
            .map_err(|e| PathResolutionError::Canonicalize(expanded_path.clone(), e))?;
        log_trace!(
            "Canonicalized path {} -> {}",
            expanded_path.display(),
            canonicalized_path.display()
        );

        Ok(canonicalized_path)
    }
}

fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
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

    let mut argv: Vec<Cow<CStr>> = Vec::new();
    argv.push(Cow::Borrowed(c"bwrap"));
    argv.push(Cow::Borrowed(c"--unshare-all"));

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
        let expanded = resolve_path(&mount, config_path.as_ref()).map_err(|e| {
            anyhow::anyhow!(
                "Failed to resolve mount path {}: {}",
                mount.display(),
                e
            )
        })?;
        log_debug!("Mounting {} as read-write", expanded.display());

        if !expanded.exists() {
            eprintln!(
                "Warning: Mount path {} does not exist, skipping",
                expanded.display()
            );
            continue;
        }

        argv.push(Cow::Borrowed(c"--bind"));
        let path = CString::new(expanded.into_os_string().into_encoded_bytes())
            .expect("Failed to convert mount path to CString");
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
                match std::env::var(&env.key).ok() {
                    Some(value) => {
                        log_trace!("Inheriting environment variable {}={}", env.key, value);

                        argv.push(Cow::Borrowed(c"--setenv"));
                        argv.push(Cow::Owned(CString::new(env.key).unwrap()));
                        argv.push(Cow::Owned(CString::new(value).unwrap()));
                    }
                    None => {
                        log_trace!(
                            "Environment variable {} is not set in host environment, unsetting for tool",
                            env.key
                        );

                        // If the variable is not set in the host environment, it will be unset for the tool.
                        argv.push(Cow::Borrowed(c"--unsetenv"));
                        argv.push(Cow::Owned(CString::new(env.key).unwrap()));
                    }
                }
            }
            EnvironmentVariableValue::Literal(value) => {
                log_trace!("Setting environment variable {}={}", env.key, value);

                argv.push(Cow::Borrowed(c"--setenv"));
                argv.push(Cow::Owned(CString::new(env.key).unwrap()));
                argv.push(Cow::Owned(CString::new(value).unwrap()));
            }
        }
    }

    argv.push(Cow::Borrowed(c"--"));
    argv.extend(
        args.command
            .iter()
            .map(|s| Cow::Owned(CString::new(s.as_str()).unwrap())),
    );

    let mut envp: Vec<CString> = Vec::new();
    for (key, value) in std::env::vars() {
        envp.push(CString::new(format!("{}={}", key, value)).unwrap());
    }

    log_trace!(
        "Executing bwrap with arguments: {:?}",
        argv.iter().map(|s| s.as_ref()).collect::<Vec<_>>()
    );
    log_trace!(
        "Executing bwrap with environment: {:?}",
        envp.iter().map(|s| s.as_c_str()).collect::<Vec<_>>()
    );

    exec_bwrap(argv.as_slice(), envp.as_slice())
        .map_err(|e| anyhow::anyhow!("Failed to execute bwrap: {}", e))
}
