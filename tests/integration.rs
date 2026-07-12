use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::fd::RawFd;
use std::os::unix::{ffi::OsStrExt as _, ffi::OsStringExt as _, fs::symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

const HELPER_ENV: &str = "AGENT_RUN_TEST_HELPER";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

static TEMP_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new() -> Self {
        let parent = std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let counter = TEMP_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path = parent.join(format!(
            "agent-run-test-{}-{timestamp}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temporary directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn resolve_data_path(variable: &str, cargo_path: Option<&str>, relative: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(variable) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return path;
        }
        return std::env::current_dir()
            .expect("failed to get current directory")
            .join(path);
    }

    if let Some(path) = cargo_path {
        return PathBuf::from(path);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn agent_run_binary() -> PathBuf {
    resolve_data_path(
        "AGENT_RUN_BINARY",
        option_env!("CARGO_BIN_EXE_agent-run"),
        "target/debug/agent-run",
    )
}

fn schema_path() -> PathBuf {
    resolve_data_path("AGENT_RUN_SCHEMA", None, "schema.json")
}

fn cargo_toml_path() -> PathBuf {
    resolve_data_path("AGENT_RUN_CARGO_TOML", None, "Cargo.toml")
}

fn workspace_status_path() -> PathBuf {
    resolve_data_path(
        "AGENT_RUN_WORKSPACE_STATUS",
        None,
        "tools/workspace-status.sh",
    )
}

fn write_config(directory: &Path, contents: &str) -> PathBuf {
    let path = directory.join("config.toml");
    fs::write(&path, contents).expect("failed to write configuration");
    path
}

fn toml_string(value: &Path) -> String {
    let value = value
        .to_str()
        .expect("test paths must be valid UTF-8")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{value}\"")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_command(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let description = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {description}: {error}"));
    let deadline = Instant::now() + COMMAND_TIMEOUT;

    loop {
        if child
            .try_wait()
            .unwrap_or_else(|error| panic!("failed to wait for {description}: {error}"))
            .is_some()
        {
            return child.wait_with_output().unwrap_or_else(|error| {
                panic!("failed to collect output from {description}: {error}")
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("failed to collect timed-out output: {error}"));
            panic!("command timed out: {description}\n{}", output_text(&output));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
}

fn assert_normal_error(output: &Output) {
    assert!(!output.status.success(), "command unexpectedly succeeded");
    assert!(
        !output_text(output).contains("panicked at"),
        "{}",
        output_text(output)
    );
}

fn helper_command(operation: &str, directory: &Path, config: Option<&Path>) -> Command {
    helper_command_with_executable(
        operation,
        directory,
        config,
        &std::env::current_exe().unwrap(),
    )
}

fn helper_command_with_executable(
    operation: &str,
    directory: &Path,
    config: Option<&Path>,
    executable: &Path,
) -> Command {
    let mut command = Command::new(agent_run_binary());
    command.current_dir(directory).env_remove("RUST_LOG");
    command.env(HELPER_ENV, operation);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command
        .arg("--")
        .arg(executable)
        .args(["--exact", "agent_run_test_helper", "--nocapture"]);
    command
}

fn run_helper(operation: &str, directory: &Path, config: Option<&Path>) -> Output {
    run_command(helper_command(operation, directory, config))
}

fn package_version() -> String {
    let cargo_toml = fs::read_to_string(cargo_toml_path()).expect("failed to read Cargo.toml");
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line == "[package]" {
            in_package = true;
        } else if line.starts_with('[') {
            in_package = false;
        } else if in_package
            && let Some(value) = line.strip_prefix("version = \"")
            && let Some(value) = value.strip_suffix('"')
        {
            return value.to_owned();
        }
    }
    panic!("no package version found in Cargo.toml");
}

fn marker_value<'a>(text: &'a str, marker: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(marker))
        .unwrap_or_else(|| panic!("missing marker {marker:?} in:\n{text}"))
}

#[test]
fn version_and_help() {
    let expected = format!("agent-run {}\n", package_version());
    for flag in ["--version", "-V"] {
        let mut command = Command::new(agent_run_binary());
        command.arg(flag);
        let output = run_command(command);
        assert_success(&output);
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }

    let mut command = Command::new(agent_run_binary());
    command.arg("--help");
    let output = run_command(command);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("-V, --version"));

    let cargo_toml = cargo_toml_path();
    let mut command = Command::new(workspace_status_path());
    command.current_dir(cargo_toml.parent().unwrap());
    let output = run_command(command);
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("STABLE_AGENT_RUN_VERSION {}\n", package_version())
    );
}

#[test]
fn no_config_and_default_configs_work() {
    let temp = TempDirectory::new();

    let output = run_helper("noop", temp.path(), None);
    assert_success(&output);

    for contents in ["", "[global]\nnetwork = true\ninherit_env = true\n"] {
        let config = write_config(temp.path(), contents);
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_success(&output);
    }
}

#[test]
fn nearest_config_is_discovered() {
    let temp = TempDirectory::new();
    let config_directory = temp.path().join(".agent-run");
    fs::create_dir(&config_directory).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        "[global]\ninherit_env = false\n\
         env = [\"FOUND=nearest\", \"AGENT_RUN_TEST_HELPER\"]\n",
    )
    .unwrap();
    let nested = temp.path().join("one/two");
    fs::create_dir_all(&nested).unwrap();

    let output = run_helper("print_found", &nested, None);
    assert_success(&output);
    assert!(output_text(&output).contains("FOUND=nearest"));
}

