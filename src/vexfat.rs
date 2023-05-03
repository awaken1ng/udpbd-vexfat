use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Seek},
};

use vexfatbd::VirtualExFatBlockDevice;
use walkdir::WalkDir;

use crate::{
    protocol::RDMA_MAX_PAYLOAD,
    utils::{relative_path_from_common_root, unsigned_align_to, unsigned_rounded_up_div},
    Args,
};

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
        let root: std::path::PathBuf = args.root.clone();
        let prefix = match &args.prefix {
            Some(name) => name.clone(),
            None => String::new(),
        };

        for name in [
            "APPS", "ART", "CD", "CFG", "DVD", "CHT", "LNG", "THM", "VMC",
        ] {
            let path = root.join(name);
            if path.exists() {
                continue;
            }

            println!("Creating {}", path.display());
            fs::create_dir(path).expect("failed to create default OPL directories");
        }

        let mut total_files_bytes = 0;
        let mut total_files_count = 0;
        let mut total_dirs_count = 0;
        let mut items = Vec::new();

        for entry in WalkDir::new(&args.root)
            .min_depth(1)
            .contents_first(false)
            .sort_by_file_name()
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    eprintln!("Failed to read entry: {err}");
                    continue;
                }
            };
            let path = entry.path();

            if path.is_file() {
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        eprintln!("Failed to read metadata: {err}");
                        continue;
                    }
                };

                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::MetadataExt;
                    total_files_bytes += metadata.size();
                }
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::fs::MetadataExt;
                    total_files_bytes += metadata.file_size();
                }

                total_files_count += 1;
            } else {
                total_dirs_count += 1;
            }

            items.push((path.to_owned(), path.is_file()));
        }

        let sector_size = 1 << BYTES_PER_SECTOR_SHIFT;
        let sectors_per_cluster_shift = 11; // 2048 sectors
        let sectors_per_cluster = 1 << sectors_per_cluster_shift;
        let bytes_per_cluster = sectors_per_cluster * sector_size;

        let cluster_count = unsigned_rounded_up_div(total_files_bytes, bytes_per_cluster)
            + (3 * (total_dirs_count + total_files_count));
        let cluster_count = unsigned_align_to(cluster_count, 2);
        let sector_count = cluster_count * sectors_per_cluster;

        let mut vexfat = vexfatbd::VirtualExFatBlockDevice::new(
            BYTES_PER_SECTOR_SHIFT,
            sectors_per_cluster_shift,
            cluster_count as _,
        )
        .unwrap();

        println!("Mapping files");

        let prefix_cluster = match &args.prefix {
            Some(name) => vexfat.add_directory_in_root(name).unwrap(),
            None => vexfat.root_directory_cluster(),
        };

        let mut dirpath_to_cluster = HashMap::from([(root.clone(), prefix_cluster)]);

        for (path, is_file) in items {
            let parent = path.parent().unwrap().to_owned();
            let parent_cluster = dirpath_to_cluster.get(&parent).cloned().unwrap();

            if is_file {
                if let Err(err) = vexfat.map_file(parent_cluster, &path) {
                    println!("! Failed to map file {}: {:?}", path.display(), err);
                }
            } else {
                let name: &str = path.file_name().unwrap().to_str().unwrap();

                match vexfat.add_directory(parent_cluster, name) {
                    Ok(dir_cluster) => {
                        dirpath_to_cluster.insert(path.to_owned(), dir_cluster);
                    }
                    Err(err) => {
                        println!("! Failed to map directory {}: {:?}", path.display(), err);
                    }
                }
            }

            let relative = relative_path_from_common_root(&root, &path);
            println!(" - ro:vexfat:{}/{}", prefix, relative.display());
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
