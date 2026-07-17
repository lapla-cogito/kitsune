//! End-to-end virtio-blk.

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run.rs"]
mod run;

fn blk_args(
    kernel: &std::path::Path,
    initrd: &std::path::Path,
    disk: &std::path::Path,
    cpus: u8,
) -> Vec<String> {
    let mut a = vec![
        "run".into(),
        "--kernel".into(),
        kernel.to_str().unwrap().into(),
        "--initrd".into(),
        initrd.to_str().unwrap().into(),
        "--block".into(),
        disk.to_str().unwrap().into(),
        "--memory".into(),
        "512".into(),
        "--cmdline".into(),
        "console=ttyS0 reboot=k panic=1 pci=off nomodule".into(),
    ];
    if cpus != 1 {
        a.push("--cpus".into());
        a.push(cpus.to_string());
    }
    a
}

/// Serialize tests that share host disk images under `KITSUNE_GUEST_DIR`.
static BLK_DISK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_blk(disk: &std::path::Path, cpus: u8, markers: &[&str]) -> String {
    let _guard = BLK_DISK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    kvm::require_kvm();
    guest::prepare_guest();
    let dir = guest::guest_dir();
    let args = blk_args(&dir.join("vmlinux"), &dir.join("initrd.img"), disk, cpus);
    let refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    run::run_until(&refs, std::time::Duration::from_secs(120), markers)
}

/// Device present, small write/readback, and multi-block (~32 KiB) I/O.
#[test]
fn boot_with_block_io_and_bulk() {
    let dir = guest::guest_dir();
    let disk = dir.join("disk.ext4");
    let out = run_blk(
        &disk,
        1,
        &[
            "kitsune-initrd-ok",
            "kitsune-blk-ok",
            "kitsune-blk-io-ok",
            "kitsune-blk-bulk-ok",
        ],
    );
    run::assert_contains(&out, "kitsune-initrd-ok");
    run::assert_contains(&out, "kitsune-blk-ok");
    run::assert_contains(&out, "kitsune-blk-io-ok");
    run::assert_contains(&out, "kitsune-blk-bulk-ok");
    assert!(
        !out.contains("kitsune-blk-io-fail"),
        "block I/O failed:\n{out}"
    );
    assert!(
        !out.contains("kitsune-blk-bulk-fail"),
        "block bulk I/O failed:\n{out}"
    );
    assert!(
        !out.contains("kitsune-blk-mount-fail"),
        "block mount failed:\n{out}"
    );
}

/// Multi-vCPU + virtio-blk datapath (worker vs AP vCPUs).
#[test]
fn boot_two_vcpus_with_block_io() {
    let dir = guest::guest_dir();
    let disk = dir.join("disk.ext4");
    let out = run_blk(
        &disk,
        2,
        &[
            "kitsune-initrd-ok",
            "kitsune-smp-ok",
            "kitsune-blk-ok",
            "kitsune-blk-io-ok",
            "kitsune-blk-bulk-ok",
        ],
    );
    run::assert_contains(&out, "kitsune-initrd-ok");
    run::assert_contains(&out, "kitsune-cpus=2");
    run::assert_contains(&out, "kitsune-smp-ok");
    run::assert_contains(&out, "kitsune-blk-ok");
    run::assert_contains(&out, "kitsune-blk-io-ok");
    run::assert_contains(&out, "kitsune-blk-bulk-ok");
}

/// Host file is mode a-w so kitsune opens the image read-only.
#[test]
fn boot_with_readonly_block() {
    let dir = guest::guest_dir();
    let disk = dir.join("disk-ro.ext4");
    let out = run_blk(
        &disk,
        1,
        &["kitsune-initrd-ok", "kitsune-blk-ok", "kitsune-blk-ro-ok"],
    );
    run::assert_contains(&out, "kitsune-initrd-ok");
    run::assert_contains(&out, "kitsune-blk-ok");
    run::assert_contains(&out, "kitsune-blk-ro-ok");
    assert!(
        !out.contains("kitsune-blk-ro-fail"),
        "read-only block allowed a guest write:\n{out}"
    );
}
