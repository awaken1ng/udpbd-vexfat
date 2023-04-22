use std::{io::{self, Read, Seek}, os::unix::prelude::MetadataExt};

use vexfatbd::VirtualExFatBlockDevice;
use walkdir::WalkDir;

use crate::{protocol::RDMA_MAX_PAYLOAD, Args, utils::{unsigned_rounded_up_div, unsigned_align_to}};

const BYTES_PER_SECTOR_SHIFT: u8 = 9; // 512 bytes

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
        let mut files = Vec::new();

        let mut total_file_size = 0;

        for entry in WalkDir::new(&args.path).min_depth(1).max_depth(2) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    eprintln!("Failed to read entry: {err}");
                    continue;
                },
            };

            if !entry.path().is_file() {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    eprintln!("Failed to read metadata: {err}");
                    continue;
                },
            };

            total_file_size += metadata.size();

            files.push(entry.path().to_owned());
        }

        total_file_size += 1024 * 1024 * 1024; // add 1 GiB for directories and what not

        let sector_size = 1 << BYTES_PER_SECTOR_SHIFT;
        let sectors_per_cluster_shift = 11; // 2048 sectors
        let sectors_per_cluster = 1 << sectors_per_cluster_shift;
        let bytes_per_cluster = sectors_per_cluster * sector_size;

        let cluster_count = unsigned_rounded_up_div(total_file_size, bytes_per_cluster);
        let cluster_count = unsigned_align_to(cluster_count, 2);
        let sector_count = cluster_count * sectors_per_cluster;

        let mut vexfat = vexfatbd::VirtualExFatBlockDevice::new(BYTES_PER_SECTOR_SHIFT, sectors_per_cluster_shift, cluster_count as _).unwrap();

        let (prefix, prefix_path_component) = match &args.prefix {
            Some(name) => (vexfat.add_directory_in_root(name).unwrap(), format!("{name}/")),
            None => (vexfat.root_directory_cluster(), String::from("/")),
        };

        // create default OPL folders
        vexfat.add_directory(prefix, "APPS").unwrap();
        vexfat.add_directory(prefix, "ART").unwrap();
        vexfat.add_directory(prefix, "CD").unwrap();
        vexfat.add_directory(prefix, "CFG").unwrap();
        vexfat.add_directory(prefix, "CHT").unwrap();
        vexfat.add_directory(prefix, "LNG").unwrap();
        vexfat.add_directory(prefix, "THM").unwrap();
        vexfat.add_directory(prefix, "VMC").unwrap();
        let dvd = vexfat.add_directory(prefix, "DVD").unwrap();

        println!("Mapping files");
        for file in files {
            let file_name = file.file_name().unwrap_or_default().to_string_lossy();
            match vexfat.map_file(dvd, &file) {
                Ok(_) => println!("- vexfat:/{prefix_path_component}DVD/{}", file_name),
                Err(err) => println!("! Failed to map {}: {:?}", file.display(), err),
            }
        }

        println!("Emulating read-only exFAT block device");
        println!(" - size = {} MiB", vexfat.volume_size() / 1024 / 1024);

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
        self.vexfat.bytes_per_sector()
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