#[test]
fn unknown_fields_are_rejected() {
    let temp = TempDirectory::new();
    for contents in [
        "unknown = true\n",
        "[global]\nnetwrok = false\n",
        "[tools.example]\nunknown = true\n",
    ] {
        let config = write_config(temp.path(), contents);
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_normal_error(&output);
        assert!(output_text(&output).contains("unknown field"));
    }
}

#[test]
fn empty_environment_names_are_rejected() {
    let temp = TempDirectory::new();
    for entry in ["", "=value"] {
        let config = write_config(temp.path(), &format!("[global]\nenv = [\"{entry}\"]\n"));
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_normal_error(&output);
        let text = output_text(&output);
        assert!(text.contains("Environment variable key cannot be empty"));
        assert!(!text.contains("bwrap: unsetenv failed"));
    }
}

#[test]
fn duplicate_environment_entries_are_rejected() {
    let temp = TempDirectory::new();
    for contents in [
        "[global]\nenv = [\"X=one\", \"X=two\"]\n",
        "[tools.example]\nenv = [\"X=one\", \"X=two\"]\n",
    ] {
        let config = write_config(temp.path(), contents);
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_normal_error(&output);
        assert!(output_text(&output).contains("Duplicate environment variable entries"));
    }
}

#[test]
fn tool_environment_overrides_global_environment() {
    let temp = TempDirectory::new();
    let executable = temp.path().join("override-tool");
    symlink(std::env::current_exe().unwrap(), &executable).unwrap();
    let config = write_config(
        temp.path(),
        "[global]\ninherit_env = false\nenv = [\"X=global\", \
         \"AGENT_RUN_TEST_HELPER\"]\n\n[tools.override-tool]\nenv = [\"X=tool\"]\n",
    );

    let output = run_command(helper_command_with_executable(
        "print_x",
        temp.path(),
        Some(&config),
        &executable,
    ));
    assert_success(&output);
    assert!(output_text(&output).contains("X=tool"));
}

