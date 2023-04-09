use arbitrary_int::{u19, u3, u4, u9};
use bitbybit::{bitenum, bitfield};
use bytemuck::{Pod, Zeroable};
use static_assertions::const_assert;
use std::mem::size_of;

pub const UDPBD_PORT: u16 = 0xBDBD;

#[derive(Debug)]
#[bitenum(u5, exhaustive: false)]
pub enum Command {
    Info      = 0x00, // client -> server
    InfoReply = 0x01, // server -> client
    Read      = 0x02, // client -> server
    ReadRdma  = 0x03, // server -> client
    Write     = 0x04, // client -> server
    WriteRdma = 0x05, // client -> server
    WriteDone = 0x06, // server -> client
}

// 2 bytes - Must be a "(multiple of 4) + 2" for RDMA on the PS2 !
#[bitfield(u16)]
#[repr(packed)]
#[derive(Zeroable, Pod)]
pub struct Header {
    #[bits(0..=4, rw)]
    pub command: Option<Command>, // 0.. 31 - command

    #[bits(5..=7, rw)]
    pub command_id: u3, // 0..  8 - increment with every new command sequence

    #[bits(8..=15, rw)]
    pub command_pkt: u8, // 0..255 - 0=request, 1 or more are response packets
}


// Info request. Can be a broadcast message to detect server on the network.
//
// Sequence of packets:
// - client: InfoRequest
// - server: InfoReply
#[repr(C)]
#[repr(packed)]
#[derive(Clone, Copy, Zeroable, Pod)]
pub struct InfoRequest {
    pub header: Header,
}

#[repr(C)]
#[repr(packed)]
#[derive(Clone, Copy, Zeroable, Pod)]
pub struct InfoReply {
    pub header: Header,
    pub sector_size: u32,
    pub sector_count: u32, // u32 here, but u16 in rw request
}

// Read request, sequence of packets:
// - client: ReadRequest
// - server: RDMA (1 or more packets)
//
// Write request, sequence of packets:
// - client: WriteRequest
// - client: RDMA (1 or more packets)
// - server: WriteDone
#[repr(C)]
#[repr(packed)]
#[derive(Clone, Copy, Zeroable, Pod)]
pub struct ReadWriteRequest {
    pub header: Header,
    pub sector_nr: u32,
    pub sector_count: u16,
}

#[repr(C)]
#[repr(packed)]
#[derive(Clone, Copy, Zeroable, Pod)]
pub struct WriteReply {
    pub header: Header,
    pub result: i32,
}

#[bitfield(u32)]
#[repr(packed)]
#[derive(Zeroable, Pod)]
pub struct BlockType {
    #[bits(0..=3, rw)]
    pub block_shift: u4, // 0..7: blocks_size = 1 << (block_shift+2); min=0=4bytes, max=7=512bytes

    #[bits(4..=12, rw)]
    pub block_count: u9, // 1..366 blocks

    #[bits(13..=31, r)]
    spare: u19,
}

impl BlockType {
    pub fn blocks_size(&self) -> u16 {
        let block_count = self.block_count().value();
        let block_shift = self.block_shift().value();

        block_count * (1 << (block_shift + 2))
    }
}

const_assert!(size_of::<Header>() == 2);
const_assert!(size_of::<InfoRequest>() == 2);
const_assert!(size_of::<InfoReply>() == 10);
const_assert!(size_of::<ReadWriteRequest>() == 8);
const_assert!(size_of::<WriteReply>() == 6);

const_assert!(size_of::<BlockType>() == 4);

/// Maximum payload for an RDMA packet depends on the used block size:
/// -   4 * 366 = 1464 bytes
/// -   8 * 183 = 1464 bytes
/// -  16 *  91 = 1456 bytes
/// -  32 *  45 = 1440 bytes
/// -  64 *  22 = 1408 bytes
/// - 128 *  11 = 1408 bytes <- default
/// - 256 *   5 = 1280 bytes
/// - 512 *   2 = 1024 bytes
pub const UDP_MAX_PAYLOAD: usize = 1472;
pub const RDMA_MAX_PAYLOAD: usize = UDP_MAX_PAYLOAD - size_of::<Header>() - size_of::<BlockType>();

/// Remote DMA (RDMA) packet
/// Used for transfering large blocks of data.
/// The heart of the protocol. Data must be a "(multiple of 4) + 2" for RDMA on the PS2 !
#[repr(C)]
#[repr(packed)]
#[derive(Clone, Copy, Zeroable, Pod)]
pub struct Rdma {
    pub header: Header,
    pub block_type: BlockType,
    pub data: [u8; RDMA_MAX_PAYLOAD],
}

const_assert!(size_of::<Rdma>() == UDP_MAX_PAYLOAD);
