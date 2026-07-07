// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::format;
#[cfg(all(
    feature = "fs",
    any(target_arch = "x86_64", target_arch = "loongarch64")
))]
use core::sync::atomic::{AtomicBool, Ordering};

use ax_errno::{AxResult, ax_err_type};
#[cfg(target_arch = "x86_64")]
use axvm::InterruptTriggerMode;
#[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
use axvm::config::VMBootProtocol;
use axvm::{
    AxVM, GuestPhysAddr,
    config::{
        AxVCpuConfig, AxVMConfig, AxVMConfigParams, GuestBootPolicy, PhysCpuList, RamdiskInfo,
        VMImageConfig,
    },
};
use axvmconfig::{AxVMCrateConfig, VMType};

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "riscv64"
))]
use crate::fdt::*;
use crate::images::ImageLoader;

/// Default BIOS load GPA for x86_64 built-in BIOS.
#[cfg(target_arch = "x86_64")]
const DEFAULT_X86_BIOS_LOAD_GPA: usize = 0x8000;

#[cfg(all(
    feature = "fs",
    any(target_arch = "x86_64", target_arch = "loongarch64")
))]
static HOST_FILESYSTEM_RELEASE_REQUIRED: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub mod vmcfg {
    use alloc::{string::String, vec, vec::Vec};

    /// Default static VM configs. Used when no VM config is provided.
    pub fn default_static_vm_configs() -> Vec<&'static str> {
        vec![]
    }

    /// Read VM configs from filesystem
    #[cfg(feature = "fs")]
    pub fn filesystem_vm_configs() -> Vec<String> {
        let config_dir = "/guest/vm_default";
        crate::manager::AxvmManager::filesystem_vm_configs(config_dir)
            .into_iter()
            .filter_map(
                |content| match axvmconfig::AxVMCrateConfig::from_toml(&content) {
                    Ok(_) => Some(content),
                    Err(e) => {
                        warn!("Filesystem VM config is invalid: {:?}", e);
                        None
                    }
                },
            )
            .collect()
    }

    /// Fallback function for when "fs" feature is not enabled
    #[cfg(not(feature = "fs"))]
    pub fn filesystem_vm_configs() -> Vec<String> {
        Vec::new()
    }

    include!(concat!(env!("OUT_DIR"), "/vm_configs.rs"));
}

pub fn init_guest_vms() {
    // Initialize LoongArch firmware resources before guest configs are materialized.
    #[cfg(target_arch = "loongarch64")]
    {
        init_guest_boot_resources();
    }

    // First try to get configs from filesystem if fs feature is enabled
    let mut gvm_raw_configs = vmcfg::filesystem_vm_configs();

    // If no filesystem configs found, fallback to static configs
    if gvm_raw_configs.is_empty() {
        let static_configs = vmcfg::static_vm_configs();
        if static_configs.is_empty() {
            info!("Static VM configs are empty.");
            info!("Now axvisor will entry the shell...");
        } else {
            info!("Using static VM configs.");
        }
        // Convert static configs to String type
        gvm_raw_configs.extend(static_configs.into_iter().map(|s| s.into()));
    }

    for raw_cfg_str in gvm_raw_configs {
        debug!("Initializing guest VM with config: {:#?}", raw_cfg_str);
        if let Err(e) = init_guest_vm(&raw_cfg_str) {
            error!("Failed to initialize guest VM: {e:?}");
        }
    }
}

