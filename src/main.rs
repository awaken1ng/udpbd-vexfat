use std::{
    fs::File,
    io::{Read, Seek, Write, self},
    path::Path, net::{UdpSocket, SocketAddr, Ipv4Addr, IpAddr}, env, mem::size_of, process
};

use arbitrary_int::{u4, u9};
use anyhow::Context;

mod protocol;

use crate::protocol::{RDMA_MAX_PAYLOAD, UDPBD_PORT, InfoRequest, ReadWriteRequest, Rdma, InfoReply, WriteReply, Header, BlockType, UDP_MAX_PAYLOAD, Command};

struct BlockDevice {
    file: File,
    file_size: u64,
}

impl BlockDevice {
    fn new<P>(path: P) -> anyhow::Result<Self>
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
            file.context("Failed to open device in read-only mode")?
        };

        let file_size = file.seek(std::io::SeekFrom::End(0)).context("Failed to seek to the end of the device")?;
        let file_size_mb = file_size / (1000 * 1000);
        let file_size_mib = file_size / (1024 * 1024);
        file.seek(std::io::SeekFrom::Start(0)).context("Failed to seek back to the startof the device")?;

        println!("Opened {path:?} as block device");
        println!(" - {}", if read_only { "read-only" } else { "read/write" });
        println!(" - size = {file_size_mb} MB / {file_size_mib} MiB");

        Ok(Self {
            file,
            file_size,
        })
    }

    fn seek(&mut self, sector: u32) -> io::Result<()> {
        let offset = u64::from(sector) * u64::from(self.sector_size());

        self.file.seek(std::io::SeekFrom::Start(offset)).map(|_| ())
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact(buf)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write_all(buf)
    }

    fn sector_size(&self) -> u16 {
        512
    }

    fn sector_count(&self) -> u32 {
        (self.file_size / u64::from(self.sector_size())).try_into().unwrap()
    }
}

struct Server {
    block_device: BlockDevice,
    block_shift: u8,
    block_size: u16,
    blocks_per_packet: u16,
    blocks_per_socket: u16,
    socket: UdpSocket,
    write_size_left: usize,
    write_rdma_valid: bool,
}

