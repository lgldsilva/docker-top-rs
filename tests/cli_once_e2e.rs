use std::fs;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn once_discovers_and_collects_a_cli_context_end_to_end() {
    let home = std::env::temp_dir().join(format!("docker-top-cli-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let docker_bin = home.join("bin");
    let meta_dir = home.join(".docker/contexts/meta/default-hash");
    fs::create_dir_all(&docker_bin).unwrap();
    fs::create_dir_all(&meta_dir).unwrap();

    let docker = docker_bin.join("docker");
    fs::write(
        &docker,
        r##"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'Docker fake for integration test'
  exit 0
fi
if [ "$1" = "context" ] && [ "$2" = "ls" ]; then
  printf 'default\n'
  exit 0
fi
if [ "$1" = "--context" ] && [ "$2" = "default" ]; then
  shift 2
fi
case "$1" in
  stats)
    printf 'abcdef123456\tweb\t5.0%%\t100MiB / 1GiB\t9.7%%\t1MiB / 2MiB\t3MiB / 4MiB\t3\n'
    ;;
  info)
    printf '4\t8589934592\n'
    ;;
  inspect)
    printf 'abcdef1234567890\t0\t0\t0\t\t536870912\n'
    ;;
  *)
    echo "unexpected docker arguments: $*" >&2
    exit 2
    ;;
