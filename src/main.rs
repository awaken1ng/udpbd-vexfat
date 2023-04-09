use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::Path, net::{UdpSocket, SocketAddr, Ipv4Addr, IpAddr}, env, mem::size_of
};

use arbitrary_int::{u5, u4, u9};
use protocol::{UDPBD_PORT, InfoRequest, ReadWriteRequest, Rdma, InfoReply, WriteReply, UDPBD_CMD_WRITE_DONE};

use crate::protocol::{RDMA_MAX_PAYLOAD, Header, UDPBD_CMD_READ_RDMA, UDPBD_CMD_INFO_REPLY, BlockType, UDP_MAX_PAYLOAD};

mod protocol;

struct BlockDevice {
    file: File,
    file_size: u64,
}

impl BlockDevice {
    fn new<P>(path: P) -> Self
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();

        let mut read_only = false;
        let mut options = File::options();
        options.read(true);
        options.write(true);

        let mut file = {
            let mut file = options.open(path);
            if file.is_err() {
                read_only = true;
                options.write(false);
                file = options.open(path);
            }
            file.unwrap()
        };

        let file_size = file.seek(std::io::SeekFrom::End(0)).unwrap();
        let file_size_mb = file_size / (1000 * 1000);
        let file_size_mib = file_size / (1024 * 1024);
        file.seek(std::io::SeekFrom::Start(0)).unwrap();

        println!("Opened {path:?} as block device");
        println!(" - {}", if read_only { "read-only" } else { "read/write" });
        println!(" - size = {file_size_mb} MB / {file_size_mib} MiB");

        Self {
            file,
            file_size,
        }
    }

    fn seek(&mut self, sector: u32) {
        let offset = u64::from(sector) * 512;

        self.file.seek(std::io::SeekFrom::Start(offset)).unwrap();
    }

    fn read(&mut self, buf: &mut [u8]) {
        self.file.read_exact(buf).unwrap();
    }

    fn write(&mut self, buf: &[u8]) {
        self.file.write_all(buf).unwrap();
    }

    fn sector_size(&self) -> u16 { 512 }
    fn sector_count(&self) -> u32 { (self.file_size / 512).try_into().unwrap() }
}

struct Server {
    block_device: BlockDevice,
    block_shift: u8,
    block_size: u16,
    blocks_per_packet: u16,
    blocks_per_socket: u16,
    socket: UdpSocket,

    seq: usize,

    write_size_left: u32,
}