#[test]
fn environment_values_may_contain_equals() {
    let temp = TempDirectory::new();
    let config = write_config(
        temp.path(),
        "[global]\ninherit_env = false\nenv = [\"FOO=a=b\", \
         \"AGENT_RUN_TEST_HELPER\"]\n",
    );
    let output = run_helper("print_foo", temp.path(), Some(&config));
    assert_success(&output);
    assert!(output_text(&output).contains("FOO=a=b"));

    let schema = fs::read_to_string(schema_path()).expect("failed to read schema.json");
    assert!(schema.contains(r#""pattern": "^[^=\\u0000]+(=[^\\u0000]*)?$""#));
}

#[test]
fn missing_mounts_are_warned_about_and_skipped() {
    let temp = TempDirectory::new();
    for mount in [
        "missing-relative".to_owned(),
        temp.path().join("missing-absolute").display().to_string(),
    ] {
        let mount = Path::new(&mount);
        let config = write_config(
            temp.path(),
            &format!("[global]\nmount = [{}]\n", toml_string(mount)),
        );
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_success(&output);
        assert!(
            output
                .stderr
                .windows(b"does not exist, skipping".len())
                .any(|window| { window == b"does not exist, skipping" })
        );
        assert!(!output_text(&output).contains("Can't find source path"));
    }
}

#[test]
fn home_tilde_mount_is_expanded() {
    let temp = TempDirectory::new();
    let writable = temp.path().join("home/writable");
    fs::create_dir_all(&writable).unwrap();
    let config = write_config(temp.path(), "[global]\nmount = [\"~/writable\"]\n");
    let mut command = helper_command("write_tilde_mount", temp.path(), Some(&config));
    command.env("HOME", temp.path().join("home"));
    let output = run_command(command);
    assert_success(&output);
    assert_eq!(fs::read_to_string(writable.join("created")).unwrap(), "ok");
}

#[test]
fn child_exit_status_is_propagated() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");

    let output = run_helper("noop", temp.path(), Some(&config));
    assert_success(&output);

    let output = run_helper("exit_42", temp.path(), Some(&config));
    assert_eq!(output.status.code(), Some(42), "{}", output_text(&output));
}

#[test]
fn nul_in_environment_is_a_normal_error() {
    let temp = TempDirectory::new();
    for contents in [
        "[global]\nenv = [\"X=\\u0000\"]\n",
        "[global]\nenv = [\"\\u0000=value\"]\n",
    ] {
        let config = write_config(temp.path(), contents);
        let output = run_helper("noop", temp.path(), Some(&config));
        assert_normal_error(&output);
        assert!(output_text(&output).contains("NUL byte"));
    }
}

#[test]
fn nul_in_mount_is_a_normal_error() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "[global]\nmount = [\"/tmp/\\u0000\"]\n");
    let output = run_helper("noop", temp.path(), Some(&config));
    assert_normal_error(&output);
    assert!(output_text(&output).contains("NUL byte"));
}

#[test]
fn non_utf8_inherited_environment_is_preserved() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");
    let mut command = helper_command("print_non_utf8", temp.path(), Some(&config));
    command.env("NON_UTF8_TEST", OsString::from_vec(b"\xffvalue".to_vec()));
    let output = run_command(command);
    assert_success(&output);
    assert!(output_text(&output).contains("NON_UTF8_TEST=ff76616c7565"));
}

#[test]
fn non_utf8_selected_environment_is_preserved() {
    let temp = TempDirectory::new();
    let config = write_config(
        temp.path(),
        "[global]\ninherit_env = false\nenv = [\"NON_UTF8_TEST\", \
         \"AGENT_RUN_TEST_HELPER\"]\n",
    );
    let mut command = helper_command("print_non_utf8", temp.path(), Some(&config));
    command.env(
        "NON_UTF8_TEST",
        OsString::from_vec(b"\xffselected".to_vec()),
    );
    let output = run_command(command);
    assert_success(&output);
    assert!(output_text(&output).contains("NON_UTF8_TEST=ff73656c6563746564"));
}

#[test]
fn mount_and_environment_isolation() {
    let temp = TempDirectory::new();
    fs::create_dir(temp.path().join("project")).unwrap();
    fs::create_dir(temp.path().join("sibling")).unwrap();
    let config = write_config(
        temp.path(),
        "[global]\nmount = [\"project\"]\ninherit_env = false\nenv = [\
         \"PASSED=literal\", \"INHERITED\", \"UNSET\", \
         \"AGENT_RUN_TEST_HELPER\"]\n",
    );
    let mut command = helper_command("mount_and_environment", temp.path(), Some(&config));
    command.env("INHERITED", "host").env_remove("UNSET");
    let output = run_command(command);
    assert_success(&output);
    let text = output_text(&output);
    assert!(text.contains("PASSED=literal"));
    assert!(text.contains("INHERITED=host"));
    assert!(text.contains("UNSET_PRESENT=false"));
    assert!(text.contains("HOME_PRESENT=false"));
    assert!(text.contains("SIBLING_WRITE=blocked"));
    assert_eq!(
        fs::read_to_string(temp.path().join("project/ok")).unwrap(),
        "ok"
    );
    assert!(!temp.path().join("sibling/bad").exists());
}

