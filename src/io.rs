use core::convert::TryFrom;
use acid_io::{Error, ErrorKind, Read, Seek, SeekFrom};
use uefi::proto::media::block::BlockIO;
use crate::error::UefiAcidioWrapper;
use crate::info;

pub struct BlockIoReader<'a> {
    inner: &'a BlockIO,
    media_id: u32,
    start_lba: u64,
    size: u64,
    cursor: u64,
    block_size: u64,
}

impl<'a> BlockIoReader<'a> {
    pub fn new(inner: &'a BlockIO, start_lba: u64, end_lba: u64) -> BlockIoReader<'a> {
        let block_size = inner.media().block_size().into();
        let start = start_lba * block_size;
        let end = (end_lba + 1) * block_size;
        BlockIoReader {
            inner,
            media_id: inner.media().media_id(),
            start_lba,
            size: end - start,
            cursor: 0,
            block_size,
        }
    }

    fn read_blocks(&self, buffer: &mut [u8]) -> acid_io::Result<()> {
        self.inner.read_blocks(self.media_id, self.start_lba + self.cursor / self.block_size, buffer)
            .map_err(|e| acid_io::Error::new(ErrorKind::Other, UefiAcidioWrapper(e, info!())))
    }
}

impl<'a> Read for BlockIoReader<'a> {
    fn read(&mut self, mut dst: &mut [u8]) -> acid_io::Result<usize> {
        if self.cursor >= self.size {
            return Ok(0);
        }
        let left = usize::try_from(self.size - self.cursor).unwrap_or(usize::MAX);
        if left < dst.len() {
            dst = &mut dst[..left];
        }

        assert!(self.block_size <= 4096);
        let block_size = self.block_size as usize;
        let mut block = [0u8; 4096];
        let block = &mut block[..block_size];
        let mut read = 0;

        // align our cursor
        let offset_in_lba = self.cursor % self.block_size;
        if offset_in_lba != 0 {
            self.read_blocks(block)?;
            let block = &block[offset_in_lba as usize..];

            if dst.len() <= block.len() {
                dst.copy_from_slice(&block[..dst.len()]);
                self.cursor += u64::try_from(dst.len()).unwrap();
                return Ok(dst.len());
            }

            dst[..block.len()].copy_from_slice(block);
            dst = &mut dst[block.len()..];
            read += block.len();
            self.cursor += u64::try_from(block.len()).unwrap();
        }

        // handle edge-case where we read less than 1 lba into dst
        if dst.len() < block_size {
            self.read_blocks(block)?;
            dst.copy_from_slice(&block[..dst.len()]);
            read += dst.len();
            self.cursor += u64::try_from(dst.len()).unwrap();
            return Ok(read);
        }

        // read full lbas
        let dst_len = dst.len();
        dst = &mut dst[..dst_len / block_size * block_size];
        self.read_blocks(dst)?;
        read += dst.len();
        self.cursor += u64::try_from(dst.len()).unwrap();
        Ok(read)
    }
}

impl<'a> Seek for BlockIoReader<'a> {
    fn seek(&mut self, pos: SeekFrom) -> acid_io::Result<u64> {
        self.cursor = match pos {
            SeekFrom::Start(offset) => Ok(offset),
            SeekFrom::End(offset) => u64::try_from((self.size as i64) + offset),
            SeekFrom::Current(offset) => u64::try_from((self.cursor as i64) + offset),
        }.map_err(|e| acid_io::Error::new(ErrorKind::InvalidInput, format!("seek before 0: {e}")))?;
        Ok(self.cursor)
    }
}

pub struct PartialReader<T: Read + Seek> {
    inner: T,
    start: u64,
    size: u64,
    cursor: u64,
    /// if we need to seek to 0 at first read
    reset: bool,
}

impl<T: Read + Seek> PartialReader<T> {
    pub fn new(inner: T, start: u64, size: u64) -> PartialReader<T> {
        PartialReader { inner, start, size, cursor: 0, reset: true }
    }
}

impl<T: Read + Seek> Read for PartialReader<T> {
    fn read(&mut self, mut dst: &mut [u8]) -> acid_io::Result<usize> {
        if self.reset {
            self.reset = false;
            self.seek(SeekFrom::Start(0))?;
        }
        if self.cursor >= self.size {
            return Ok(0)
        }
        let left = self.size - self.cursor;
        if dst.len() > usize::try_from(left).unwrap() {
            dst = &mut dst[..left as usize];
        }
        let read = self.inner.read(dst)?;
        self.cursor = self.cursor.checked_add(u64::try_from(read).unwrap()).unwrap();
        Ok(read)
    }
}
impl<T: Read + Seek> Seek for PartialReader<T> {
    fn seek(&mut self, pos: SeekFrom) -> acid_io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(pos) => self.inner.seek(SeekFrom::Start(self.start.checked_add(pos).unwrap()))?,
            SeekFrom::Current(pos) => self.inner.seek(SeekFrom::Current(pos))?,
            SeekFrom::End(pos) => self.inner.seek(SeekFrom::Start(u64::try_from(((self.start + self.size) as i64).checked_add(pos).unwrap()).unwrap()))?,
        };
        if self.start > pos {
            return Err(Error::new(ErrorKind::InvalidInput, "seek before start"));
        }
        self.cursor = pos - self.start;
        Ok(self.cursor)
    }
}
