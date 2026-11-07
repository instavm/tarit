//! Sparse RAM extents for full snapshots.
//!
//! A full snapshot (`VMSN`) stores packed guest RAM as one extent of `mem_len`
//! bytes whose CRC covers every byte. That format is unchanged; only the
//! storage of the extent is. Ranges that hold no guest data are not written:
//! unplugged virtio-mem blocks (named by the caller and never read) and runs
//! of all-zero pages of at least [`MIN_HOLE_BYTES`]. The file is extended over
//! them, so they are holes that read back as zeros, and the CRC is computed as
//! if zeros had been written there.
//!
//! A reader that reads the extent byte for byte, including an older VMM,
//! therefore verifies and restores these files unchanged. The readers here
//! skip holes with `SEEK_DATA`/`SEEK_HOLE`: the eager loader leaves fresh
//! guest memory untouched (unallocated) there, and checksums account for
//! holes arithmetically instead of reading zeros. Turning data into a hole or
//! writing data into a hole still changes the logical bytes and fails the CRC.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// Granularity of zero detection.
pub const PAGE_SIZE: u64 = 4096;

/// Shortest run of all-zero pages stored as a hole. Shorter runs are written:
/// a hole per stray zero page would cost a write call and a file extent each.
/// Caller-named ranges (unplugged memory) are holes at any length.
pub const MIN_HOLE_BYTES: u64 = 64 * 1024;

const ZERO_BLOCK_BYTES: usize = 64 * 1024;
static ZERO_BLOCK: [u8; ZERO_BLOCK_BYTES] = [0; ZERO_BLOCK_BYTES];

/// Whether every byte of `bytes` is zero.
pub fn is_zero(bytes: &[u8]) -> bool {
    // OR-reduce fixed blocks (vectorizes) but stop at the first non-zero one.
    let (blocks, rest) = bytes.as_chunks::<64>();
    blocks
        .iter()
        .all(|block| block.iter().fold(0u8, |acc, byte| acc | byte) == 0)
        && rest.iter().all(|byte| *byte == 0)
}

/// Feed `len` zero bytes to `hasher` without touching `len` bytes: CRC-32
/// combination doubles a zero block, so a hole of any size costs O(log len).
pub fn crc32_update_zeros(hasher: &mut crc32fast::Hasher, len: u64) {
    let block_len = ZERO_BLOCK_BYTES as u64;
    let mut blocks = len / block_len;
    if blocks > 0 {
        let mut block = crc32fast::Hasher::new();
        block.update(&ZERO_BLOCK);
        let mut zeros = crc32fast::Hasher::new();
        loop {
            if blocks & 1 == 1 {
                zeros.combine(&block);
            }
            blocks >>= 1;
            if blocks == 0 {
                break;
            }
            let half = block.clone();
            block.combine(&half);
        }
        hasher.combine(&zeros);
    }
    // `len % block_len < ZERO_BLOCK_BYTES`, so the cast is lossless.
    hasher.update(&ZERO_BLOCK[..(len % block_len) as usize]);
}

/// One run of a RAM extent, relative to its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub offset: u64,
    pub len: u64,
    /// Not stored: reads back as zeros.
    pub hole: bool,
}

/// Turns a stream of per-page classifications into maximal runs.
struct Planner<E> {
    emit: E,
    /// Start of the open data run.
    data_start: Option<u64>,
    /// Start of the open zero run, which directly follows the data run.
    zero_start: Option<u64>,
    /// The open zero run includes a caller-named hole: always a hole.
    zero_forced: bool,
}

impl<E: FnMut(Run) -> io::Result<()>> Planner<E> {
    fn zero(&mut self, at: u64, forced: bool) {
        self.zero_start.get_or_insert(at);
        self.zero_forced |= forced;
    }

    /// Close the open zero run at `at`: a hole if long enough (or forced),
    /// otherwise part of the data run.
    fn close_zero_run(&mut self, at: u64) -> io::Result<()> {
        let Some(zero_start) = self.zero_start.take() else {
            return Ok(());
        };
        if std::mem::take(&mut self.zero_forced) || at - zero_start >= MIN_HOLE_BYTES {
            if let Some(data_start) = self.data_start.take() {
                (self.emit)(Run {
                    offset: data_start,
                    len: zero_start - data_start,
                    hole: false,
                })?;
            }
            (self.emit)(Run {
                offset: zero_start,
                len: at - zero_start,
                hole: true,
            })
        } else {
            self.data_start.get_or_insert(zero_start);
            Ok(())
        }
    }

