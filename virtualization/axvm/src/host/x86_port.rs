//! Native x86 host I/O port passthrough devices.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ax_errno::{AxResult, ax_err};
use axdevice_base::{AccessWidth, BaseDeviceOps, EmuDeviceType, Port, PortRange};
use axvm_types::PassThroughDeviceConfig;

const PCI_CONFIG_ADDRESS_PORT: u16 = 0xcf8;
const PCI_CONFIG_DATA_PORT: u16 = 0xcfc;
const PCI_CONFIG_PORT_LENGTH: u16 = 8;
const PCI_CONFIG_ENABLE: u32 = 1 << 31;
const PCI_CONFIG_HIDDEN_READ: usize = usize::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PciBdf {
    bus: u8,
    device: u8,
    function: u8,
}

impl PciBdf {
    const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            bus,
            device,
            function,
        }
    }
}

/// Returns true if a port range is the legacy x86 PCI configuration mechanism #1 window.
pub(crate) fn is_pci_config_port_range(base: u16, length: u16) -> bool {
    base == PCI_CONFIG_ADDRESS_PORT && length == PCI_CONFIG_PORT_LENGTH
}

/// A host x86 I/O port range passed directly through to a guest.
pub(crate) struct HostPortPassthrough {
    base: Port,
    length: u16,
}

impl HostPortPassthrough {
    /// Creates a passthrough device for an inclusive host I/O port range.
    pub(crate) fn new(base: u16, length: u16) -> AxResult<Self> {
        if length == 0 {
            return ax_err!(InvalidInput, "host port passthrough range is empty");
        }
        if base.checked_add(length - 1).is_none() {
            return ax_err!(InvalidInput, "host port passthrough range overflows");
        }
        Ok(Self {
            base: Port::new(base),
            length,
        })
    }

    fn end(&self) -> Port {
        Port::new(self.base.number() + self.length - 1)
    }
}

impl BaseDeviceOps<PortRange> for HostPortPassthrough {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(self.base, self.end())
    }

    fn handle_read(&self, port: Port, width: AccessWidth) -> AxResult<usize> {
        match width {
            AccessWidth::Byte => Ok(unsafe { inb(port.number()) } as usize),
            AccessWidth::Word => Ok(unsafe { inw(port.number()) } as usize),
            AccessWidth::Dword => Ok(unsafe { inl(port.number()) } as usize),
            AccessWidth::Qword => ax_err!(Unsupported, "x86 port I/O does not support qword read"),
        }
    }

    fn handle_write(&self, port: Port, width: AccessWidth, value: usize) -> AxResult {
        match width {
            AccessWidth::Byte => unsafe { outb(port.number(), value as u8) },
            AccessWidth::Word => unsafe { outw(port.number(), value as u16) },
            AccessWidth::Dword => unsafe { outl(port.number(), value as u32) },
            AccessWidth::Qword => {
                return ax_err!(Unsupported, "x86 port I/O does not support qword write");
            }
        }
        Ok(())
    }
}

/// Filtered passthrough for x86 PCI configuration mechanism #1.
pub(crate) struct HostPciConfigPortPassthrough {
    config_address: AtomicU32,
    allowed_bdfs: Vec<PciBdf>,
}

impl HostPciConfigPortPassthrough {
    pub(crate) fn new(devices: &[PassThroughDeviceConfig]) -> AxResult<Self> {
        let mut allowed_bdfs = Vec::new();
        for device in devices {
            if let Some((endpoint, root)) = parse_pci_intx_bdfs(&device.name) {
                push_unique_bdf(&mut allowed_bdfs, endpoint);
                push_unique_bdf(&mut allowed_bdfs, root);
            }
        }

        if allowed_bdfs.is_empty() {
            return ax_err!(
                InvalidInput,
                "filtered PCI config port requires at least one PCI device"
            );
        }

        Ok(Self {
            config_address: AtomicU32::new(0),
            allowed_bdfs,
        })
    }

    fn selected_bdf(&self) -> Option<PciBdf> {
        let address = self.config_address.load(Ordering::Acquire);
        if address & PCI_CONFIG_ENABLE == 0 {
            return None;
        }
        Some(PciBdf::new(
            ((address >> 16) & 0xff) as u8,
            ((address >> 11) & 0x1f) as u8,
            ((address >> 8) & 0x7) as u8,
        ))
    }

    fn selected_bdf_allowed(&self) -> bool {
        self.selected_bdf()
            .is_some_and(|bdf| self.allowed_bdfs.contains(&bdf))
    }
}

