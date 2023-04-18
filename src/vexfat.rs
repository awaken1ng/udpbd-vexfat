use std::io::{self, Read, Seek};

use vexfatbd::VirtualExFatBlockDevice;

use crate::{protocol::RDMA_MAX_PAYLOAD, Args};

pub struct VexFat {
    vexfat: VirtualExFatBlockDevice,
    sector_count: u32,
    pub block_shift: u8,
    pub block_size: u16,
    pub blocks_per_packet: u16,
    pub blocks_per_socket: u16,
}

impl VexFat {
    pub fn new(args: &Args) -> Self {
        let sector_size = 512;
        let sectors_per_cluster = 8;
        let cluster_bytes = sectors_per_cluster * sector_size;

        let file_size: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
        let file_size_mb = file_size / (1000 * 1000);
        let file_size_mib = file_size / (1024 * 1024);

        let cluster_count = file_size / cluster_bytes;
        let sector_count = cluster_count * sectors_per_cluster;

        let mut vexfat = vexfatbd::VirtualExFatBlockDevice::new(cluster_count as _);

        let prefix = match &args.prefix {
            Some(name) => vexfat.add_directory_in_root(name),
            None => vexfat.root_directory_first_cluster(),
        };

        // create default OPL folders
        vexfat.add_directory(prefix, "APPS");
        vexfat.add_directory(prefix, "ART");
        vexfat.add_directory(prefix, "CD");
        vexfat.add_directory(prefix, "CFG");
        vexfat.add_directory(prefix, "CHT");
        vexfat.add_directory(prefix, "LNG");
        vexfat.add_directory(prefix, "THM");
        vexfat.add_directory(prefix, "VMC");

        let dvd_cluster = vexfat.add_directory(prefix, "DVD");
        vexfat.add_file(dvd_cluster, &args.file);

        println!("Emulating block device");
        println!(" - size = {file_size_mb} MB / {file_size_mib} MiB");

        Self {
            vexfat,
            sector_count: sector_count as u32,
            block_shift: 0,
            block_size: 0,
            blocks_per_packet: 0,
            blocks_per_socket: 0,
        }
    }

    pub fn seek(&mut self, sector: u32) -> io::Result<()> {
        let offset = u64::from(sector) * u64::from(self.sector_size());

        self.vexfat
            .seek(std::io::SeekFrom::Start(offset))
            .map(|_| ())
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.vexfat.read_exact(buf).map(|_| ())
    }

    pub fn write(&mut self, _: &[u8]) -> io::Result<()> {
        // TODO
        Ok(())
    }

    pub fn sector_size(&self) -> u16 {
        512
    }

    pub fn sector_count(&self) -> u32 {
        self.sector_count
    }

    pub fn set_block_shift(&mut self, shift: u8) {
        if shift == self.block_shift {
            return;
        }

        self.block_shift = shift;
        self.block_size = 1 << (shift + 2);
        self.blocks_per_packet = RDMA_MAX_PAYLOAD as u16 / self.block_size;
        self.blocks_per_socket = self.sector_size() / self.block_size;
        println!("Block size changed to {}", self.block_size);
    }

    pub fn set_block_shift_sectors(&mut self, sectors: u16) {
        // Optimize for:
        // - the least number of network packets
        // - the largest block size (faster on the PS2)
        let size = u32::from(sectors) * u32::from(self.sector_size());
        let packets_min = (size + 1440 - 1) / 1440;
        let packets_128 = (size + 1408 - 1) / 1408;
        let packets_256 = (size + 1280 - 1) / 1280;
        let packets_512 = (size + 1024 - 1) / 1024;

        let shift = {
            if packets_512 == packets_min {
                7 // 512 byte blocks
            } else if packets_256 == packets_min {
                6 // 256 byte blocks
            } else if packets_128 == packets_min {
                5 // 128 byte blocks
            } else {
                3 //  32 byte blocks
            }
        };

        self.set_block_shift(shift);
    }
}