#[test]
fn network_setting_controls_network_namespace() {
    let parent_namespace = fs::read_link("/proc/self/ns/net")
        .unwrap()
        .display()
        .to_string();
    let temp = TempDirectory::new();
    let mut namespaces = Vec::new();
    for enabled in [true, false] {
        let config = write_config(temp.path(), &format!("[global]\nnetwork = {enabled}\n"));
        let output = run_helper("print_network_namespace", temp.path(), Some(&config));
        assert_success(&output);
        let text = output_text(&output);
        namespaces.push(marker_value(&text, "NETWORK_NAMESPACE=").to_owned());
    }

    assert_eq!(namespaces[0], parent_namespace);
    assert_ne!(namespaces[1], parent_namespace);
}

extern "C" fn signal_helper_handler(_signal: libc::c_int) {
    const GOT_SIGNAL: &[u8] = b"GOT_SIGNAL\n";
    // SAFETY: `write` and `_exit` are async-signal-safe, and the byte slice is static.
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            GOT_SIGNAL.as_ptr().cast(),
            GOT_SIGNAL.len(),
        );
        libc::_exit(0);
    }
}

fn install_signal_handler(signal: libc::c_int) {
    // SAFETY: The action is fully initialized before it is passed to sigaction.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = signal_helper_handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
    }
}

fn wait_for_signal() -> ! {
    println!("READY");
    std::io::stdout().flush().unwrap();
    loop {
        // SAFETY: pause simply waits for a signal.
        unsafe { libc::pause() };
    }
}

