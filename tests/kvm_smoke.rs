//! Light KVM smoke tests (no guest Linux image required).

#[path = "support/kvm.rs"]
mod kvm;

#[test]
fn create_vm_and_debug_exit() {
    kvm::require_kvm();

    let config = kitsune::VmmConfig {
        mem_size: 4 * 1024 * 1024,
        num_vcpus: 1,
    };
    let mut vmm = kitsune::Vmm::new(&config).expect("Vmm::new");
    // Real mode: OUT 0x00 to port 0x501 (isa-debug-exit style).
    // HLT alone is handled in-kernel when irqchip/PIT is enabled.
    //   mov dx, 0x501
    //   xor al, al
    //   out dx, al
    let code = [0xba, 0x01, 0x05, 0x30, 0xc0, 0xee];
    vmm.load_flat_binary(&code, 0, 0).expect("load");
    vmm.run().expect("run to debug exit");
}
