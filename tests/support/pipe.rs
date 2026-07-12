//! Background drain of child stdout/stderr for timeout-aware waits.

pub fn drain_pipe(
    mut pipe: impl std::io::Read + Send + 'static,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut pipe, &mut buf) {
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

pub fn wait_child(
    mut child: std::process::Child,
    out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    err: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    timeout: std::time::Duration,
    markers: &[&str],
) -> String {
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
    std::thread::sleep(std::time::Duration::from_millis(100));

    let o = out.lock().unwrap_or_else(|e| e.into_inner());
    let e = err.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = String::from_utf8_lossy(&o).into_owned();
    s.push_str(&String::from_utf8_lossy(&e));
    s
}

pub fn kitsune_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_kitsune")
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_kitsune (run via cargo test)")
}

pub fn assert_contains(hay: &str, needle: &str) {
    assert!(
        hay.contains(needle),
        "expected output to contain {needle:?}\n--- output ---\n{hay}\n--- end ---"
    );
}
