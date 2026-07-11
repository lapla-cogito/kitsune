/// Configure a vCPU for 16-bit real mode with CS base 0 and the given entry RIP.
pub fn setup_real_mode(vcpu: &kvm_ioctls::VcpuFd, entry: u64) -> crate::error::Result<()> {
    let mut sregs = vcpu.get_sregs().map_err(crate::error::Error::KvmIoctl)?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs)
        .map_err(crate::error::Error::KvmIoctl)?;

    let mut regs = vcpu.get_regs().map_err(crate::error::Error::KvmIoctl)?;
    regs.rip = entry;
    // Bit 1 of RFLAGS is reserved and must be 1 on x86.
    regs.rflags = 2;
    vcpu.set_regs(&regs)
        .map_err(crate::error::Error::KvmIoctl)?;

    Ok(())
}
