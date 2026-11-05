//! Spawn kitsune and wait for serial markers.

#[path = "pipe.rs"]
mod pipe;

pub use pipe::assert_contains;

/// Run kitsune until `timeout`, all `markers` appear, or the process exits.
///
/// Returns the captured output and the child's exit code. The code is `None`
/// when the child is killed after the markers match or the timeout fires. An
/// empty `markers` slice waits for the process to exit and keeps that status.
pub fn run_until(
    args: &[&str],
    timeout: std::time::Duration,
    markers: &[&str],
) -> (String, Option<i32>) {
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
