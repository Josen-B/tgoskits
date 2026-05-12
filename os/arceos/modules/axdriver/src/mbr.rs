extern crate alloc;

use gpt_disk_io::{
    BlockIo,
    gpt_disk_types::{BlockSize, Lba, LbaRangeInclusive, MasterBootRecord},
};
use log::{debug, info};

use super::prelude::*;

struct BlockDriverAdapter<'a, T>(&'a mut T);

impl<T: BlockDriverOps> BlockIo for BlockDriverAdapter<'_, T> {
    type Error = DevError;

    fn block_size(&self) -> BlockSize {
        BlockSize::from_usize(self.0.block_size()).unwrap()
    }

    fn num_blocks(&mut self) -> Result<u64, Self::Error> {
        Ok(self.0.num_blocks())
    }

    fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<(), Self::Error> {
        self.block_size().assert_valid_block_buffer(dst);
        for (i, chunk) in dst.chunks_exact_mut(self.0.block_size()).enumerate() {
            self.0.read_block(start_lba.to_u64() + i as u64, chunk)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, start_lba: Lba, src: &[u8]) -> Result<(), Self::Error> {
        self.block_size().assert_valid_block_buffer(src);
        for (i, chunk) in src.chunks_exact(self.0.block_size()).enumerate() {
            self.0.write_block(start_lba.to_u64() + i as u64, chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush()
    }
}

fn parse_mbr(mbr_block: &[u8]) -> DevResult<MasterBootRecord> {
    mbr_block
        .get(..core::mem::size_of::<MasterBootRecord>())
        .map(|bytes| {
            let ptr = bytes.as_ptr() as *const MasterBootRecord;
            unsafe { ptr.read_unaligned() }
        })
        .ok_or(DevError::BadState)
}

/// A Mbr partition.
pub struct MbrPartitionDev<T> {
    inner: T,
    range: LbaRangeInclusive,
}

impl<T: BlockDriverOps> MbrPartitionDev<T> {
    /// Creates a new Mbr partition device from the given block storage device driver.
    /// Will use the first bootable partition
    pub fn new(mut inner: T) -> DevResult<Self> {
        let bs = BlockSize::BS_512;
        let mut mbr_block_buf = alloc::vec![0u8; bs.to_usize().unwrap()];
        let total_blocks = {
            let mut block_io = BlockDriverAdapter(&mut inner);
            block_io.read_blocks(Lba(0), &mut mbr_block_buf)?;
            block_io.num_blocks()?
        };
        let mbr = parse_mbr(&mbr_block_buf)?;

        if mbr.signature == [0x55, 0xaa] {
            debug!("Found MBR: {:x?}", mbr);

            let mut selected_range = None;
            for i in 0..4 {
                let partition = &mbr.partitions[i];
                match partition.os_indicator {
                    0x07 => {
                        debug!("Found a NTFS/exFAT MBR partition[{}]", i);
                    }
                    0x0c => {
                        info!("Found a FAT32 MBR partition[{}]", i);
                    }
                    0x0f => {
                        debug!("Found an Extended partition[{}].", i);
                    }
                    0x83 => {
                        info!("Found a Linux (ext2/3/4) MBR partition[{}].", i);
                        if partition.size_in_lba.to_u32() != 0 && partition.boot_indicator == 0x80 {
                            let start = partition.starting_lba.to_u32() as u64;
                            let size = partition.size_in_lba.to_u32() as u64;
                            let Some(end_exclusive) = start.checked_add(size) else {
                                error!("Invalid MBR partition[{}] range.", i);
                                return Err(DevError::BadState);
                            };
                            if end_exclusive > total_blocks {
                                error!(
                                    "MBR partition[{}] range {:#x}..{:#x} exceeds device block \
                                     count {:#x}.",
                                    i, start, end_exclusive, total_blocks
                                );
                                return Err(DevError::InvalidParam);
                            }
                            let range = LbaRangeInclusive::new(Lba(start), Lba(end_exclusive - 1))
                                .ok_or(DevError::BadState)?;
                            info!(
                                "Selecting this bootable partition[{}] {}M @ {:#x} ~ {:#x} as the \
                                 rootfs",
                                i,
                                (size * bs.to_u64()) / (1024 * 1024),
                                range.start().to_u64(),
                                range.end().to_u64()
                            );
                            selected_range = Some(range);
                            break;
                        }
                    }
                    0xee => {
                        debug!("Found GPT protective partition[{}].", i);
                    }
                    0xef => {
                        debug!("Found (ESP) EFI system partition[{}].", i);
                    }
                    _ => {
                        debug!(
                            "Unknown MBR partition[{}] type: {:#x}",
                            i, partition.os_indicator
                        );
                    }
                }
            }

            match selected_range {
                Some(range) => Ok(Self { inner, range }),
                None => {
                    error!("Invalid MBR partition range.");
                    Err(DevError::Unsupported)
                }
            }
        } else {
            Err(DevError::Unsupported)
        }
    }
}

impl<T: BlockDriverOps> BaseDriverOps for MbrPartitionDev<T> {
    fn device_name(&self) -> &str {
        self.inner.device_name()
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}

impl<T: BlockDriverOps> MbrPartitionDev<T> {
    fn check_io_bounds(&self, block_id: u64, buf_len: usize) -> DevResult {
        let block_size = self.inner.block_size();
        if block_size == 0 || !buf_len.is_multiple_of(block_size) {
            return Err(DevError::InvalidParam);
        }

        let blocks = u64::try_from(buf_len / block_size).map_err(|_| DevError::BadState)?;
        let end_block = block_id.checked_add(blocks).ok_or(DevError::BadState)?;
        if end_block > self.range.num_blocks() {
            return Err(DevError::InvalidParam);
        }

        Ok(())
    }
}

impl<T: BlockDriverOps> BlockDriverOps for MbrPartitionDev<T> {
    fn num_blocks(&self) -> u64 {
        self.range.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.inner.block_size()
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        self.check_io_bounds(block_id, buf.len())?;
        self.inner
            .read_block(self.range.start().to_u64() + block_id, buf)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        self.check_io_bounds(block_id, buf.len())?;
        self.inner
            .write_block(self.range.start().to_u64() + block_id, buf)
    }

    fn flush(&mut self) -> DevResult {
        self.inner.flush()
    }
}