fn run_test_helper(operation: &str) {
    match operation {
        "noop" => {}
        "print_found" => println!("FOUND={}", std::env::var("FOUND").unwrap()),
        "print_x" => println!("X={}", std::env::var("X").unwrap()),
        "print_foo" => println!("FOO={}", std::env::var("FOO").unwrap()),
        "write_tilde_mount" => fs::write("home/writable/created", "ok").unwrap(),
        "exit_42" => std::process::exit(42),
        "print_non_utf8" => {
            let value = std::env::var_os("NON_UTF8_TEST").unwrap().into_vec();
            print!("NON_UTF8_TEST=");
            for byte in value {
                print!("{byte:02x}");
            }
            println!();
        }
        "mount_and_environment" => {
            fs::write("project/ok", "ok").unwrap();
            match fs::write("sibling/bad", "bad") {
                Ok(()) => panic!("write to read-only sibling unexpectedly succeeded"),
                Err(error) => {
                    assert!(matches!(
                        error.raw_os_error(),
                        Some(libc::EROFS | libc::EACCES | libc::EPERM)
                    ));
                    println!("SIBLING_WRITE=blocked");
                }
            }
            println!("PASSED={}", std::env::var("PASSED").unwrap());
            println!("INHERITED={}", std::env::var("INHERITED").unwrap());
            println!("UNSET_PRESENT={}", std::env::var_os("UNSET").is_some());
            println!("HOME_PRESENT={}", std::env::var_os("HOME").is_some());
        }
        "print_network_namespace" => println!(
            "NETWORK_NAMESPACE={}",
            fs::read_link("/proc/self/ns/net").unwrap().display()
        ),
        "terminal_ioctls" => {
            let tty = c"/dev/tty";
            // SAFETY: The path is NUL-terminated and the returned fd is checked.
            let fd = unsafe { libc::open(tty.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            assert!(
                fd >= 0,
                "failed to open /dev/tty: {}",
                std::io::Error::last_os_error()
            );

            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: `size` is a valid output buffer for TIOCGWINSZ.
            let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
            assert_eq!(
                result,
                0,
                "TIOCGWINSZ failed: {}",
                std::io::Error::last_os_error()
            );

            let byte = b'x';
            // SAFETY: `byte` is a valid input buffer for TIOCSTI.
            let result = unsafe { libc::ioctl(fd, libc::TIOCSTI, &byte) };
            assert_eq!(result, -1, "TIOCSTI unexpectedly succeeded");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            // SAFETY: `fd` was returned by open and has not been closed yet.
            unsafe { libc::close(fd) };
            println!("TIOCSTI_EPERM");
            println!("TTY_OK");
        }
        "wait_forever" => wait_for_signal(),
        "wait_sigwinch" => {
            install_signal_handler(libc::SIGWINCH);
            wait_for_signal();
        }
        "wait_sigint" => {
            install_signal_handler(libc::SIGINT);
            wait_for_signal();
        }
        other => panic!("unknown test helper operation: {other}"),
    }
}

#[test]
fn agent_run_test_helper() {
    let Some(operation) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    run_test_helper(operation.to_str().expect("helper operation must be UTF-8"));
}

#[derive(Clone, Copy)]
enum SigintDisposition {
    Default,
    Ignore,
}

struct PtyProcess {
    pid: libc::pid_t,
    master: RawFd,
    status: Option<libc::c_int>,
    output: Vec<u8>,
}

impl PtyProcess {
    fn spawn(config: &Path, operation: &str, directory: &Path) -> Self {
        Self::spawn_with_sigint_disposition(
            config,
            operation,
            directory,
            SigintDisposition::Default,
        )
    }

    fn spawn_ignoring_sigint(config: &Path, operation: &str, directory: &Path) -> Self {
        Self::spawn_with_sigint_disposition(config, operation, directory, SigintDisposition::Ignore)
    }

    fn spawn_with_sigint_disposition(
        config: &Path,
        operation: &str,
        directory: &Path,
        sigint_disposition: SigintDisposition,
    ) -> Self {
        let executable = std::env::current_exe().unwrap();
        let arguments = [
            agent_run_binary().into_os_string(),
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--"),
            executable.into_os_string(),
            OsString::from("--exact"),
            OsString::from("agent_run_test_helper"),
            OsString::from("--nocapture"),
        ];
        let argument_strings: Vec<CString> = arguments
            .iter()
            .map(|argument| CString::new(argument.as_bytes()).unwrap())
            .collect();
        let mut argument_pointers: Vec<*const libc::c_char> = argument_strings
            .iter()
            .map(|argument| argument.as_ptr())
            .collect();
        argument_pointers.push(std::ptr::null());

        let mut environment_strings = Vec::new();
        for (key, value) in std::env::vars_os() {
            if key == OsStr::new(HELPER_ENV) || key == OsStr::new("RUST_LOG") {
                continue;
            }
            let mut entry = key.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            environment_strings.push(CString::new(entry).unwrap());
        }
        environment_strings.push(CString::new(format!("{HELPER_ENV}={operation}")).unwrap());
        let mut environment_pointers: Vec<*const libc::c_char> = environment_strings
            .iter()
            .map(|entry| entry.as_ptr())
            .collect();
        environment_pointers.push(std::ptr::null());
        let directory = CString::new(directory.as_os_str().as_bytes()).unwrap();

        let mut master = -1;
        // SAFETY: All arguments are valid. The child calls only async-signal-safe functions before
        // execve, and all pointer backing storage remains alive across the call.
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert!(
            pid >= 0,
            "forkpty failed: {}",
            std::io::Error::last_os_error()
        );
        if pid == 0 {
            // SAFETY: These calls are async-signal-safe and all pointers are valid in the child.
            unsafe {
                let mut sigint_action: libc::sigaction = std::mem::zeroed();
                sigint_action.sa_sigaction = match sigint_disposition {
                    SigintDisposition::Default => libc::SIG_DFL,
                    SigintDisposition::Ignore => libc::SIG_IGN,
                };
                libc::sigemptyset(&mut sigint_action.sa_mask);
                if libc::sigaction(libc::SIGINT, &sigint_action, std::ptr::null_mut()) != 0 {
                    libc::_exit(125);
                }

                let mut sigint_mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut sigint_mask);
                libc::sigaddset(&mut sigint_mask, libc::SIGINT);
                if libc::sigprocmask(libc::SIG_UNBLOCK, &sigint_mask, std::ptr::null_mut()) != 0 {
                    libc::_exit(125);
                }

                if libc::chdir(directory.as_ptr()) != 0 {
                    libc::_exit(126);
                }
                libc::execve(
                    argument_pointers[0],
                    argument_pointers.as_ptr(),
                    environment_pointers.as_ptr(),
                );
                libc::_exit(127);
            }
        }

        Self {
            pid,
            master,
            status: None,
            output: Vec::new(),
        }
    }

    fn poll_status(&mut self) -> Option<libc::c_int> {
        if self.status.is_none() {
            let mut status = 0;
            // SAFETY: `status` is a valid output pointer and `pid` is our child.
            let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if result == self.pid {
                self.status = Some(status);
            } else if result < 0 {
                panic!("waitpid failed: {}", std::io::Error::last_os_error());
            }
        }
        self.status
    }

    fn read_once(&mut self, timeout: Duration) -> bool {
        let timeout_millis = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd: self.master,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: `poll_fd` points to one initialized pollfd.
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_millis) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return false;
            }
            panic!("poll failed: {error}");
        }
        if result == 0 {
            return false;
        }

        let mut buffer = [0_u8; 4096];
        // SAFETY: `buffer` is writable for its full length and `master` is an open fd.
        let count = unsafe { libc::read(self.master, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EIO | libc::EAGAIN)) {
                return false;
            }
            panic!("failed to read PTY: {error}");
        }
        if count == 0 {
            return false;
        }
        self.output.extend_from_slice(&buffer[..count as usize]);
        true
    }

    fn read_until(&mut self, marker: &[u8]) {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while !self
            .output
            .windows(marker.len())
            .any(|window| window == marker)
        {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {:?}; output: {}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&self.output)
            );
            let read = self.read_once(Duration::from_millis(50));
            assert!(
                self.poll_status().is_none() || read,
                "process exited before {:?}; output: {}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&self.output)
            );
        }
    }

    fn wait(&mut self) -> (libc::c_int, Vec<u8>) {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while self.poll_status().is_none() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for PTY process; output: {}",
                String::from_utf8_lossy(&self.output)
            );
            self.read_once(Duration::from_millis(50));
        }
        while self.read_once(Duration::from_millis(10)) {}
        (self.status.unwrap(), self.output.clone())
    }

    fn write(&mut self, bytes: &[u8]) {
        // SAFETY: `bytes` is readable for its full length and `master` is an open fd.
        let count = unsafe { libc::write(self.master, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(count, bytes.len() as isize);
    }

    fn set_window_size(&mut self, rows: u16, columns: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `size` is a valid input buffer for TIOCSWINSZ.
        let result = unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &size) };
        assert_eq!(
            result,
            0,
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.status.is_none() {
            // The forkpty child is a process-group leader. Kill the whole group on failure.
            // SAFETY: A negative pid addresses the process group created by forkpty.
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
                libc::waitpid(self.pid, std::ptr::null_mut(), 0);
            }
        }
        // SAFETY: `master` is owned by this object and is closed exactly once.
        unsafe { libc::close(self.master) };
    }
}

