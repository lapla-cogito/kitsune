//! Kernel command line assembly for direct boot.

/// Options that append kitsune-managed cmdline tokens.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelCmdlineOpts {
    pub initrd: bool,
    pub block: bool,
    pub tap: bool,
}

/// Build the final kernel command line from a user-supplied base string.
pub fn build_kernel_cmdline(base: &str, opts: KernelCmdlineOpts) -> String {
    let mut cmdline = base.to_string();

    if opts.initrd && !cmdline.split_whitespace().any(|t| t.starts_with("rdinit=")) {
        push_token(&mut cmdline, crate::config::INITRD_CMDLINE_EXTRA);
    }

    if opts.block {
        let token = format!(
            "virtio_mmio.device=4K@{:#x}:{}",
            crate::devices::VirtioBlock::MMIO_BASE,
            crate::devices::VirtioBlock::IRQ,
        );
        if !cmdline.contains(&format!(
            "virtio_mmio.device=4K@{:#x}:",
            crate::devices::VirtioBlock::MMIO_BASE
        )) {
            push_token(&mut cmdline, &token);
        }
        if !cmdline.split_whitespace().any(|t| t.starts_with("root=")) {
            push_token(&mut cmdline, "root=/dev/vda rw");
        }
    }

    if opts.tap {
        let token = format!(
            "virtio_mmio.device=4K@{:#x}:{}",
            crate::devices::VirtioNet::MMIO_BASE,
            crate::devices::VirtioNet::IRQ,
        );
        if !cmdline.contains(&format!(
            "virtio_mmio.device=4K@{:#x}:",
            crate::devices::VirtioNet::MMIO_BASE
        )) {
            push_token(&mut cmdline, &token);
        }
    }

    cmdline
}

fn push_token(cmdline: &mut String, token: &str) {
    if !cmdline.is_empty() && !cmdline.ends_with(char::is_whitespace) {
        cmdline.push(' ');
    }
    cmdline.push_str(token);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initrd_appends_rdinit_once() {
        let base = crate::config::DEFAULT_KERNEL_CMDLINE;
        let once = build_kernel_cmdline(
            base,
            KernelCmdlineOpts {
                initrd: true,
                ..KernelCmdlineOpts::default()
            },
        );
        assert!(once.contains("rdinit=/init"));
        assert_eq!(once.matches("rdinit=").count(), 1);

        let already = build_kernel_cmdline(
            "console=ttyS0 rdinit=/custom",
            KernelCmdlineOpts {
                initrd: true,
                ..KernelCmdlineOpts::default()
            },
        );
        assert!(already.contains("rdinit=/custom"));
        assert!(!already.contains("rdinit=/init"));
    }

    #[test]
    fn block_adds_virtio_mmio_and_root() {
        let out = build_kernel_cmdline(
            "console=ttyS0",
            KernelCmdlineOpts {
                block: true,
                ..KernelCmdlineOpts::default()
            },
        );
        assert!(out.contains("virtio_mmio.device=4K@0xd0000000:5"));
        assert!(out.contains("root=/dev/vda rw"));
    }

    #[test]
    fn block_respects_existing_root() {
        let out = build_kernel_cmdline(
            "console=ttyS0 root=/dev/vda1 ro",
            KernelCmdlineOpts {
                block: true,
                ..KernelCmdlineOpts::default()
            },
        );
        assert!(out.contains("root=/dev/vda1 ro"));
        assert!(!out.contains("root=/dev/vda rw"));
    }

    #[test]
    fn tap_adds_net_mmio() {
        let out = build_kernel_cmdline(
            "console=ttyS0",
            KernelCmdlineOpts {
                tap: true,
                ..KernelCmdlineOpts::default()
            },
        );
        assert!(out.contains("virtio_mmio.device=4K@0xd0001000:6"));
    }
}
