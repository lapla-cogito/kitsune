//! Shared helpers for integration tests.

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

fn drain_pipe(
    mut pipe: impl std::io::Read + Send + 'static,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });
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

    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let err = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    drain_pipe(
        child.stdout.take().expect("stdout"),
        std::sync::Arc::clone(&out),
    );
    drain_pipe(
        child.stderr.take().expect("stderr"),
        std::sync::Arc::clone(&err),
    );

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            break;
        }

        let combined = {
            let o = out.lock().unwrap_or_else(|e| e.into_inner());
            let e = err.lock().unwrap_or_else(|e| e.into_inner());
            let mut s = String::from_utf8_lossy(&o).into_owned();
            s.push_str(&String::from_utf8_lossy(&e));
            s
        };
        if !markers.is_empty() && markers.iter().all(|m| combined.contains(m)) {
            let _ = child.kill();
            break;
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.wait();
    // Allow drain threads to finish after EOF.
    std::thread::sleep(std::time::Duration::from_millis(100));

    let o = out.lock().unwrap_or_else(|e| e.into_inner());
    let e = err.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = String::from_utf8_lossy(&o).into_owned();
    s.push_str(&String::from_utf8_lossy(&e));
    s
}

pub fn assert_contains(hay: &str, needle: &str) {
    assert!(
        hay.contains(needle),
        "expected output to contain {needle:?}\n--- output ---\n{hay}\n--- end ---"
    );
}
