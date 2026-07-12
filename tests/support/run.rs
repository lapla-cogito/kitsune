//! Spawn kitsune and wait for serial markers.

#[path = "pipe.rs"]
mod pipe;

pub use pipe::assert_contains;

/// Run kitsune until `timeout` or all `markers` appear in the combined output.
pub fn run_until(args: &[&str], timeout: std::time::Duration, markers: &[&str]) -> String {
    let mut child = std::process::Command::new(pipe::kitsune_bin())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn kitsune");

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
