//! Shared helpers for integration tests.

use std::io::Read as _;

pub fn guest_dir() -> std::path::PathBuf {
    std::env::var_os("KITSUNE_GUEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/ci-guest"))
}

pub fn require_kvm() {
    let path = std::path::Path::new("/dev/kvm");
    if !path.exists() {
        if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
            panic!("/dev/kvm is required but missing");
        }
        eprintln!("skipping: /dev/kvm not available");
        std::process::exit(0);
    }
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_err()
    {
        if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
            panic!("/dev/kvm exists but is not usable");
        }
        eprintln!("skipping: cannot open /dev/kvm");
        std::process::exit(0);
    }
}

pub fn kitsune_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_kitsune")
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_kitsune (run via cargo test)")
}

/// Run kitsune until `timeout` or all `markers` appear in the combined output.
pub fn run_until(args: &[&str], timeout: std::time::Duration, markers: &[&str]) -> String {
    let mut child = std::process::Command::new(kitsune_bin())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn kitsune");

    let start = std::time::Instant::now();
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            break;
        }
        match stdout.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
        match stderr.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => err.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                let _ = stdout.read_to_end(&mut out);
                let _ = stderr.read_to_end(&mut err);
                break;
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }

        let combined = String::from_utf8_lossy(&out);
        if !markers.is_empty() && markers.iter().all(|m| combined.contains(m)) {
            let _ = child.kill();
            let _ = stdout.read_to_end(&mut out);
            let _ = stderr.read_to_end(&mut err);
            break;
        }
    }
    let _ = child.wait();

    let mut s = String::from_utf8_lossy(&out).into_owned();
    s.push_str(&String::from_utf8_lossy(&err));
    s
}

pub fn assert_contains(hay: &str, needle: &str) {
    assert!(
        hay.contains(needle),
        "expected output to contain {needle:?}\n--- output ---\n{hay}\n--- end ---"
    );
}