fn assert_wait_status_success(status: libc::c_int, output: &[u8]) {
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "wait status {status}; output: {}",
        String::from_utf8_lossy(output)
    );
}

#[test]
fn terminal_tiocsti_is_blocked_but_terminal_remains_attached() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");
    let mut child = PtyProcess::spawn(&config, "terminal_ioctls", temp.path());
    let (status, output) = child.wait();
    assert_wait_status_success(status, &output);
    assert!(
        output
            .windows(b"TIOCSTI_EPERM".len())
            .any(|window| window == b"TIOCSTI_EPERM")
    );
    assert!(
        output
            .windows(b"TTY_OK".len())
            .any(|window| window == b"TTY_OK")
    );
}

#[test]
fn terminal_sigwinch_reaches_the_command() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");
    let mut child = PtyProcess::spawn(&config, "wait_sigwinch", temp.path());
    child.read_until(b"READY");
    child.set_window_size(40, 100);
    child.read_until(b"GOT_SIGNAL");
    let (status, output) = child.wait();
    assert_wait_status_success(status, &output);
}

#[test]
fn terminal_ctrl_c_reaches_the_command() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");

    // If agent-run and bwrap also receive SIGINT, they can tear down the sandbox before the
    // command's signal handler is scheduled. Ignore SIGINT in those wrapper processes so this
    // test independently guarantees that the foreground command receives terminal-generated
    // SIGINT.
    let mut child = PtyProcess::spawn_ignoring_sigint(&config, "wait_sigint", temp.path());
    child.read_until(b"READY");
    child.write(b"\x03");
    child.read_until(b"GOT_SIGNAL");
    let (status, output) = child.wait();
    assert_wait_status_success(status, &output);
}

#[test]
fn terminal_ctrl_c_terminates_the_outer_process() {
    let temp = TempDirectory::new();
    let config = write_config(temp.path(), "");
    let mut child = PtyProcess::spawn(&config, "wait_forever", temp.path());
    child.read_until(b"READY");
    child.write(b"\x03");
    let (status, output) = child.wait();
    assert!(
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGINT,
        "wait status {status}; output: {}",
        String::from_utf8_lossy(&output)
    );
}