pub fn init_guest_vm(raw_cfg: &str) -> AxResult<usize> {
    #[allow(unused_mut)]
    let mut vm_create_config = AxVMCrateConfig::from_toml(raw_cfg)
        .map_err(|e| ax_err_type!(InvalidData, format!("Failed to resolve VM config: {e:?}")))?;

    #[cfg(all(
        feature = "fs",
        any(target_arch = "x86_64", target_arch = "loongarch64")
    ))]
    let release_host_filesystem = vm_config_needs_host_filesystem_release(&vm_create_config);

    if let Some(linux) = super::images::get_image_header(&vm_create_config) {
        debug!(
            "VM[{}] Linux header: {:#x?}",
            vm_create_config.base.id, linux
        );
    }

    #[allow(unused_mut)]
    let mut vm_config = build_axvm_config(&vm_create_config);

    // Handle FDT-related operations for architectures that boot guests with DTB.
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    let guest_dtb = handle_fdt_operations(&mut vm_config, &mut vm_create_config)?;
    #[cfg(target_arch = "loongarch64")]
    handle_fdt_operations(&mut vm_config, &mut vm_create_config)?;

    sync_axvm_config_from_crate_config(&mut vm_config, &vm_create_config);

    #[cfg(target_arch = "x86_64")]
    let skip_guest_address_adjustment = x86_linux_direct_boot_config(&vm_create_config);
    #[cfg(not(target_arch = "x86_64"))]
    let skip_guest_address_adjustment = false;
    vm_config.set_boot_policy(guest_boot_policy(
        &vm_create_config,
        skip_guest_address_adjustment,
    ));

    // info!("after parse_vm_interrupt, crate VM[{}] with config: {:#?}", vm_config.id(), vm_config);
    info!("Creating VM[{}] {:?}", vm_config.id(), vm_config.name());

    // Create VM.
    let vm = AxVM::new(vm_config)
        .map_err(|e| ax_err_type!(InvalidData, format!("Failed to create VM: {e:?}")))?;
    let vm_id = vm.id();

    let memory_layout = vm.prepare_memory_layout()?;
    let main_mem = memory_layout.main_memory().clone();

    // Load corresponding images for VM.
    info!("VM[{}] created success, loading images...", vm.id());

    #[cfg(target_arch = "x86_64")]
    register_x86_passthrough_irq_routes(&vm_create_config);

    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    let mut loader = ImageLoader::new(main_mem, vm_create_config, vm.clone(), guest_dtb);
    #[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
    let mut loader = ImageLoader::new(main_mem, vm_create_config, vm.clone());
    loader.load()?;

    vm.prepare()
        .map_err(|e| ax_err_type!(InvalidData, format!("VM[{}] setup failed: {e:?}", vm.id())))?;

    if !axvm::register_vm(vm) {
        return Err(ax_err_type!(
            AlreadyExists,
            format!("VM[{vm_id}] already exists")
        ));
    }

    #[cfg(target_arch = "loongarch64")]
    crate::manager::register_loongarch_passthrough_irq_routes(vm_id);

    #[cfg(all(
        feature = "fs",
        any(target_arch = "x86_64", target_arch = "loongarch64")
    ))]
    if release_host_filesystem {
        #[cfg(target_arch = "x86_64")]
        register_x86_host_fs_passthrough_irq_route();
        HOST_FILESYSTEM_RELEASE_REQUIRED.store(true, Ordering::Release);
    }

    Ok(vm_id)
}

