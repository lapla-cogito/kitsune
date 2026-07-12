//! End-to-end Linux boot over serial (requires prepared guest artifacts).

#[path = "common/mod.rs"]
mod common;

fn ensure_guest() {
    let dir = common::guest_dir();
    if dir.join("vmlinux").is_file() && dir.join("initrd.img").is_file() {
        return;
    }
    let status = std::process::Command::new("bash")
        .args(["scripts/ci_prepare_guest.sh", dir.to_str().unwrap()])
        .status()
        .expect("run ci_prepare_guest.sh");
    assert!(status.success(), "ci_prepare_guest.sh failed");
}

#[test]
fn boot_initrd_prints_marker() {
    common::require_kvm();
    ensure_guest();
    let dir = common::guest_dir();
    let kernel = dir.join("vmlinux");
    let initrd = dir.join("initrd.img");

    let out = common::run_until(
        &[
            "run",
            "--kernel",
            kernel.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--memory",
            "512",
            "--cmdline",
            "console=ttyS0 reboot=k panic=1 pci=off nomodule",
        ],
        std::time::Duration::from_secs(90),
        &["Linux version", "kitsune-initrd-ok"],
    );
    common::assert_contains(&out, "Linux version");
    common::assert_contains(&out, "kitsune-initrd-ok");
}
