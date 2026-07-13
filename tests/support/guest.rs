//! Paths and preparation for CI guest artifacts.

/// Serialize initrd rebuilds.
static PREPARE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn guest_dir() -> std::path::PathBuf {
    std::env::var_os("KITSUNE_GUEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/ci-guest"))
}

pub fn prepare_guest() {
    let _guard = PREPARE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = guest_dir();
    let status = std::process::Command::new("bash")
        .args(["scripts/ci_prepare_guest.sh", dir.to_str().unwrap()])
        .status()
        .expect("run ci_prepare_guest.sh");
    assert!(status.success(), "ci_prepare_guest.sh failed");
}