#[cfg(target_arch = "x86_64")]
fn register_x86_passthrough_irq_routes(config: &AxVMCrateConfig) {
    for device in &config.devices.passthrough_devices {
        if let Some(info) = parse_x86_passthrough_intx_route(device) {
            match register_x86_passthrough_intx_route(info, device.irq_id) {
                Ok(()) => {}
                Err(err) => warn!(
                    "failed to register x86 passthrough INTx route for {}: {err:?}",
                    device.name
                ),
            }
        }

        if let Some((start, end)) = parse_x86_passthrough_msi_vector_range(device) {
            axvm::register_x86_msi_vector_forwarding_range(start as usize, end as usize);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn parse_x86_passthrough_intx_route(
    device: &axvmconfig::PassThroughDeviceConfig,
) -> Option<ax_driver::probe::pci::PciInfo> {
    use ax_driver::probe::pci::{PciAddress, PciInfo, PciIntxRoute};

    let spec = parse_x86_pci_intx_spec(&device.name)?;
    Some(PciInfo {
        address: PciAddress::new(0, spec.bus, spec.dev, spec.func),
        interrupt_pin: spec.pin,
        interrupt_line: 0,
        intx_route: Some(PciIntxRoute {
            root_device: spec.root_dev,
            root_function: spec.root_func,
            root_pin: spec.root_pin,
        }),
    })
}

#[cfg(target_arch = "x86_64")]
pub(crate) struct X86PciIntxSpec {
    pub(crate) bus: u8,
    pub(crate) dev: u8,
    pub(crate) func: u8,
    pub(crate) pin: u8,
    pub(crate) root_dev: u8,
    pub(crate) root_func: u8,
    pub(crate) root_pin: u8,
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn parse_x86_pci_intx_spec(name: &str) -> Option<X86PciIntxSpec> {
    let spec = name.strip_prefix("pci-intx:")?;
    let mut bus = None;
    let mut dev = None;
    let mut func = None;
    let mut pin = None;
    let mut root_dev = None;
    let mut root_func = None;
    let mut root_pin = None;

    for field in spec.split(',') {
        let Some((key, value)) = field.split_once('=') else {
            warn!("ignore malformed x86 passthrough INTx field `{field}`");
            continue;
        };
        let value = match parse_u8_config_value(value) {
            Some(value) => value,
            None => {
                warn!("ignore invalid x86 passthrough INTx value `{key}={value}`");
                continue;
            }
        };
        match key.trim() {
            "bus" => bus = Some(value),
            "dev" | "device" => dev = Some(value),
            "func" | "function" => func = Some(value),
            "pin" => pin = Some(value),
            "root_dev" | "root_device" => root_dev = Some(value),
            "root_func" | "root_function" => root_func = Some(value),
            "root_pin" => root_pin = Some(value),
            "msi_start" | "msi_end" | "vector_start" | "vector_end" => {}
            unknown => warn!("ignore unknown x86 passthrough INTx key `{unknown}`"),
        }
    }

    Some(X86PciIntxSpec {
        bus: bus?,
        dev: dev?,
        func: func?,
        pin: pin?,
        root_dev: root_dev?,
        root_func: root_func?,
        root_pin: root_pin?,
    })
}

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
fn parse_x86_passthrough_msi_vector_range(
    device: &axvmconfig::PassThroughDeviceConfig,
) -> Option<(u8, u8)> {
    let spec = device
        .name
        .strip_prefix("pci-msi:")
        .or_else(|| device.name.strip_prefix("pci-intx:"))?;
    let mut start = None;
    let mut end = None;

    for field in spec.split(',') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = parse_u8_config_value(value)?;
        match key.trim() {
            "msi_start" | "vector_start" => start = Some(value),
            "msi_end" | "vector_end" => end = Some(value),
            _ => {}
        }
    }

    match (start, end) {
        (Some(start), Some(end)) => Some((start, end)),
        (Some(vector), None) | (None, Some(vector)) => Some((vector, vector)),
        (None, None) => None,
    }
}

#[cfg(target_arch = "x86_64")]
fn register_x86_passthrough_intx_route(
    info: ax_driver::probe::pci::PciInfo,
    guest_gsi: usize,
) -> Result<(), ax_hal::irq::IrqError> {
    let Some(result) = ax_driver::probe::acpi::with_acpi(|acpi| acpi.pci_irq_for_endpoint(info))
    else {
        warn!("x86 passthrough PCI INTx route requires ACPI routing for {info:?}");
        return Ok(());
    };
    let route = match result {
        Ok(Some(route)) => route,
        Ok(None) => {
            warn!("x86 passthrough PCI INTx ACPI route was not found for {info:?}");
            return Ok(());
        }
        Err(err) => {
            warn!("failed to resolve x86 passthrough PCI INTx ACPI route for {info:?}: {err}");
            return Ok(());
        }
    };

    let binding = ax_driver::BindingIrq::from(route.gsi);
    let trigger = x86_intx_forwarding_trigger(&binding);
    let host_irq = resolve_binding_irq(binding)?;
    axvm::register_x86_ioapic_irq_forwarding_route_with_trigger(guest_gsi, host_irq, trigger);
    info!(
        "Registered x86 passthrough PCI INTx forwarding route: endpoint {} guest GSI \
         {guest_gsi} <- host IRQ {host_irq:?}, trigger {trigger:?}",
        info.address
    );
    Ok(())
}

pub(crate) fn build_axvm_config(cfg: &AxVMCrateConfig) -> AxVMConfig {
    AxVMConfig::new(AxVMConfigParams {
        id: cfg.base.id,
        name: cfg.base.name.clone(),
        vm_type: VMType::from(cfg.base.vm_type),
        phys_cpu_ls: PhysCpuList::new(
            cfg.base.cpu_num,
            cfg.base.phys_cpu_ids.clone(),
            cfg.base.phys_cpu_sets.clone(),
        ),
        cpu_config: AxVCpuConfig {
            bsp_entry: GuestPhysAddr::from(cfg.kernel.entry_point),
            ap_entry: GuestPhysAddr::from(cfg.kernel.entry_point),
            #[cfg(target_arch = "loongarch64")]
            boot_args: [0; 3],
            #[cfg(target_arch = "loongarch64")]
            boot_stack_top: 0,
            #[cfg(target_arch = "loongarch64")]
            firmware_boot: cfg.kernel.effective_boot_protocol() == VMBootProtocol::Uefi,
        },
        image_config: VMImageConfig {
            kernel_load_gpa: GuestPhysAddr::from(cfg.kernel.kernel_load_addr),
            loaded_from_filesystem: cfg.kernel.image_location.as_deref() == Some("fs"),
            bios_load_gpa: configured_bios_load_gpa(cfg),
            dtb_load_gpa: cfg.kernel.dtb_load_addr.map(GuestPhysAddr::from),
            ramdisk: cfg.kernel.ramdisk_load_addr.map(|addr| RamdiskInfo {
                load_gpa: GuestPhysAddr::from(addr),
                size: None,
            }),
        },
        emu_devices: cfg.devices.emu_devices.clone(),
        pass_through_devices: cfg.devices.passthrough_devices.clone(),
        excluded_devices: cfg.devices.excluded_devices.clone(),
        pass_through_addresses: cfg.devices.passthrough_addresses.clone(),
        reserved_address_ranges: Vec::new(),
        pass_through_ports: cfg.devices.passthrough_ports.clone(),
        address_space_policy: cfg.devices.address_space_policy,
        memory_regions: cfg.kernel.memory_regions.clone(),
        boot_policy: GuestBootPolicy::KeepConfigured,
        interrupt_mode: cfg.devices.interrupt_mode,
    })
}

fn sync_axvm_config_from_crate_config(vm_config: &mut AxVMConfig, cfg: &AxVMCrateConfig) {
    vm_config.set_memory_regions(cfg.kernel.memory_regions.clone());
}

fn guest_boot_policy(
    cfg: &AxVMCrateConfig,
    skip_guest_address_adjustment: bool,
) -> GuestBootPolicy {
    if skip_guest_address_adjustment {
        GuestBootPolicy::KeepConfigured
    } else {
        GuestBootPolicy::AdjustKernelForBootProtocol {
            protocol: cfg.kernel.effective_boot_protocol(),
        }
    }
}

fn configured_bios_load_gpa(cfg: &AxVMCrateConfig) -> Option<GuestPhysAddr> {
    if !cfg.kernel.enable_bios {
        return None;
    }

    if let Some(addr) = cfg.kernel.bios_load_addr {
        return Some(GuestPhysAddr::from(addr));
    }

    #[cfg(target_arch = "x86_64")]
    if cfg.kernel.boot_firmware_path().is_none()
        && cfg.kernel.effective_boot_protocol() == VMBootProtocol::Multiboot
    {
        return Some(GuestPhysAddr::from(DEFAULT_X86_BIOS_LOAD_GPA));
    }

    None
}

#[cfg(all(
    feature = "fs",
    any(target_arch = "x86_64", target_arch = "loongarch64")
))]
fn vm_config_needs_host_filesystem_release(config: &AxVMCrateConfig) -> bool {
    config.kernel.image_location.as_deref() == Some("fs")
        && (!config.devices.passthrough_devices.is_empty()
            || !config.devices.passthrough_addresses.is_empty()
            || !config.devices.passthrough_ports.is_empty())
}

