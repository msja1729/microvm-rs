//! Pure, guest-facing request parsers: descriptor chain → typed request.
//!
//! No I/O happens here. A parser either returns a request whose every guest range is
//! inside guest RAM and whose disk range is inside the disk, or an error. It reads
//! guest memory only for request headers, each exactly once.

use virtio_bindings::virtio_blk::{VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT};

use super::queue::{range_in_memory, ChainDesc, GuestMem};

/// Bytes per virtio-blk sector, independent of the disk's real block size.
pub const SECTOR_SIZE: u64 = 512;
/// Size of `struct virtio_blk_outhdr`: `type: u32, reserved: u32, sector: u64`.
pub const BLK_HEADER_SIZE: u32 = 16;

/// What a virtio-blk request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkRequestType {
    /// Read from the disk into the data buffers.
    In,
    /// Write the data buffers to the disk.
    Out,
    /// Make earlier writes durable.
    Flush,
}

/// One guest buffer of a request, checked to lie inside guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRange {
    pub addr: u64,
    pub len: u32,
}

/// A virtio-blk request that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlkRequest {
    pub kind: BlkRequestType,
    /// Byte offset on the disk (`sector * 512`). 0 for `Flush`.
    pub offset: u64,
    /// Data buffers in chain order: device-writable for `In`, device-readable for
    /// `Out`, empty for `Flush`. `offset + total length <= disk size`.
    pub data: Vec<DataRange>,
    /// Guest address of the 1-byte status the device writes last.
    pub status_addr: u64,
}

impl BlkRequest {
    /// Sum of the data buffer lengths. Cannot overflow: the parser bounded it by the
    /// disk size, which is a `u64`.
    pub fn data_len(&self) -> u64 {
        self.data.iter().map(|d| u64::from(d.len)).sum()
    }
}

/// Why a virtio-blk request was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkError {
    /// Fewer than two descriptors (a request needs a header and a status byte).
    TooShort,
    /// The first descriptor is device-writable, shorter than 16 bytes, or outside RAM.
    BadHeader,
    /// The last descriptor is not a device-writable byte inside RAM.
    BadStatus,
    /// A data buffer has the wrong direction for the request type, or `Flush` has data.
    BadDirection,
    /// A data buffer lies outside guest RAM.
    OutsideMemory,
    /// The data length is not a multiple of 512 bytes.
    BadLength,
    /// The request reaches past the end of the disk (or its arithmetic overflows).
    OutOfRange,
    /// The request type (e.g. `GET_ID`) is not supported; answered with `UNSUPP`.
    Unsupported(u32),
}

/// Where the device should write the status byte of `chain`, if it has a usable one:
/// the last descriptor, device-writable, at least 1 byte, inside RAM. Used to report
/// errors for chains that failed to parse.
pub fn blk_status_addr<M: GuestMem + ?Sized>(chain: &[ChainDesc], mem: &M) -> Option<u64> {
    let last = chain.last()?;
    (chain.len() >= 2
        && last.writable
        && last.len >= 1
        && range_in_memory(mem.size(), last.addr, 1))
    .then_some(last.addr)
}