impl Server {
    fn new(block_device: BlockDevice) -> Self {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), UDPBD_PORT);
        let socket = UdpSocket::bind(addr).unwrap();
        socket.set_broadcast(true).unwrap();

        let mut server = Server {
            block_device,
            block_shift: 0,
            block_size: 0,
            blocks_per_packet: 0,
            blocks_per_socket: 0,
            socket,
            write_size_left: 0,

            seq: 0,
        };
        server.set_block_shift(5); // 128b blocks

        server
    }

    fn run(&mut self) {
        let mut buf = [0u8; UDP_MAX_PAYLOAD];
        println!("Server running on port {} (0x{:x})", UDPBD_PORT, UDPBD_PORT);

        loop {
            let (_, addr) = self.socket.recv_from(&mut buf[..]).unwrap();

            let header: &Header = bytemuck::from_bytes(&buf[..size_of::<Header>()]);
            let command = header.command().value();

            #[allow(clippy::match_single_binding)]
            let trimmed_buf = match command {
                0x0 => {
                    self.handle_cmd_info(addr, bytemuck::from_bytes(&buf[..size_of::<InfoRequest>()]));
                    &buf[..size_of::<InfoRequest>()]
                },
                0x2 => {
                    self.handle_cmd_read(addr, bytemuck::from_bytes(&buf[..size_of::<ReadWriteRequest>()]));
                    &buf[..size_of::<ReadWriteRequest>()]
                },
                0x4 => {
                    self.handle_cmd_write(bytemuck::from_bytes(&buf[..size_of::<ReadWriteRequest>()]));
                    &buf[..size_of::<ReadWriteRequest>()]
                },
                0x5 => {
                    self.handle_cmd_write_rdma(addr, bytemuck::from_bytes(&buf));
                    &buf
                },
                _ => {
                    println!("Invalid command: {command}");
                    continue;
                }
            };

            let filepath = format!("/tmp/dump/req-{}", self.seq);
            let mut out = std::fs::File::options().create(true).write(true).open(filepath).unwrap();
            out.write_all(trimmed_buf).unwrap();
            self.seq += 1;
        }
    }

    fn set_block_shift(&mut self, shift: u8) {
        if shift == self.block_shift { return }

        self.block_shift = shift;
        self.block_size = 1 << (shift + 2);
        self.blocks_per_packet = RDMA_MAX_PAYLOAD as u16 / self.block_size;
        self.blocks_per_socket = self.block_device.sector_size() / self.block_size;
        println!("Block size changed to {}", self.block_size);
    }

    fn set_block_shift_sectors(&mut self, sectors: u16) {
        // Optimize for:
        // 1 - the least number of network packets
        // 2 - the largest block size (faster on the ps2)
        let size = u32::from(sectors) * 512;
        let packets_min  = (size + 1440 - 1) / 1440;
        let packets_128 = (size + 1408 - 1) / 1408;
        let packets_256 = (size + 1280 - 1) / 1280;
        let packets_512 = (size + 1024 - 1) / 1024;

        let shift = {
            if packets_512 == packets_min {
                7 // 512 byte blocks
            }
            else if packets_256 == packets_min {
                6 // 256 byte blocks
            }
            else if packets_128 == packets_min {
                5 // 128 byte blocks
            }
            else {
                3 //  32 byte blocks
            }
        };

        self.set_block_shift(shift);
    }

    fn handle_cmd_info(&mut self, addr: SocketAddr, req: &InfoRequest) {
        println!("UDPBD_CMD_INFO from {addr}");

        let reply = InfoReply {
            header: Header::new_with_raw_value(0)
                .with_command(u5::new(UDPBD_CMD_INFO_REPLY))
                .with_command_id(req.header.command_id())
                .with_command_pkt(1),
            sector_size: u32::from(self.block_device.sector_size()),
            sector_count: self.block_device.sector_count(),
        };
        let ser = bytemuck::bytes_of(&reply);

        let mut out = std::fs::File::options().create(true).write(true).open(format!("/tmp/dump/resp-{}", self.seq)).unwrap();
        out.write_all(ser).unwrap();

        self.socket.send_to(ser, addr).unwrap();
    }

    fn handle_cmd_read(&mut self, addr: SocketAddr, req: &ReadWriteRequest) {
        let sector_nr = req.sector_nr;
        let sector_count = req.sector_count;

        println!("UDPBD_CMD_READ(cmdId={}, startSector={}, sectorCount={})", req.header.command_id(), sector_nr, sector_count);

        self.set_block_shift_sectors(sector_count);

        let mut reply = Rdma {
            header: Header::new_with_raw_value(0)
                .with_command(u5::new(UDPBD_CMD_READ_RDMA))
                .with_command_id(req.header.command_id())
                .with_command_pkt(1),
            block_type: BlockType::new_with_raw_value(0)
                .with_block_shift(u4::new(self.block_shift)),
            data: [0; RDMA_MAX_PAYLOAD],
        };

        let mut times = 0;
        let mut blocks_left = sector_count * self.blocks_per_socket;

        self.block_device.seek(sector_nr);

        while blocks_left > 0 {
            let block_count = if blocks_left > self.blocks_per_packet {
                self.blocks_per_packet
            } else {
                blocks_left
            };
            reply.block_type = reply.block_type.with_block_count(u9::new(block_count));
            blocks_left -= block_count;

            // read data from file
            let size = usize::from(block_count * self.block_size);
            let buf = &mut reply.data[..size];
            self.block_device.read(buf);

            let ser = bytemuck::bytes_of(&reply);
            let resp = &ser[..size_of::<Header>() + size_of::<BlockType>() + size];

            let mut out = std::fs::File::options().create(true).write(true).open(format!("/tmp/dump/resp-{}-{}", self.seq, times)).unwrap();
            out.write_all(resp).unwrap();
            times += 1;

            // send packet to PS2
            self.socket.send_to(resp, addr).unwrap();

            reply.header = reply.header.with_command_pkt(reply.header.command_pkt() + 1);
        }
    }

    fn handle_cmd_write(&mut self, req: &ReadWriteRequest) {
        let sector_nr = req.sector_nr;
        let sector_count = req.sector_count;
        println!("UDPBD_CMD_WRITE(cmdId={}, startSector={}, sectorCount={})", req.header.command_id(), sector_nr, sector_count);

        self.block_device.seek(sector_nr);
        self.write_size_left = u32::from(sector_count) * 512;
    }

    fn handle_cmd_write_rdma(&mut self, addr: SocketAddr, req: &Rdma) {
        let block_count = req.block_type.block_count().value();
        let block_shift = req.block_type.block_shift().value();
        let size = block_count * (1 << (block_shift + 2));
        let data = &req.data[..usize::from(size)];

        self.block_device.write(data);
        self.write_size_left -= u32::from(size);
        if self.write_size_left == 0 {
            let reply = WriteReply {
                header: Header::new_with_raw_value(0)
                    .with_command(u5::new(UDPBD_CMD_WRITE_DONE))
                    .with_command_id(req.header.command_id())
                    .with_command_pkt(req.header.command_id().value() + 1), // ?
                result: 0,
            };
            let ser = bytemuck::bytes_of(&reply);

            let mut out = std::fs::File::options().create(true).write(true).open(format!("/tmp/dump/resp-{}", self.seq)).unwrap();
            out.write_all(ser).unwrap();

            self.socket.send_to(ser, addr).unwrap();

        }
    }
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args.len() < 2 {
        println!("Usage:");
        println!("  {} <file>", args[0]);
        return
    }

    let bd = BlockDevice::new(&args[1]);
    let mut server = Server::new(bd);
    server.run();
}