impl BaseDeviceOps<PortRange> for HostPciConfigPortPassthrough {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::Dummy
    }

    fn address_range(&self) -> PortRange {
        PortRange::new(
            Port::new(PCI_CONFIG_ADDRESS_PORT),
            Port::new(PCI_CONFIG_ADDRESS_PORT + PCI_CONFIG_PORT_LENGTH - 1),
        )
    }

    fn handle_read(&self, port: Port, width: AccessWidth) -> AxResult<usize> {
        let port_number = port.number();
        if pci_config_address_access(port_number, width) {
            let shift = (port_number - PCI_CONFIG_ADDRESS_PORT) as usize * 8;
            return Ok(
                (self.config_address.load(Ordering::Acquire) as usize >> shift) & width_mask(width),
            );
        }

        match port_number {
            PCI_CONFIG_DATA_PORT..=0xcff => {
                if self.selected_bdf_allowed() {
                    read_host_port(port, width)
                } else {
                    Ok(PCI_CONFIG_HIDDEN_READ & width_mask(width))
                }
            }
            _ => ax_err!(InvalidInput, "port outside PCI config range"),
        }
    }

    fn handle_write(&self, port: Port, width: AccessWidth, value: usize) -> AxResult {
        let port_number = port.number();
        if pci_config_address_access(port_number, width) {
            let shift = (port_number - PCI_CONFIG_ADDRESS_PORT) as usize * 8;
            let mask = (width_mask(width) << shift) as u32;
            let old = self.config_address.load(Ordering::Acquire);
            let new = (old & !mask) | (((value & width_mask(width)) as u32) << shift);
            self.config_address.store(new, Ordering::Release);
            unsafe { outl(PCI_CONFIG_ADDRESS_PORT, new) };
            return Ok(());
        }

        match port_number {
            PCI_CONFIG_DATA_PORT..=0xcff => {
                if self.selected_bdf_allowed() {
                    write_host_port(port, width, value)
                } else {
                    Ok(())
                }
            }
            _ => ax_err!(InvalidInput, "port outside PCI config range"),
        }
    }
}

fn pci_config_address_access(port: u16, width: AccessWidth) -> bool {
    let start = port as usize;
    let end = start.saturating_add(width_bytes(width));
    start >= PCI_CONFIG_ADDRESS_PORT as usize && end <= PCI_CONFIG_DATA_PORT as usize
}

fn width_bytes(width: AccessWidth) -> usize {
    match width {
        AccessWidth::Byte => 1,
        AccessWidth::Word => 2,
        AccessWidth::Dword => 4,
        AccessWidth::Qword => 8,
    }
}

fn parse_pci_intx_bdfs(name: &str) -> Option<(PciBdf, PciBdf)> {
    let spec = name.strip_prefix("pci-intx:")?;
    let mut bus = None;
    let mut dev = None;
    let mut func = None;
    let mut root_dev = None;
    let mut root_func = Some(0);

    for field in spec.split(',') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let Some(value) = parse_u8_config_value(value) else {
            continue;
        };
        match key.trim() {
            "bus" => bus = Some(value),
            "dev" | "device" => dev = Some(value),
            "func" | "function" => func = Some(value),
            "root_dev" | "root_device" => root_dev = Some(value),
            "root_func" | "root_function" => root_func = Some(value),
            _ => {}
        }
    }

    Some((
        PciBdf::new(bus?, dev?, func?),
        PciBdf::new(0, root_dev?, root_func?),
    ))
}

fn parse_u8_config_value(value: &str) -> Option<u8> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16).ok()
    } else {
        value.parse::<u8>().ok()
    }
}

fn push_unique_bdf(bdfs: &mut Vec<PciBdf>, bdf: PciBdf) {
    if !bdfs.contains(&bdf) {
        bdfs.push(bdf);
    }
}

fn width_mask(width: AccessWidth) -> usize {
    match width {
        AccessWidth::Byte => 0xff,
        AccessWidth::Word => 0xffff,
        AccessWidth::Dword => 0xffff_ffff,
        AccessWidth::Qword => usize::MAX,
    }
}

fn read_host_port(port: Port, width: AccessWidth) -> AxResult<usize> {
    match width {
        AccessWidth::Byte => Ok(unsafe { inb(port.number()) } as usize),
        AccessWidth::Word => Ok(unsafe { inw(port.number()) } as usize),
        AccessWidth::Dword => Ok(unsafe { inl(port.number()) } as usize),
        AccessWidth::Qword => ax_err!(Unsupported, "x86 port I/O does not support qword read"),
    }
}

fn write_host_port(port: Port, width: AccessWidth, value: usize) -> AxResult {
    match width {
        AccessWidth::Byte => unsafe { outb(port.number(), value as u8) },
        AccessWidth::Word => unsafe { outw(port.number(), value as u16) },
        AccessWidth::Dword => unsafe { outl(port.number(), value as u32) },
        AccessWidth::Qword => {
            return ax_err!(Unsupported, "x86 port I/O does not support qword write");
        }
    }
    Ok(())
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack));
    }
    value
}

unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe {
        core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack));
    }
    value
}

unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack));
    }
    value
}

unsafe fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack));
    }
}

unsafe fn outw(port: u16, value: u16) {
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack));
    }
}

unsafe fn outl(port: u16, value: u32) {
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_port_range_is_inclusive() {
        let dev = HostPortPassthrough::new(0x6000, 0x80).unwrap();

        assert_eq!(
            dev.address_range(),
            PortRange::new(Port::new(0x6000), Port::new(0x607f))
        );
    }

    #[test]
    fn passthrough_port_range_rejects_empty_and_overflowing_ranges() {
        assert!(HostPortPassthrough::new(0x6000, 0).is_err());
        assert!(HostPortPassthrough::new(0xfff0, 0x20).is_err());
    }

    #[test]
    fn passthrough_port_rejects_qword_without_touching_hardware() {
        let dev = HostPortPassthrough::new(0x6000, 0x80).unwrap();

        assert!(
            dev.handle_read(Port::new(0x6000), AccessWidth::Qword)
                .is_err()
        );
        assert!(
            dev.handle_write(Port::new(0x6000), AccessWidth::Qword, 0)
                .is_err()
        );
    }
}