/// Parse a virtio-blk descriptor chain for a disk of `capacity_sectors` sectors.
///
/// Layout: descriptor 0 is the device-readable 16-byte header, the last descriptor is
/// the device-writable status byte, and everything between is data.
pub fn parse_blk_request<M: GuestMem + ?Sized>(
    chain: &[ChainDesc],
    mem: &M,
    capacity_sectors: u64,
) -> Result<BlkRequest, BlkError> {
    let mem_size = mem.size();
    let (header, rest) = chain.split_first().ok_or(BlkError::TooShort)?;
    let (status, data) = rest.split_last().ok_or(BlkError::TooShort)?;

    if header.writable
        || header.len < BLK_HEADER_SIZE
        || !range_in_memory(mem_size, header.addr, u64::from(BLK_HEADER_SIZE))
    {
        return Err(BlkError::BadHeader);
    }
    if !status.writable || status.len < 1 || !range_in_memory(mem_size, status.addr, 1) {
        return Err(BlkError::BadStatus);
    }

    // Read the header once; `raw` is our private copy from here on.
    let mut raw = [0u8; BLK_HEADER_SIZE as usize];
    mem.read(header.addr, &mut raw)
        .map_err(|_| BlkError::BadHeader)?;
    let [t0, t1, t2, t3, _, _, _, _, s0, s1, s2, s3, s4, s5, s6, s7] = raw;
    let request_type = u32::from_le_bytes([t0, t1, t2, t3]);
    let sector = u64::from_le_bytes([s0, s1, s2, s3, s4, s5, s6, s7]);

    let kind = match request_type {
        VIRTIO_BLK_T_IN => BlkRequestType::In,
        VIRTIO_BLK_T_OUT => BlkRequestType::Out,
        VIRTIO_BLK_T_FLUSH => BlkRequestType::Flush,
        other => return Err(BlkError::Unsupported(other)),
    };

    let mut ranges = Vec::with_capacity(data.len());
    let mut total: u64 = 0;
    for desc in data {
        let direction_ok = match kind {
            BlkRequestType::In => desc.writable,
            BlkRequestType::Out => !desc.writable,
            BlkRequestType::Flush => false,
        };
        if !direction_ok {
            return Err(BlkError::BadDirection);
        }
        if !range_in_memory(mem_size, desc.addr, u64::from(desc.len)) {
            return Err(BlkError::OutsideMemory);
        }
        total = total
            .checked_add(u64::from(desc.len))
            .ok_or(BlkError::OutOfRange)?;
        ranges.push(DataRange {
            addr: desc.addr,
            len: desc.len,
        });
    }

    let offset = match kind {
        BlkRequestType::Flush => 0,
        BlkRequestType::In | BlkRequestType::Out => {
            if !total.is_multiple_of(SECTOR_SIZE) {
                return Err(BlkError::BadLength);
            }
            let offset = sector
                .checked_mul(SECTOR_SIZE)
                .ok_or(BlkError::OutOfRange)?;
            let end = offset.checked_add(total).ok_or(BlkError::OutOfRange)?;
            // A capacity whose byte size exceeds u64 bounds nothing a u64 end can reach.
            let disk_bytes = capacity_sectors.saturating_mul(SECTOR_SIZE);
            if end > disk_bytes {
                return Err(BlkError::OutOfRange);
            }
            offset
        }
    };

    Ok(BlkRequest {
        kind,
        offset,
        data: ranges,
        status_addr: status.addr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::VecMem;
    use virtio_bindings::virtio_blk::VIRTIO_BLK_T_GET_ID;

    const HDR: u64 = 0x100;
    const DATA: u64 = 0x1000;
    const STATUS: u64 = 0x200;
    const MEM: usize = 0x4000;
    const CAPACITY: u64 = 8; // 4 KiB disk

    fn mem_with_header(request_type: u32, sector: u64) -> VecMem {
        let mem = VecMem::new(MEM);
        mem.write(HDR, &request_type.to_le_bytes())
            .expect("in range");
        mem.write(HDR + 8, &sector.to_le_bytes()).expect("in range");
        mem
    }

    fn d(addr: u64, len: u32, writable: bool) -> ChainDesc {
        ChainDesc {
            addr,
            len,
            writable,
        }
    }

    fn chain(data: &[ChainDesc]) -> Vec<ChainDesc> {
        let mut c = vec![d(HDR, 16, false)];
        c.extend_from_slice(data);
        c.push(d(STATUS, 1, true));
        c
    }

    #[test]
    fn parses_in_out_and_flush() {
        let mem = mem_with_header(VIRTIO_BLK_T_IN, 2);
        let req =
            parse_blk_request(&chain(&[d(DATA, 1024, true)]), &mem, CAPACITY).expect("valid IN");
        assert_eq!(req.kind, BlkRequestType::In);
        assert_eq!(req.offset, 1024);
        assert_eq!(
            req.data,
            vec![DataRange {
                addr: DATA,
                len: 1024
            }]
        );
        assert_eq!(req.status_addr, STATUS);

        let mem = mem_with_header(VIRTIO_BLK_T_OUT, 7);
        let c = chain(&[d(DATA, 256, false), d(DATA + 256, 256, false)]);
        let req = parse_blk_request(&c, &mem, CAPACITY).expect("valid OUT");
        assert_eq!(req.kind, BlkRequestType::Out);
        assert_eq!(req.offset, 7 * 512);
        assert_eq!(req.data_len(), 512);

        let mem = mem_with_header(VIRTIO_BLK_T_FLUSH, 12345);
        let req = parse_blk_request(&chain(&[]), &mem, CAPACITY).expect("valid FLUSH");
        assert_eq!(req.kind, BlkRequestType::Flush);
        assert!(req.data.is_empty());
    }

    #[test]
    fn get_id_and_unknown_types_are_unsupported() {
        for t in [VIRTIO_BLK_T_GET_ID, 3, 0xdead_beef] {
            let mem = mem_with_header(t, 0);
            let c = chain(&[d(DATA, 20, true)]);
            assert_eq!(
                parse_blk_request(&c, &mem, CAPACITY),
                Err(BlkError::Unsupported(t))
            );
            assert_eq!(blk_status_addr(&c, &mem), Some(STATUS));
        }
    }

    #[test]
    fn rejects_out_of_range_and_overflow() {
        // Last sector is 7; reading sectors 7..9 runs past the end.
        let mem = mem_with_header(VIRTIO_BLK_T_IN, 7);
        let c = chain(&[d(DATA, 1024, true)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::OutOfRange)
        );
        // sector * 512 overflows u64.
        let mem = mem_with_header(VIRTIO_BLK_T_IN, u64::MAX / 256);
        let c = chain(&[d(DATA, 512, true)]);
        assert_eq!(
            parse_blk_request(&c, &mem, u64::MAX),
            Err(BlkError::OutOfRange)
        );
        // offset + length overflows u64.
        let mem = mem_with_header(VIRTIO_BLK_T_IN, u64::MAX / 512);
        assert_eq!(
            parse_blk_request(&c, &mem, u64::MAX),
            Err(BlkError::OutOfRange)
        );
    }

    #[test]
    fn rejects_partial_sectors() {
        let mem = mem_with_header(VIRTIO_BLK_T_OUT, 0);
        let c = chain(&[d(DATA, 100, false)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadLength)
        );
    }

    #[test]
    fn rejects_bad_direction_flags() {
        let mem = mem_with_header(VIRTIO_BLK_T_IN, 0);
        let c = chain(&[d(DATA, 512, false)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadDirection)
        );
        let mem = mem_with_header(VIRTIO_BLK_T_OUT, 0);
        let c = chain(&[d(DATA, 512, true)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadDirection)
        );
        let mem = mem_with_header(VIRTIO_BLK_T_FLUSH, 0);
        let c = chain(&[d(DATA, 512, false)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadDirection)
        );
        // A writable header.
        let mut c = chain(&[]);
        c[0].writable = true;
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadHeader)
        );
    }

    #[test]
    fn rejects_missing_or_bad_status() {
        let mem = mem_with_header(VIRTIO_BLK_T_IN, 0);
        let only_header = [d(HDR, 16, false)];
        assert_eq!(
            parse_blk_request(&only_header, &mem, CAPACITY),
            Err(BlkError::TooShort)
        );
        assert_eq!(
            parse_blk_request(&[], &mem, CAPACITY),
            Err(BlkError::TooShort)
        );
        assert_eq!(blk_status_addr(&only_header, &mem), None);

        // Last descriptor is device-readable: it cannot be the status byte.
        let c = [d(HDR, 16, false), d(DATA, 512, true), d(STATUS, 1, false)];
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadStatus)
        );
        assert_eq!(blk_status_addr(&c, &mem), None);

        // Status byte past the end of RAM.
        let c = [d(HDR, 16, false), d(MEM as u64, 1, true)];
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadStatus)
        );
        assert_eq!(blk_status_addr(&c, &mem), None);
    }

    #[test]
    fn rejects_short_header_and_ranges_outside_memory() {
        let mem = mem_with_header(VIRTIO_BLK_T_IN, 0);
        let mut c = chain(&[]);
        c[0].len = 15;
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::BadHeader)
        );

        let c = chain(&[d(MEM as u64 - 256, 512, true)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::OutsideMemory)
        );
        let c = chain(&[d(u64::MAX - 100, 512, true)]);
        assert_eq!(
            parse_blk_request(&c, &mem, CAPACITY),
            Err(BlkError::OutsideMemory)
        );
    }
}