impl Server {
    fn new(block_device: BlockDevice) -> anyhow::Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), UDPBD_PORT);
        let socket = UdpSocket::bind(addr).context("Failed to create UDP socket")?;
        socket.set_broadcast(true).context("Failed to enable broadcast on UDP socket")?;

        let mut server = Server {
            block_device,
            block_shift: 0,
            block_size: 0,
            blocks_per_packet: 0,
            blocks_per_socket: 0,
            socket,
            write_size_left: 0,
            write_rdma_valid: false,
        };
        server.set_block_shift(5); // 128b blocks

        Ok(server)
    }

    fn run(&mut self) {
        let mut buf = [0u8; UDP_MAX_PAYLOAD];
        println!("Server running on port {} (0x{:x})", UDPBD_PORT, UDPBD_PORT);

        loop {
            let (_, addr) = self.socket.recv_from(&mut buf[..]).unwrap();

            macro_rules! cast_buffer_as {
                ($type:ty) => {
                    bytemuck::from_bytes::<$type>(&buf[..size_of::<$type>()])
                };
            }

            let header = cast_buffer_as!(Header);
            match header.command() {
                Ok(cmd) => match cmd {
                    Command::Info => self.handle_cmd_info(cast_buffer_as!(InfoRequest), addr),
                    Command::Read => self.handle_cmd_read(cast_buffer_as!(ReadWriteRequest), addr),
                    Command::Write => self.handle_cmd_write(cast_buffer_as!(ReadWriteRequest)),
                    Command::WriteRdma => self.handle_cmd_write_rdma(cast_buffer_as!(Rdma), addr),
                    cmd => println!("Unexpected command: {cmd:?}")
                },
                Err(cmd) => println!("Unknown command: {cmd}"),
            };
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
        // - the least number of network packets
        // - the largest block size (faster on the PS2)
        let size = u32::from(sectors) * u32::from(self.block_device.sector_size());
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

    fn handle_cmd_info(&mut self, req: &InfoRequest, addr: SocketAddr) {
        println!("UDPBD_CMD_INFO from {addr}");

        let reply = InfoReply {
            header: Header::new_with_raw_value(0)
                .with_command(Command::InfoReply)
                .with_command_id(req.header.command_id())
                .with_command_pkt(1),
            sector_size: u32::from(self.block_device.sector_size()),
            sector_count: self.block_device.sector_count(),
        };
        let ser = bytemuck::bytes_of(&reply);

        if let Err(err) = self.socket.send_to(ser, addr) {
            eprintln!("Failed to reply with UDPBD_CMD_INFO_REPLY to {addr}: {err}");
        }
    }

    fn handle_cmd_read(&mut self, req: &ReadWriteRequest, addr: SocketAddr) {
        let ReadWriteRequest { sector_nr, sector_count, .. } = *req;

        println!("UDPBD_CMD_READ(cmdId={}, startSector={}, sectorCount={})", req.header.command_id(), sector_nr, sector_count);

        self.set_block_shift_sectors(sector_count);

        let mut reply = Rdma {
            header: Header::new_with_raw_value(0)
                .with_command(Command::ReadRdma)
                .with_command_id(req.header.command_id())
                .with_command_pkt(1),
            block_type: BlockType::new_with_raw_value(0)
                .with_block_shift(u4::new(self.block_shift)),
            data: [0; RDMA_MAX_PAYLOAD],
        };

        let mut seeked = true;
        if let Err(err) = self.block_device.seek(sector_nr) {
            eprintln!("Failed to seek block device in UDPBD_CMD_READ for {addr}: {err}");
            seeked = false;
        }

        let mut blocks_left = sector_count * self.blocks_per_socket;
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
            if seeked {
                if let Err(err) = self.block_device.read(buf) {
                    eprintln!("Failed to read block device in UDPBD_CMD_READ for {addr}, zeroing: {err}");
                    reply.data = [0; RDMA_MAX_PAYLOAD];
                }
            }

            let ser = bytemuck::bytes_of(&reply);
            let resp = &ser[..size_of::<Header>() + size_of::<BlockType>() + size];

            // send packet to PS2
            if let Err(err) = self.socket.send_to(resp, addr) {
                eprintln!("Failed to reply with UDPBD_CMD_READ_RDMA to {addr}: {err}");
            }

            let next_cmd_pkt = reply.header.command_pkt() + 1;
            reply.header = reply.header.with_command_pkt(next_cmd_pkt);
        }
    }

    fn handle_cmd_write(&mut self, req: &ReadWriteRequest) {
        let ReadWriteRequest { sector_nr, sector_count, .. } = *req;
        println!("UDPBD_CMD_WRITE(cmdId={}, startSector={}, sectorCount={})", req.header.command_id(), sector_nr, sector_count);

        self.write_size_left = usize::from(sector_count) * usize::from(self.block_device.sector_size());

        match self.block_device.seek(sector_nr) {
            Ok(_) => {
                self.write_rdma_valid = true;
            },
            Err(err) => {
                eprintln!("Failed to seek to sector {sector_nr}: {err}");
                self.write_rdma_valid = false;
            },
        }
    }

    fn handle_cmd_write_rdma(&mut self, req: &Rdma, addr: SocketAddr) {
        let size = req.block_type.blocks_size();
        let data = &req.data[..size];

        #[allow(clippy::collapsible_if)]
        if self.write_rdma_valid {
            if self.block_device.write(data).is_err() {
                eprintln!("Failed to write data to block device");
            }
        }

        match self.write_size_left.checked_sub(size) {
            Some(new_size) => self.write_size_left = new_size,
            None => {
                eprintln!("write_size_left wraparound at 0");
                self.write_size_left = 0;
            },
        }

        if self.write_size_left == 0 {
            let reply = WriteReply {
                header: Header::new_with_raw_value(0)
                    .with_command(Command::WriteDone)
                    .with_command_id(req.header.command_id())
                    .with_command_pkt(req.header.command_id().value() + 1), // ?
                result: 0,
            };
            let ser = bytemuck::bytes_of(&reply);

            if let Err(err) = self.socket.send_to(ser, addr) {
                eprintln!("Failed to reply with UDPBD_CMD_WRITE_DONE to {addr}: {err}");
            };
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args.len() < 2 {
        println!("Usage:");
        println!("  {} <file>", args[0]);
        process::exit(1);
    }

    let block_device = BlockDevice::new(&args[1]).unwrap();
    Server::new(block_device).unwrap().run();
}
