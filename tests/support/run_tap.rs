//! Spawn kitsune with a TAP device inside a user network namespace.

#[path = "pipe.rs"]
mod pipe;

pub use pipe::assert_contains;

/// Like `run::run_until`, but creates a TAP under `unshare --user --net --map-root-user`.
pub fn run_until_with_tap(args: &[&str], timeout: std::time::Duration, markers: &[&str]) -> String {
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
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
                panic!("failed to spawn unshare for TAP e2e: {e}");
            }
            eprintln!("skipping: unshare not available ({e})");
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
