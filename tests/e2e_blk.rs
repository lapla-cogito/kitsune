//! End-to-end virtio-blk discovery (requires prepared guest artifacts).

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run.rs"]
mod run;

#[test]
fn boot_with_block_sees_vda() {
    kvm::require_kvm();
    guest::prepare_guest();
    let dir = guest::guest_dir();
    let kernel = dir.join("vmlinux");
    let initrd = dir.join("initrd.img");
    let disk = dir.join("disk.ext4");

    let out = run::run_until(
        &[
            "run",
            "--kernel",
            kernel.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--block",
            disk.to_str().unwrap(),
            "--memory",
            "512",
            "--cmdline",
            "console=ttyS0 reboot=k panic=1 pci=off nomodule",
        ],
        std::time::Duration::from_secs(90),
        &["kitsune-initrd-ok", "kitsune-blk-ok"],
    );
    run::assert_contains(&out, "kitsune-initrd-ok");
    run::assert_contains(&out, "kitsune-blk-ok");
}
