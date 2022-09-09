use core::convert::TryFrom;
use core::fmt::{Display, Formatter};
use acid_io::{Error, ErrorKind, Read, Seek, SeekFrom};
#[cfg(target_os = "uefi")] use uefi::proto::media::block::BlockIO;

#[cfg(target_os = "uefi")]
pub struct BlockIoReader<'a> {
    inner: &'a BlockIO,
    media_id: u32,
    start_lba: u64,
    size: u64,
    cursor: u64,
    block_size: u64,
}

#[cfg(target_os = "uefi")]
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
        let lba = self.start_lba + self.cursor / self.block_size;
        self.inner.read_blocks(self.media_id, lba, buffer)
            .map_err(|e| acid_io::Error::new(ErrorKind::Other, crate::error::Error::new_from_uefi(e, format!("can't read BlockIO LBAs starting from {lba}; number of blocks: {}", buffer.len() as u64 / self.block_size))))
    }
}
#[cfg(target_os = "uefi")]
impl<'a> Read for BlockIoReader<'a> {
    fn read(&mut self, mut dst: &mut [u8]) -> acid_io::Result<usize> {
        if self.cursor >= self.size {
            return Ok(0);
        }
        let left = usize::try_from(self.size - self.cursor).unwrap_or(usize::MAX);
        if left < dst.len() {
            dst = &mut dst[..left];
        }

        assert!(self.block_size <= 4096, "block_size <= 4096; reported block_size: {}", self.block_size);
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
#[cfg(target_os = "uefi")]
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
    need_reset: bool,
}

impl<T: Read + Seek> PartialReader<T> {
    pub fn new(inner: T, start: u64, size: u64) -> PartialReader<T> {
        PartialReader { inner, start, size, cursor: 0, need_reset: true }
    }
}

impl<T: Read + Seek> Read for PartialReader<T> {
    fn read(&mut self, mut dst: &mut [u8]) -> acid_io::Result<usize> {
        if self.need_reset {
            self.need_reset = false;
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
        self.need_reset = false;
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

/// Length of the inner Read must not change
pub struct OptimizedSeek<T> {
    inner: T,
    cursor: Option<u64>,
    end: Option<u64>,
    total_seeks: u64,
    unstopped_seeks: u64,
}

impl<T> OptimizedSeek<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, cursor: None, end: None, total_seeks: 0, unstopped_seeks: 0 }
    }
    pub fn total_seeks(&self) -> u64 {
        self.total_seeks
    }
    pub fn stopped_seeks(&self) -> u64 {
        self.total_seeks - self.unstopped_seeks
    }
}

impl<T: Read> Read for OptimizedSeek<T> {
    fn read(&mut self, buf: &mut [u8]) -> acid_io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(cursor) = &mut self.cursor {
            *cursor += u64::try_from(read).unwrap();
        }
        Ok(read)
    }
}
impl<T: Seek> Seek for OptimizedSeek<T> {
    fn seek(&mut self, pos: SeekFrom) -> acid_io::Result<u64> {
        self.total_seeks += 1;
        let add_end_offset = |end, offset| {
            i64::try_from(end).ok()
                .and_then(|end| end.checked_sub(offset))
                .and_then(|pos| u64::try_from(pos).ok())
        };
        match (self.cursor, self.end, pos) {
            (Some(cursor), _, SeekFrom::Start(pos)) if pos == cursor => Ok(cursor),
            (Some(cursor), _, SeekFrom::Current(0)) => Ok(cursor),
            (Some(cursor), Some(end), SeekFrom::End(offset))
                if Some(cursor) == add_end_offset(end, offset) => Ok(cursor),
            (_, _, pos) => {
                self.unstopped_seeks += 1;
                let new_pos = self.inner.seek(pos.clone())?;
                self.cursor = Some(new_pos);
                match pos {
                    SeekFrom::End(offset) if self.end.is_none() => {
                            self.end = offset.checked_neg()
                                .and_then(|offset| add_end_offset(new_pos, offset));
                    },
                    _ => (),
                }
                Ok(new_pos)
            },
        }
    }
}

// compat
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

#[derive(Debug)]
pub struct ErrorWrapper(acid_io::Error);
impl fatfs::IoError for ErrorWrapper {
    fn is_interrupted(&self) -> bool {
        self.0.kind() == ErrorKind::Interrupted
    }
    fn new_unexpected_eof_error() -> Self {
        ErrorWrapper(acid_io::Error::new(acid_io::ErrorKind::UnexpectedEof, "failed to fill whole buffer"))
    }

    fn new_write_zero_error() -> Self {
        ErrorWrapper(acid_io::Error::new(acid_io::ErrorKind::WriteZero, "failed to write whole buffer"))
    }
}

impl Display for ErrorWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

pub struct IgnoreWriteWrapper<T>(T);
impl<T> IgnoreWriteWrapper<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }
}
impl<T: Read> fatfs::Read for IgnoreWriteWrapper<T> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, ErrorWrapper> {
        self.0.read(buf).map_err(ErrorWrapper)
    }
}
impl<T: Seek> fatfs::Seek for IgnoreWriteWrapper<T> {
    fn seek(&mut self, pos: fatfs::SeekFrom) -> core::result::Result<u64, ErrorWrapper> {
        let pos = match pos {
            fatfs::SeekFrom::Start(pos) => SeekFrom::Start(pos),
            fatfs::SeekFrom::Current(pos) => SeekFrom::Current(pos),
            fatfs::SeekFrom::End(pos) => SeekFrom::End(pos),
        };
        self.0.seek(pos).map_err(ErrorWrapper)
    }
}
impl<T> fatfs::IoBase for IgnoreWriteWrapper<T> { type Error = ErrorWrapper; }
impl<T> fatfs::Write for IgnoreWriteWrapper<T> {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, ErrorWrapper> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> core::result::Result<(), ErrorWrapper> {
        Ok(())
    }
}

