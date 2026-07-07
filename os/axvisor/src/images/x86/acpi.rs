//! Minimal ACPI tables for x86 Linux direct boot.

use alloc::{vec, vec::Vec};

use super::linux::X86LinuxRange;

pub const ACPI_RSDP_GPA: usize = 0x000e_0000;
pub const ACPI_TABLES_GPA: usize = 0x000e_1000;
pub const ACPI_RESERVED_SIZE: usize = 0x0002_0000;

const ACPI_OEM_ID: &[u8; 6] = b"AXVIS ";
const ACPI_OEM_TABLE_ID: &[u8; 8] = b"X86LINUX";
const LOCAL_APIC_ADDR: u32 = 0xfee0_0000;
const IO_APIC_ADDR: u32 = 0xfec0_0000;
const ACPI_SLEEP_REG_GPA: u64 = 0xfed1_0000;
const IO_APIC_ID: u8 = 1;
const COM1_PORT: u16 = 0x03f8;
const COM1_IRQ: u8 = 4;

const FADT_V5_SIZE: usize = 0x10c;
const FADT_DSDT_OFFSET: usize = 40;
const FADT_IAPC_BOOT_ARCH_OFFSET: usize = 109;
const FADT_FLAGS_OFFSET: usize = 112;
const FADT_XDSDT_OFFSET: usize = 140;
const FADT_SLEEP_CONTROL_OFFSET: usize = 244;
const FADT_SLEEP_STATUS_OFFSET: usize = 256;
const FADT_IAPC_BOOT_ARCH_LEGACY_DEVICES: u16 = 1;
const FADT_FLAG_HW_REDUCED_ACPI: u32 = 1 << 20;

pub struct AcpiImage {
    pub rsdp: [u8; 20],
    pub tables: Vec<u8>,
}

pub const fn reserved_range() -> X86LinuxRange {
    X86LinuxRange::new(ACPI_RSDP_GPA, ACPI_RESERVED_SIZE)
}

pub fn build() -> AcpiImage {
    let mut tables = Vec::new();

    let dsdt = tables.len() as u32;
    build_dsdt(&mut tables);
    let fadt = tables.len() as u32;
    build_fadt(&mut tables, ACPI_TABLES_GPA as u32 + dsdt);
    let madt = tables.len() as u32;
    build_madt(&mut tables);
    let spcr = tables.len() as u32;
    build_spcr(&mut tables);
    let rsdt = tables.len() as u32;
    build_rsdt(&mut tables, &[fadt, madt, spcr]);

    AcpiImage {
        rsdp: build_rsdp(ACPI_TABLES_GPA as u32 + rsdt),
        tables,
    }
}

fn build_rsdp(rsdt_addr: u32) -> [u8; 20] {
    let mut rsdp = [0u8; 20];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(ACPI_OEM_ID);
    rsdp[15] = 1;
    rsdp[16..20].copy_from_slice(&rsdt_addr.to_le_bytes());
    rsdp[8] = checksum(&rsdp);
    rsdp
}

fn build_rsdt(tables: &mut Vec<u8>, offsets: &[u32]) {
    let start = begin_table(tables, b"RSDT", 1);
    for offset in offsets {
        push_u32(tables, ACPI_TABLES_GPA as u32 + *offset);
    }
    end_table(tables, start);
}

fn build_fadt(tables: &mut Vec<u8>, dsdt_addr: u32) {
    let start = begin_table(tables, b"FACP", 5);
    tables.resize(start + FADT_V5_SIZE, 0);
    write_u32(tables, start + FADT_DSDT_OFFSET, dsdt_addr);
    write_u16(
        tables,
        start + FADT_IAPC_BOOT_ARCH_OFFSET,
        FADT_IAPC_BOOT_ARCH_LEGACY_DEVICES,
    );
    write_u32(tables, start + FADT_FLAGS_OFFSET, FADT_FLAG_HW_REDUCED_ACPI);
    write_u64(tables, start + FADT_XDSDT_OFFSET, dsdt_addr as u64);
    write_gas_memory(
        tables,
        start + FADT_SLEEP_CONTROL_OFFSET,
        ACPI_SLEEP_REG_GPA,
    );
    write_gas_memory(
        tables,
        start + FADT_SLEEP_STATUS_OFFSET,
        ACPI_SLEEP_REG_GPA + 1,
    );
    end_table(tables, start);
}

