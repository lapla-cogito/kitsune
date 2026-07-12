//! End-to-end multi-vCPU (SMP) bring-up.

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run.rs"]
mod run;

#[test]
fn boot_two_vcpus_reports_online_cpus() {
    kvm::require_kvm();
    guest::prepare_guest();
    let dir = guest::guest_dir();
    let kernel = dir.join("vmlinux");
    let initrd = dir.join("initrd.img");

    let out = run::run_until(
        &[
            "run",
            "--kernel",
            kernel.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--memory",
            "512",
            "--cpus",
            "2",
            "--cmdline",
            "console=ttyS0 reboot=k panic=1 pci=off nomodule",
        ],
        std::time::Duration::from_secs(90),
        &["kitsune-initrd-ok", "kitsune-smp-ok"],
    );
    run::assert_contains(&out, "kitsune-initrd-ok");
    run::assert_contains(&out, "kitsune-cpus=2");
    run::assert_contains(&out, "kitsune-smp-ok");
}