#[cfg(all(
    feature = "fs",
    any(target_arch = "x86_64", target_arch = "loongarch64")
))]
pub fn host_filesystem_release_required() -> bool {
    HOST_FILESYSTEM_RELEASE_REQUIRED.load(Ordering::Acquire)
}

#[cfg(all(feature = "fs", target_arch = "x86_64"))]
fn register_x86_host_fs_passthrough_irq_route() {
    let (_, _, _, guest_gsi) = crate::images::x86_qemu_passthrough_block_intx();
    let info = x86_host_fs_passthrough_pci_info();

    let route = match ax_driver::pci::resolve_intx_binding(info) {
        Ok(Some(binding)) => {
            let trigger = x86_intx_forwarding_trigger(&binding);
            resolve_binding_irq(binding).map(|host_irq| (host_irq, trigger))
        }
        Ok(None) => {
            warn!("x86 host filesystem passthrough PCI INTx route was not found for {info:?}");
            return;
        }
        Err(err) => {
            warn!("failed to resolve x86 host filesystem passthrough PCI INTx route: {err:?}");
            return;
        }
    };

    match route {
        Ok((host_irq, trigger)) => {
            axvm::register_x86_ioapic_irq_forwarding_route_with_trigger(
                guest_gsi, host_irq, trigger,
            );
            axvm::register_x86_ioapic_irq_forwarding_activator(
                guest_gsi,
                unmask_x86_host_fs_passthrough_intx,
            );
            info!(
                "Registered x86 host filesystem PCI INTx forwarding route: guest GSI \
                 {guest_gsi} <- host IRQ {host_irq:?}, trigger {trigger:?}"
            );
        }
        Err(err) => {
            warn!(
                "failed to resolve x86 host filesystem passthrough IRQ source into host IRQ: \
                 {err:?}"
            );
        }
    }
}