fn build_madt(tables: &mut Vec<u8>) {
    let start = begin_table(tables, b"APIC", 1);
    push_u32(tables, LOCAL_APIC_ADDR);
    push_u32(tables, 1);
    tables.extend_from_slice(&[0, 8, 0, 0]);
    push_u32(tables, 1);
    tables.extend_from_slice(&[1, 12, IO_APIC_ID, 0]);
    push_u32(tables, IO_APIC_ADDR);
    push_u32(tables, 0);
    end_table(tables, start);
}

fn build_spcr(tables: &mut Vec<u8>) {
    let start = begin_table(tables, b"SPCR", 2);
    tables.extend_from_slice(&[0, 0, 0, 0]);
    push_gas_io(tables, COM1_PORT as u64);
    tables.extend_from_slice(&[0, 0]);
    push_u32(tables, COM1_IRQ as u32);
    tables.extend_from_slice(&[7, 0, 1, 0, 3, 0]);
    push_u16(tables, 0xffff);
    push_u16(tables, 0xffff);
    tables.extend_from_slice(&[0, 0, 0]);
    push_u32(tables, 0);
    tables.push(0);
    push_u32(tables, 1_843_200);
    push_u32(tables, 115_200);
    push_u16(tables, 0);
    push_u16(tables, 0);
    end_table(tables, start);
}

fn build_dsdt(tables: &mut Vec<u8>) {
    let mut scope = Vec::new();
    scope.extend(aml_device("COM1", build_com1_aml()));
    let mut aml = Vec::new();
    aml.extend(aml_scope("_SB_", scope));

    let start = begin_table(tables, b"DSDT", 1);
    tables.extend_from_slice(&aml);
    end_table(tables, start);
}

fn build_com1_aml() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(aml_name_decl("_HID", aml_string("PNP0501")));
    body.extend(aml_name_decl("_UID", aml_int(0)));
    body.extend(aml_name_decl("_CRS", serial_crs_aml()));
    body
}

fn serial_crs_aml() -> Vec<u8> {
    aml_resource_template(&[fixed_io_resource(COM1_PORT, 8), irq_resource(COM1_IRQ)])
}

fn fixed_io_resource(base: u16, size: u8) -> Vec<u8> {
    let mut out = vec![0x47, 0x01];
    out.extend_from_slice(&base.to_le_bytes());
    out.extend_from_slice(&base.to_le_bytes());
    out.push(1);
    out.push(size);
    out
}

fn irq_resource(irq: u8) -> Vec<u8> {
    let mask = 1u16 << irq;
    let mut out = vec![0x22];
    out.extend_from_slice(&mask.to_le_bytes());
    out
}

fn begin_table(tables: &mut Vec<u8>, signature: &[u8; 4], revision: u8) -> usize {
    let start = tables.len();
    tables.extend_from_slice(signature);
    push_u32(tables, 0);
    tables.push(revision);
    tables.push(0);
    tables.extend_from_slice(ACPI_OEM_ID);
    tables.extend_from_slice(ACPI_OEM_TABLE_ID);
    push_u32(tables, 1);
    tables.extend_from_slice(b"AXVS");
    push_u32(tables, 1);
    start
}

fn end_table(tables: &mut [u8], start: usize) {
    let len = (tables.len() - start) as u32;
    tables[start + 4..start + 8].copy_from_slice(&len.to_le_bytes());
    tables[start + 9] = checksum(&tables[start..start + len as usize]);
}