    fn data(&mut self, at: u64) -> io::Result<()> {
        self.close_zero_run(at)?;
        self.data_start.get_or_insert(at);
        Ok(())
    }

    fn finish(mut self, end: u64) -> io::Result<()> {
        self.close_zero_run(end)?;
        if let Some(data_start) = self.data_start.take() {
            (self.emit)(Run {
                offset: data_start,
                len: end - data_start,
                hole: false,
            })?;
        }
        Ok(())
    }
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

/// Split `mem` into maximal data and hole runs, in order, covering all of it.
/// `holes` lists `(offset, len)` ranges, sorted and non-overlapping, that are
/// holes without being read. Elsewhere, zero-page runs of at least
/// [`MIN_HOLE_BYTES`] are holes.
pub fn plan_runs<E>(mem: &[u8], holes: &[(u64, u64)], emit: E) -> io::Result<()>
where
    E: FnMut(Run) -> io::Result<()>,
{
    let len = mem.len() as u64;
    let mut planner = Planner {
        emit,
        data_start: None,
        zero_start: None,
        zero_forced: false,
    };
    let scan = |planner: &mut Planner<E>, from: u64, to: u64| -> io::Result<()> {
        let mut page = from;
        while page < to {
            let page_end = (page + PAGE_SIZE).min(to);
            // In bounds: `to <= len == mem.len()`.
            if is_zero(&mem[page as usize..page_end as usize]) {
                planner.zero(page, false);
            } else {
                planner.data(page)?;
            }
            page = page_end;
        }
        Ok(())
    };

    let mut cursor = 0u64;
    for &(start, hole_len) in holes {
        let end = start
            .checked_add(hole_len)
            .ok_or_else(|| invalid_input("hole range overflows"))?;
        if start < cursor || end > len {
            return Err(invalid_input(
                "hole ranges must be sorted, disjoint and inside the extent",
            ));
        }
        scan(&mut planner, cursor, start)?;
        if hole_len > 0 {
            planner.zero(start, true);
        }
        cursor = end;
    }
    scan(&mut planner, cursor, len)?;
    planner.finish(len)
}

/// What [`write_image`] stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageStats {
    /// CRC-32 of the extent as stored (holes as zeros).
    pub crc: u32,
    /// Bytes written; the rest of the extent is holes.
    pub data_bytes: u64,
}

/// Store `mem` at `file_offset` of `file`, writing data runs and skipping
/// holes (see [`plan_runs`]). The target range must already read as zeros: a
/// new or truncated file that the caller extends with `set_len`.
pub fn write_image(
    file: &File,
    file_offset: u64,
    mem: &[u8],
    holes: &[(u64, u64)],
) -> io::Result<ImageStats> {
    let mut hasher = crc32fast::Hasher::new();
    let mut data_bytes = 0u64;
    plan_runs(mem, holes, |run| {
        if run.hole {
            crc32_update_zeros(&mut hasher, run.len);
            return Ok(());
        }
        // Runs lie inside `mem`, so these casts and the slice are in bounds.
        let bytes = &mem[run.offset as usize..(run.offset + run.len) as usize];
        hasher.update(bytes);
        let at = file_offset
            .checked_add(run.offset)
            .ok_or_else(|| invalid_input("image offset overflows"))?;
        file.write_all_at(bytes, at)?;
        data_bytes += run.len;
        Ok(())
    })?;
    Ok(ImageStats {
        crc: hasher.finalize(),
        data_bytes,
    })
}