#[cfg(all(feature = "fs", target_arch = "x86_64"))]
pub(crate) fn prepare_x86_host_fs_passthrough_devices() {
    let info = x86_host_fs_passthrough_pci_info();
    match ax_driver::pci::prepare_intx_passthrough(info) {
        Ok(()) => {
            info!("Prepared x86 host filesystem PCI INTx passthrough device {info:?}");
        }
        Err(err) => {
            warn!("failed to prepare x86 host filesystem PCI INTx passthrough device: {err:?}");
        }
    }
}

#[cfg(all(feature = "fs", target_arch = "x86_64"))]
fn unmask_x86_host_fs_passthrough_intx() {
    let info = x86_host_fs_passthrough_pci_info();
    match ax_driver::pci::unmask_intx_passthrough(info) {
        Ok(()) => {
            info!("Unmasked x86 host filesystem PCI INTx passthrough device {info:?}");
        }
        Err(err) => {
            warn!("failed to unmask x86 host filesystem PCI INTx passthrough device: {err:?}");
        }
    }
}

#[cfg(all(feature = "fs", target_arch = "x86_64"))]
fn x86_host_fs_passthrough_pci_info() -> ax_driver::probe::pci::PciInfo {
    use ax_driver::probe::pci::{PciAddress, PciInfo, PciIntxRoute};

    let (device, function, pin, _) = crate::images::x86_qemu_passthrough_block_intx();
    PciInfo {
        address: PciAddress::new(0, 0, device, function),
        interrupt_pin: pin,
        interrupt_line: 0,
        intx_route: Some(PciIntxRoute {
            root_device: device,
            root_function: function,
            root_pin: pin,
        }),
    }
}

#[cfg(target_arch = "x86_64")]
fn resolve_binding_irq(
    binding: ax_driver::BindingIrq,
) -> Result<ax_hal::irq::IrqId, ax_hal::irq::IrqError> {
    use ax_hal::irq;

    match binding {
        ax_driver::BindingIrq::Id(irq) => Ok(irq),
        ax_driver::BindingIrq::Source(source) => match source {
            ax_driver::BindingIrqSource::AcpiGsi(gsi) => {
                irq::resolve_irq_source(irq::IrqSource::AcpiGsi(gsi))
            }
            ax_driver::BindingIrqSource::AcpiGsiRoute(route) => {
                irq::resolve_irq_source(irq::IrqSource::AcpiGsiRoute(route))
            }
            ax_driver::BindingIrqSource::FdtInterrupt(_) => Err(irq::IrqError::Unsupported),
        },
    }
}

#[cfg(target_arch = "x86_64")]
fn x86_intx_forwarding_trigger(binding: &ax_driver::BindingIrq) -> InterruptTriggerMode {
    match binding {
        ax_driver::BindingIrq::Source(ax_driver::BindingIrqSource::AcpiGsiRoute(route)) => {
            match route.trigger {
                ax_hal::irq::AcpiIrqTrigger::Edge => InterruptTriggerMode::EdgeTriggered,
                ax_hal::irq::AcpiIrqTrigger::Level => InterruptTriggerMode::LevelTriggered,
            }
        }
        _ => InterruptTriggerMode::LevelTriggered,
    }
}

#[cfg(target_arch = "x86_64")]
fn x86_linux_direct_boot_config(config: &AxVMCrateConfig) -> bool {
    crate::images::is_x86_linux_image_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axvmconfig::{VmMemConfig, VmMemMappingType};

    fn memory_region(gpa: usize, size: usize, map_type: VmMemMappingType) -> VmMemConfig {
        VmMemConfig {
            gpa,
            size,
            flags: 0x7,
            map_type,
        }
    }

    #[test]
    fn sync_axvm_config_keeps_fdt_reserved_memory_regions() {
        let mut crate_config = AxVMCrateConfig::default();
        crate_config.kernel.memory_regions.push(memory_region(
            0x8000_0000,
            0x200000,
            VmMemMappingType::MapIdentical,
        ));
        let mut vm_config = build_axvm_config(&crate_config);

        crate_config.kernel.memory_regions.push(memory_region(
            0x110000,
            0x10000,
            VmMemMappingType::MapReserved,
        ));
        assert_eq!(vm_config.memory_regions().len(), 1);

        sync_axvm_config_from_crate_config(&mut vm_config, &crate_config);

        let regions = vm_config.memory_regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[1].gpa, 0x110000);
        assert_eq!(regions[1].size, 0x10000);
        assert_eq!(regions[1].map_type, VmMemMappingType::MapReserved);
    }
}
