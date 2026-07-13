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

/// Whether to print captured guest serial (CI, or `KITSUNE_E2E_LOG=1`).
pub fn e2e_log_enabled() -> bool {
    std::env::var_os("KITSUNE_E2E_LOG").is_some() || std::env::var_os("CI").is_some()
}

/// Print and optionally save combined guest serial / kitsune stderr.
pub fn dump_guest_serial(markers: &[&str], serial: &str) {
    if !e2e_log_enabled() {
        return;
    }
    let tag = if markers.is_empty() {
        "e2e".to_string()
    } else {
        markers.join(",")
    };
    eprintln!("===== guest serial [{tag}] begin =====");
    eprint!("{serial}");
    if !serial.ends_with('\n') {
        eprintln!();
    }
    eprintln!("===== guest serial [{tag}] end =====");

    if let Ok(dir) = std::env::var("KITSUNE_GUEST_DIR") {
        let log_dir = std::path::Path::new(&dir).join("e2e-logs");
        if std::fs::create_dir_all(&log_dir).is_ok() {
            let name = markers
                .first()
                .copied()
                .unwrap_or("e2e")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let path = log_dir.join(format!("{name}-{stamp}.log"));
            let _ = std::fs::write(&path, serial);
            eprintln!("guest serial saved to {}", path.display());
        }
    }
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
    dump_guest_serial(markers, &s);
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