/// Data extents `(start, end)` of `file` within `[offset, offset + len)`, in
/// order; the gaps are holes. A filesystem that does not track holes reports
/// a single extent. Moves the file offset (callers use positioned I/O).
pub fn data_extents(file: &File, offset: u64, len: u64) -> io::Result<Vec<(u64, u64)>> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid_input("file range overflows"))?;
    if len == 0 {
        return Ok(Vec::new());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let seek = |pos: u64, whence: libc::c_int| -> io::Result<Option<u64>> {
            let pos = libc::off_t::try_from(pos)
                .map_err(|_| invalid_input("file offset does not fit off_t"))?;
            // SAFETY: lseek on an open descriptor only moves its offset.
            let found = unsafe { libc::lseek(file.as_raw_fd(), pos, whence) };
            if found >= 0 {
                return Ok(Some(found as u64));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                // No data at or after `pos`.
                Ok(None)
            } else {
                Err(error)
            }
        };

        let mut extents = Vec::new();
        let mut pos = offset;
        while pos < end {
            let data = match seek(pos, libc::SEEK_DATA) {
                Ok(Some(data)) => data,
                Ok(None) => break,
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) && pos == offset => {
                    // SEEK_DATA unsupported here: treat the range as data.
                    return Ok(vec![(offset, end)]);
                }
                Err(error) => return Err(error),
            };
            if data >= end {
                break;
            }
            let hole = seek(data, libc::SEEK_HOLE)?.unwrap_or(end).min(end);
            if hole <= data {
                return Err(io::Error::other("SEEK_HOLE did not advance"));
            }
            extents.push((data.max(offset), hole));
            pos = hole;
        }
        Ok(extents)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        Ok(vec![(offset, end)])
    }
}

/// CRC-32 of `[offset, offset + len)` of `file`, holes as zeros, reading only
/// data extents.
pub fn crc32_range(file: &File, offset: u64, len: u64) -> io::Result<u32> {
    const READ_BYTES: u64 = 1024 * 1024;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; READ_BYTES as usize];
    let mut cursor = offset;
    for (start, end) in data_extents(file, offset, len)? {
        crc32_update_zeros(&mut hasher, start - cursor);
        let mut at = start;
        while at < end {
            let take = (end - at).min(READ_BYTES) as usize;
            file.read_exact_at(&mut buf[..take], at)?;
            hasher.update(&buf[..take]);
            at += take as u64;
        }
        cursor = end;
    }
    crc32_update_zeros(&mut hasher, offset + len - cursor);
    Ok(hasher.finalize())
}