esac
"##,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        meta_dir.join("meta.json"),
        r#"{"Name":"default","Endpoints":{"docker":{"Host":"ssh://test-engine"}}}"#,
    )
    .unwrap();

    let config = home.join("config.toml");
    fs::write(&config, "").unwrap();
    let coverage_profile = std::env::var_os("LLVM_PROFILE_FILE");
    let mut command = Command::new(env!("CARGO_BIN_EXE_docker-top-rs"));
    command
        .args([
            "--once",
            "--context",
            "default",
            "--delay",
            "12",
            "--config",
            config.to_str().unwrap(),
        ])
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", docker_bin.display()));
    // Keep subprocess coverage active while still isolating the fake Docker environment.
    if let Some(profile) = coverage_profile.clone() {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let output = command.output().expect("launch docker-top-rs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("default"), "stdout: {stdout}");
    assert!(stdout.contains("web"), "stdout: {stdout}");
    assert!(stdout.contains("abcdef123456"), "stdout: {stdout}");
    assert!(!stdout.contains("Erro no contexto"), "stdout: {stdout}");
    assert!(
        stderr.contains("context=default") && stderr.contains("via=cli"),
        "stderr: {stderr}"
    );
    assert!(stdout.contains("Refresh: 10.0s"), "stdout: {stdout}");

    fs::write(&config, "context = ''\n").unwrap();
    let mut all_contexts = Command::new(env!("CARGO_BIN_EXE_docker-top-rs"));
    all_contexts
        .args(["--once", "--config", config.to_str().unwrap()])
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", docker_bin.display()));
    if let Some(profile) = coverage_profile {
        all_contexts.env("LLVM_PROFILE_FILE", profile);
    }
    let all_output = all_contexts.output().expect("launch all-context run");
    let all_stdout = String::from_utf8_lossy(&all_output.stdout);
    let all_stderr = String::from_utf8_lossy(&all_output.stderr);
    assert!(all_output.status.success(), "stderr: {all_stderr}");
    assert!(all_stdout.contains("Ctx: all"), "stdout: {all_stdout}");
    assert!(all_stdout.contains("web"), "stdout: {all_stdout}");

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn config_example_and_invalid_config_cli_paths_are_end_to_end() {
    let home = std::env::temp_dir().join(format!("docker-top-config-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    let profile = std::env::var_os("LLVM_PROFILE_FILE");

    let mut generate = Command::new(env!("CARGO_BIN_EXE_docker-top-rs"));
    generate
        .arg("--generate-config-example")
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin");
    if let Some(profile) = profile.clone() {
        generate.env("LLVM_PROFILE_FILE", profile);
    }
    let generated = generate.output().expect("generate example config");
    assert!(generated.status.success());
    let example = fs::read_to_string(home.join("config.example.toml")).unwrap();
    assert!(example.contains("refresh_delay"));

    let invalid = home.join("invalid.toml");
    fs::write(&invalid, "unknown_field = true\n").unwrap();
    let mut invalid_run = Command::new(env!("CARGO_BIN_EXE_docker-top-rs"));
    invalid_run
        .args(["--once", "--config"])
        .arg(&invalid)
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin");
    if let Some(profile) = profile {
        invalid_run.env("LLVM_PROFILE_FILE", profile);
    }
    let rejected = invalid_run.output().expect("reject invalid config");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success());
    assert!(stderr.contains("config inválida"), "stderr: {stderr}");
    let _ = fs::remove_dir_all(&home);
}

/// Drain a PTY stream into a shared buffer so the pipe never blocks the
/// child and the buffer can be polled for readiness.
fn drain_into<R: Read + Send + 'static>(
    mut pipe: R,
    buf: Arc<Mutex<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf
                    .lock()
                    .expect("lock PTY buffer")
                    .push_str(&String::from_utf8_lossy(&chunk[..n])),
            }
        }
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn tui_starts_and_quits_under_a_pseudo_terminal() {
    let home = std::env::temp_dir().join(format!("docker-top-tui-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let docker_bin = home.join("bin");
    let meta_dir = home.join(".docker/contexts/meta/default-hash");
    fs::create_dir_all(&docker_bin).unwrap();
    fs::create_dir_all(&meta_dir).unwrap();

    let docker = docker_bin.join("docker");
    fs::write(
        &docker,
        r##"#!/bin/sh
if [ "$1" = "--version" ]; then exit 0; fi
if [ "$1" = "context" ] && [ "$2" = "ls" ]; then printf 'default\n'; exit 0; fi
if [ "$1" = "--context" ] && [ "$2" = "default" ]; then shift 2; fi
case "$1" in
  stats) printf 'abcdef123456\tweb\t5.0%%\t100MiB / 1GiB\t9.7%%\t1MiB / 2MiB\t3MiB / 4MiB\t3\n' ;;
  info) printf '4\t8589934592\n' ;;
  inspect) printf 'abcdef1234567890\t0\t0\t0\t\t536870912\n' ;;
  *) echo "unexpected docker arguments: $*" >&2; exit 2 ;;
esac
"##,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        meta_dir.join("meta.json"),
        r#"{"Name":"default","Endpoints":{"docker":{"Host":"ssh://test-engine"}}}"#,
    )
    .unwrap();
    let config = home.join("config.toml");
    fs::write(&config, "").unwrap();

    let binary = env!("CARGO_BIN_EXE_docker-top-rs");
    let mut command = Command::new("script");
    #[cfg(target_os = "linux")]
    {
        let escaped = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let shell_command = format!(
            "{} --context default --config {}",
            escaped(binary),
            escaped(&config.to_string_lossy())
        );
        command.args(["-qec", &shell_command, "/dev/null"]);
    }
    #[cfg(target_os = "macos")]
    {
        command
            .args([
                "-q",
                "/dev/null",
                binary,
                "--context",
                "default",
                "--config",
            ])
            .arg(&config);
    }
    let profile = std::env::var_os("LLVM_PROFILE_FILE");
    command
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", docker_bin.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(profile) = profile {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let mut child = command.spawn().expect("start script with a PTY");
    let mut input = child.stdin.take().expect("script stdin");
    let stdout_pipe = child.stdout.take().expect("script stdout");
    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stdout_thread = drain_into(stdout_pipe, Arc::clone(&stdout_buf));

    // TUI is up once it enters the alternate screen; poll instead of a fixed
    // sleep so slow starts wait and broken starts fail fast (10s deadline).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if stdout_buf
            .lock()
            .expect("lock PTY buffer")
            .contains("\u{1b}[?1049h")
        {
            break;
        }
        if child.try_wait().expect("poll PTY process").is_some() {
            panic!("docker-top-rs exited before the TUI was ready");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("docker-top-rs TUI did not start within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    input.write_all(b"q").expect("send quit key");
    drop(input);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("poll PTY process").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("docker-top-rs TUI did not quit after q");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().expect("collect PTY output");
    stdout_thread.join().expect("join stdout drain");
    let stdout = stdout_buf.lock().expect("lock PTY buffer").clone();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains("docker-top-rs finalizado."),
        "stdout: {stdout}"
    );
    let combined_output = format!("{stdout}{stderr}");
    assert!(
        combined_output.contains("context=default") && combined_output.contains("via=cli"),
        "combined PTY output: {combined_output}"
    );
    let _ = fs::remove_dir_all(&home);
}