fn push_gas_io(tables: &mut Vec<u8>, address: u64) {
    tables.push(1);
    tables.push(8);
    tables.push(0);
    tables.push(1);
    push_u64(tables, address);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_gas_memory(out: &mut [u8], offset: usize, address: u64) {
    out[offset] = 0;
    out[offset + 1] = 8;
    out[offset + 2] = 0;
    out[offset + 3] = 1;
    write_u64(out, offset + 4, address);
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

fn aml_scope(name: &str, body: Vec<u8>) -> Vec<u8> {
    let mut content = aml_name_ref(name);
    content.extend(body);
    aml_pkg_op(&[0x10], content)
}

fn aml_device(name: &str, body: Vec<u8>) -> Vec<u8> {
    let mut content = aml_name_ref(name);
    content.extend(body);
    aml_pkg_op(&[0x5b, 0x82], content)
}

fn aml_name_decl(name: &str, value: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x08);
    out.extend(aml_name_ref(name));
    out.extend(value);
    out
}

fn aml_name_ref(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    assert_eq!(bytes.len(), 4, "AML short names must be 4 bytes");
    bytes.to_vec()
}

fn aml_string(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(0x0d);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    out
}

fn aml_int(value: u64) -> Vec<u8> {
    match value {
        0 => vec![0x00],
        1 => vec![0x01],
        2..=0xff => vec![0x0a, value as u8],
        0x100..=0xffff => {
            let mut out = vec![0x0b];
            out.extend_from_slice(&(value as u16).to_le_bytes());
            out
        }
        _ => {
            let mut out = vec![0x0c];
            out.extend_from_slice(&(value as u32).to_le_bytes());
            out
        }
    }
}

fn aml_buffer(bytes: &[u8]) -> Vec<u8> {
    let mut content = aml_int(bytes.len() as u64);
    content.extend_from_slice(bytes);
    aml_pkg_op(&[0x11], content)
}

fn aml_resource_template(resources: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for resource in resources {
        bytes.extend_from_slice(resource);
    }
    bytes.extend_from_slice(&[0x79, 0x00]);
    aml_buffer(&bytes)
}

fn aml_pkg_op(opcode: &[u8], content: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(opcode);
    out.extend(aml_pkg_len(content.len()));
    out.extend(content);
    out
}

fn aml_pkg_len(content_len: usize) -> Vec<u8> {
    for len_len in 1..=4 {
        let total_len = content_len + len_len;
        let max_len = 1usize << (4 + 8 * (len_len - 1));
        if total_len < max_len {
            if len_len == 1 {
                return vec![total_len as u8];
            }
            let mut bytes = Vec::with_capacity(len_len);
            bytes.push((((len_len - 1) as u8) << 6) | ((total_len as u8) & 0x0f));
            let mut remaining = total_len >> 4;
            for _ in 1..len_len {
                bytes.push((remaining & 0xff) as u8);
                remaining >>= 8;
            }
            return bytes;
        }
    }
    unreachable!("AML package is too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_len(tables: &[u8], offset: usize) -> usize {
        u32::from_le_bytes(tables[offset + 4..offset + 8].try_into().unwrap()) as usize
    }

    fn read_u32(tables: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(tables[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(tables: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(tables[offset..offset + 8].try_into().unwrap())
    }

    fn find_table(tables: &[u8], sig: &[u8; 4]) -> usize {
        let mut offset = 0;
        while offset < tables.len() {
            if &tables[offset..offset + 4] == sig {
                return offset;
            }
            offset += table_len(tables, offset);
        }
        panic!("table not found: {sig:?}");
    }

    #[test]
    fn builds_hardware_reduced_fadt_with_dsdt_pointer() {
        let image = build();
        let dsdt = find_table(&image.tables, b"DSDT");
        let fadt = find_table(&image.tables, b"FACP");
        let dsdt_addr = ACPI_TABLES_GPA as u32 + dsdt as u32;

        assert_eq!(image.tables[fadt + 8], 5);
        assert_eq!(table_len(&image.tables, fadt), FADT_V5_SIZE);
        assert_eq!(read_u32(&image.tables, fadt + FADT_DSDT_OFFSET), dsdt_addr);
        assert_eq!(
            read_u64(&image.tables, fadt + FADT_XDSDT_OFFSET),
            dsdt_addr as u64
        );
        assert_eq!(
            read_u32(&image.tables, fadt + FADT_FLAGS_OFFSET),
            FADT_FLAG_HW_REDUCED_ACPI
        );
        assert_eq!(
            read_u32(&image.tables, fadt + FADT_SLEEP_CONTROL_OFFSET + 4),
            ACPI_SLEEP_REG_GPA as u32
        );
        assert_eq!(
            image.tables[fadt..fadt + FADT_V5_SIZE]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
    }

    #[test]
    fn dsdt_describes_com1_as_pnp0501_irq4() {
        let image = build();
        let dsdt = find_table(&image.tables, b"DSDT");
        let len = table_len(&image.tables, dsdt);
        let bytes = &image.tables[dsdt..dsdt + len];

        assert!(bytes.windows(8).any(|window| window == b"PNP0501\0"));
        assert!(
            bytes
                .windows(8)
                .any(|window| window == [0x47, 0x01, 0xf8, 0x03, 0xf8, 0x03, 0x01, 0x08])
        );
        assert!(bytes.windows(3).any(|window| window == [0x22, 0x10, 0x00]));
    }

    #[test]
    fn rsdp_points_to_rsdt_and_checksums_are_valid() {
        let image = build();
        let rsdt = find_table(&image.tables, b"RSDT");
        assert_eq!(
            u32::from_le_bytes(image.rsdp[16..20].try_into().unwrap()),
            ACPI_TABLES_GPA as u32 + rsdt as u32
        );
        assert_eq!(
            image
                .rsdp
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
    }
}