/// Load `[offset, offset + dst.len())` of `file` into `dst`, reading only data
/// extents, and return the CRC-32 of what `dst` then holds. `dst` must already
/// be zero (fresh anonymous memory): holes are left untouched rather than
/// written with zeros, which would allocate them.
pub fn read_into(file: &File, offset: u64, dst: &mut [u8]) -> io::Result<u32> {
    let len = dst.len() as u64;
    let mut hasher = crc32fast::Hasher::new();
    let mut cursor = 0u64;
    for (start, end) in data_extents(file, offset, len)? {
        let (start, end) = (start - offset, end - offset);
        crc32_update_zeros(&mut hasher, start - cursor);
        // Extents lie inside the requested range, so `end <= dst.len()`.
        let bytes = &mut dst[start as usize..end as usize];
        file.read_exact_at(bytes, offset + start)?;
        hasher.update(bytes);
        cursor = end;
    }
    crc32_update_zeros(&mut hasher, len - cursor);
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const MIB: usize = 1024 * 1024;

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; PAGE_SIZE as usize]
    }

    fn runs(mem: &[u8], holes: &[(u64, u64)]) -> Vec<Run> {
        let mut out = Vec::new();
        plan_runs(mem, holes, |run| {
            out.push(run);
            Ok(())
        })
        .unwrap();
        // Runs tile the extent exactly.
        let mut cursor = 0;
        for run in &out {
            assert_eq!(run.offset, cursor);
            assert!(run.len > 0);
            cursor += run.len;
        }
        assert_eq!(cursor, mem.len() as u64);
        out
    }

    fn data(offset: u64, len: u64) -> Run {
        Run {
            offset,
            len,
            hole: false,
        }
    }

    fn hole(offset: u64, len: u64) -> Run {
        Run {
            offset,
            len,
            hole: true,
        }
    }

    /// The extent as a byte-for-byte reader must see it: caller-named holes
    /// read as zeros whatever memory held there.
    fn logical(mem: &[u8], holes: &[(u64, u64)]) -> Vec<u8> {
        let mut image = mem.to_vec();
        for &(start, len) in holes {
            image[start as usize..(start + len) as usize].fill(0);
        }
        image
    }

    fn scratch(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    /// Whether this filesystem stores holes (allocates nothing for them).
    fn holes_supported(dir: &std::path::Path) -> bool {
        let probe = dir.join("hole-probe");
        let file = File::create(&probe).unwrap();
        file.set_len(8 * MIB as u64).unwrap();
        let sparse = file.metadata().unwrap().blocks() == 0;
        std::fs::remove_file(probe).unwrap();
        sparse
    }

    #[test]
    fn zero_crc_combination_matches_hashing_zeros() {
        for len in [
            0u64,
            1,
            4095,
            4096,
            65_535,
            65_536,
            65_537,
            3 * 65_536 + 5,
            (64 * MIB + 123) as u64,
        ] {
            let mut combined = crc32fast::Hasher::new();
            combined.update(b"prefix");
            crc32_update_zeros(&mut combined, len);
            combined.update(b"suffix");

            let mut direct = crc32fast::Hasher::new();
            direct.update(b"prefix");
            direct.update(&vec![0u8; len as usize]);
            direct.update(b"suffix");
            assert_eq!(combined.finalize(), direct.finalize(), "len {len}");
        }
    }

    #[test]
    fn zero_detection_sees_every_byte() {
        let mut bytes = vec![0u8; 4096 + 7];
        assert!(is_zero(&bytes));
        assert!(is_zero(&[]));
        for index in [0, 63, 64, 4095, 4096, 4102] {
            bytes[index] = 1;
            assert!(!is_zero(&bytes), "byte {index}");
            bytes[index] = 0;
        }
    }

    #[test]
    fn short_zero_runs_stay_data_and_long_ones_become_holes() {
        let min_pages = MIN_HOLE_BYTES / PAGE_SIZE;
        let mut mem = Vec::new();
        mem.extend(page(1)); // page 0: data
        mem.extend(page(0).repeat(2)); // pages 1-2: short zero run
        mem.extend(page(2)); // page 3: data
        mem.extend(page(0).repeat(min_pages as usize)); // a hole-sized zero run
        mem.extend(page(3)); // data
        mem.extend(page(0).repeat(3)); // short trailing zero run
        let p = PAGE_SIZE;
        assert_eq!(
            runs(&mem, &[]),
            vec![
                data(0, 4 * p),
                hole(4 * p, min_pages * p),
                data((4 + min_pages) * p, 4 * p),
            ]
        );

        // Leading and trailing zero runs long enough to be holes.
        let mut mem = page(0).repeat(min_pages as usize);
        mem.extend(page(9));
        mem.extend(page(0).repeat(min_pages as usize));
        assert_eq!(
            runs(&mem, &[]),
            vec![
                hole(0, min_pages * p),
                data(min_pages * p, p),
                hole((min_pages + 1) * p, min_pages * p),
            ]
        );

        assert_eq!(runs(&page(0), &[]), vec![data(0, p)]);
        assert_eq!(runs(&[], &[]), vec![]);
    }

    #[test]
    fn named_holes_are_holes_at_any_length_and_merge_with_zero_neighbours() {
        let p = PAGE_SIZE;
        let mut mem = page(1);
        mem.extend(page(0)); // zero page directly before the named hole
        mem.extend(page(0xee).repeat(2)); // named hole: junk is never stored
        mem.extend(page(0)); // zero page after it
        mem.extend(page(4));
        let holes = [(2 * p, 2 * p)];
        assert_eq!(
            runs(&mem, &holes),
            vec![data(0, p), hole(p, 4 * p), data(5 * p, p)]
        );

        // Unaligned, empty and trailing named holes.
        let mem = vec![7u8; 10_000];
        assert_eq!(
            runs(&mem, &[(100, 0), (5000, 5000)]),
            vec![data(0, 5000), hole(5000, 5000)]
        );

        for bad in [
            vec![(4096, 4096), (0, 4096)],
            vec![(0, 8192), (4096, 4096)],
            vec![(0, 20_000)],
            vec![(u64::MAX, 2)],
        ] {
            assert!(plan_runs(&mem, &bad, |_| Ok(())).is_err(), "{bad:?}");
        }
    }

    /// 32 MiB extent: 1 MiB of data, a short zero run inside the data, a
    /// 15 MiB never-plugged range full of junk, and 15 MiB of zeros.
    fn sample_extent() -> (Vec<u8>, Vec<(u64, u64)>) {
        let mut mem = vec![0u8; 32 * MIB];
        for (index, byte) in mem[..MIB].iter_mut().enumerate() {
            *byte = (index % 251) as u8 + 1;
        }
        mem[8192..12288].fill(0);
        mem[2 * MIB..17 * MIB].fill(0xa5);
        (mem, vec![(2 * MIB as u64, 15 * MIB as u64)])
    }

    #[test]
    fn written_image_is_sparse_and_reads_back_as_the_logical_extent() {
        let (mem, holes) = sample_extent();
        let expected = logical(&mem, &holes);
        let (dir, path) = scratch("image.snap");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let header = b"HEADER-AND-STATE";
        file.write_all_at(header, 0).unwrap();
        let mem_offset = 4096;

        let stats = write_image(&file, mem_offset, &mem, &holes).unwrap();
        file.set_len(mem_offset + mem.len() as u64).unwrap();

        assert_eq!(stats.crc, crc32fast::hash(&expected));
        assert!(
            stats.data_bytes <= MIB as u64,
            "wrote {} bytes",
            stats.data_bytes
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..header.len()], header);
        assert_eq!(&bytes[mem_offset as usize..], &expected[..]);

        if holes_supported(dir.path()) {
            let allocated = file.metadata().unwrap().blocks() * 512;
            assert!(
                allocated <= stats.data_bytes + 2 * MIB as u64,
                "{allocated} bytes allocated for {} bytes of data",
                stats.data_bytes
            );
            let extents = data_extents(&file, mem_offset, mem.len() as u64).unwrap();
            let in_extents: u64 = extents.iter().map(|(start, end)| end - start).sum();
            assert!(in_extents < 2 * MIB as u64, "{extents:?}");
        } else {
            eprintln!("filesystem does not store holes; skipping allocation check");
        }
    }

    #[test]
    fn readers_treat_holes_as_zeros_and_verify_the_logical_extent() {
        let (mem, holes) = sample_extent();
        let expected = logical(&mem, &holes);
        let (_dir, path) = scratch("image.snap");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mem_offset = 8192;
        let stats = write_image(&file, mem_offset, &mem, &holes).unwrap();
        file.set_len(mem_offset + mem.len() as u64).unwrap();

        assert_eq!(
            crc32_range(&file, mem_offset, mem.len() as u64).unwrap(),
            stats.crc
        );
        let mut loaded = vec![0u8; mem.len()];
        assert_eq!(
            read_into(&file, mem_offset, &mut loaded).unwrap(),
            stats.crc
        );
        assert_eq!(loaded, expected);

        // A dense image of the same bytes verifies identically.
        let (_dense_dir, dense_path) = scratch("dense.snap");
        std::fs::write(&dense_path, &expected).unwrap();
        let dense = File::open(&dense_path).unwrap();
        assert_eq!(crc32_range(&dense, 0, mem.len() as u64).unwrap(), stats.crc);
    }

    #[test]
    fn integrity_check_catches_corruption_in_data_and_in_holes() {
        let (mem, holes) = sample_extent();
        let (_dir, path) = scratch("image.snap");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let stats = write_image(&file, 0, &mem, &holes).unwrap();
        let len = mem.len() as u64;
        file.set_len(len).unwrap();
        assert_eq!(crc32_range(&file, 0, len).unwrap(), stats.crc);

        // One flipped bit in written data.
        let mut byte = [0u8];
        file.read_exact_at(&mut byte, 100).unwrap();
        file.write_all_at(&[byte[0] ^ 1], 100).unwrap();
        assert_ne!(crc32_range(&file, 0, len).unwrap(), stats.crc);
        let mut loaded = vec![0u8; mem.len()];
        assert_ne!(read_into(&file, 0, &mut loaded).unwrap(), stats.crc);
        file.write_all_at(&byte, 100).unwrap();
        assert_eq!(crc32_range(&file, 0, len).unwrap(), stats.crc);

        // One non-zero byte written into the never-plugged hole.
        file.write_all_at(&[1], 5 * MIB as u64).unwrap();
        assert_ne!(crc32_range(&file, 0, len).unwrap(), stats.crc);
        let mut loaded = vec![0u8; mem.len()];
        assert_ne!(read_into(&file, 0, &mut loaded).unwrap(), stats.crc);
    }
}
