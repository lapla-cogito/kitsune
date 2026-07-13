//! Spawn kitsune with a TAP device for virtio-net e2e.

#[path = "host_tcp.rs"]
mod host_tcp;
#[path = "pipe.rs"]
mod pipe;

pub use pipe::assert_contains;

static TAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run kitsune with TAP until timeout or all markers appear.
pub fn run_until_with_tap(args: &[&str], timeout: std::time::Duration, markers: &[&str]) -> String {
    let _guard = TAP_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    if let Ok(tap) = std::env::var("KITSUNE_TAP_NAME") {
        return run_with_existing_tap(&tap, args, timeout, markers);
    }
    run_with_unshare_tap(args, timeout, markers)
}

fn run_with_existing_tap(
    tap: &str,
    args: &[&str],
    timeout: std::time::Duration,
    markers: &[&str],
) -> String {
    // TAP IP is preconfigured on the host (CI); serve TCP for guest clients.
    let _tcp = match host_tcp::HostTcpService::start(host_tcp::DEFAULT_ADDR) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "warning: host TCP helper on {}: {e} (TCP e2e markers may fail)",
                host_tcp::DEFAULT_ADDR
            );
            None
        }
    };

    let mut owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    owned.push("--tap".to_string());
    owned.push(tap.to_string());
    let refs: Vec<&str> = owned.iter().map(std::string::String::as_str).collect();

    let mut cmd = std::process::Command::new(pipe::kitsune_bin());
    cmd.args(refs);
    spawn_and_wait(&mut cmd, timeout, markers)
}

fn run_with_unshare_tap(args: &[&str], timeout: std::time::Duration, markers: &[&str]) -> String {
    let bin = pipe::kitsune_bin();
    let wrapper = std::path::Path::new("scripts/ci_run_with_tap.sh");
    assert!(
        wrapper.is_file(),
        "missing {}; run tests from the repository root",
        wrapper.display()
    );

    let mut cmd = std::process::Command::new("unshare");
    cmd.args(["--user", "--net", "--map-root-user", "--"])
        .arg("bash")
        .arg(wrapper)
        .arg(&bin)
        .args(args);

    let out = spawn_and_wait(&mut cmd, timeout, markers);
    if out.contains("uid_map") && out.contains("Operation not permitted") {
        panic!(
            "unshare user namespaces are disabled here; pre-create a TAP and set \
             KITSUNE_TAP_NAME (see .github/workflows/ci.yml).\n--- output ---\n{out}"
        );
    }
    out
}

fn spawn_and_wait(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
    markers: &[&str],
) -> String {
    let mut child = match cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
                panic!("failed to spawn virtio-net e2e command: {e}");
            }
            eprintln!("skipping: cannot spawn virtio-net e2e ({e})");
            std::process::exit(0);
        }
    };

    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let err = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    pipe::drain_pipe(
        child.stdout.take().expect("stdout"),
        std::sync::Arc::clone(&out),
    );
    pipe::drain_pipe(
        child.stderr.take().expect("stderr"),
        std::sync::Arc::clone(&err),
    );
    pipe::wait_child(child, out, err, timeout, markers)
}
