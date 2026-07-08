//! Platform-specific "hole punching": tell the filesystem to free the
//! physical blocks backing a byte range of a file, without changing the
//! file's logical size. This is how Molt reclaims space *during* extraction.

use std::fs::File;
use std::io;

/// Punch a hole over `[offset, offset + len)` in `file`.
/// The filesystem frees any whole blocks inside the range immediately.
#[cfg(unix)]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if len == 0 {
        return Ok(());
    }
    let ret = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            len as libc::off_t,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// No preparation needed on Unix filesystems that support punching.
#[cfg(unix)]
pub fn prepare(_file: &File) -> io::Result<()> {
    Ok(())
}

/// On Windows (NTFS) the file must first be flagged sparse; afterwards,
/// FSCTL_SET_ZERO_DATA on a range deallocates its clusters.
#[cfg(windows)]
pub fn prepare(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::{FSCTL_SET_ZERO_DATA, FILE_ZERO_DATA_INFORMATION};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    if len == 0 {
        return Ok(());
    }
    let info = FILE_ZERO_DATA_INFORMATION {
        FileOffset: offset as i64,
        BeyondFinalZero: (offset + len) as i64,
    };
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_ZERO_DATA,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// True if `[offset, offset + len)` holds no data — every byte is either in
/// a filesystem hole or reads as zero. That is the signature of a range this
/// tool punched in an earlier run: punching deallocates whole blocks and
/// zeroes the block-edge remainders, and no valid compressed entry is all
/// zeros. Used to skip already-extracted entries when resuming.
pub fn hollowed(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    if len == 0 {
        return Ok(false);
    }
    let ranges = allocated_in(file, offset, len)?;
    let allocated: u64 = ranges.iter().map(|&(_, l)| l).sum();
    if allocated == 0 {
        return Ok(true);
    }
    // A punched range keeps at most a couple of partial blocks allocated
    // (zeroed). Anything with substantially allocated data was never
    // punched — don't spend I/O proving it.
    const READ_CAP: u64 = 256 * 1024;
    if allocated > READ_CAP {
        return Ok(false);
    }
    let mut buf = vec![0u8; 64 * 1024];
    for &(mut o, mut l) in &ranges {
        while l > 0 {
            let n = l.min(buf.len() as u64) as usize;
            read_exact_at(file, &mut buf[..n], o)?;
            if buf[..n].iter().any(|&b| b != 0) {
                return Ok(false);
            }
            o += n as u64;
            l -= n as u64;
        }
    }
    Ok(true)
}

/// Allocated (non-hole) subranges of `[offset, offset + len)`, as
/// `(absolute_offset, length)` pairs clamped to the queried range.
#[cfg(unix)]
fn allocated_in(file: &File, offset: u64, len: u64) -> io::Result<Vec<(u64, u64)>> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let end = offset + len;
    let mut out = Vec::new();
    let mut pos = offset as libc::off_t;
    loop {
        let data = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
        if data < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENXIO) {
                break; // only holes from pos to EOF
            }
            return Err(err); // includes EINVAL: fs without SEEK_DATA support
        }
        let data = data as u64;
        if data >= end {
            break;
        }
        let hole = unsafe { libc::lseek(fd, data as libc::off_t, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(io::Error::last_os_error());
        }
        let sub_end = (hole as u64).min(end);
        out.push((data, sub_end - data));
        if sub_end >= end {
            break;
        }
        pos = sub_end as libc::off_t;
    }
    Ok(out)
}

#[cfg(windows)]
fn allocated_in(file: &File, offset: u64, len: u64) -> io::Result<Vec<(u64, u64)>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let end = offset + len;
    let mut out = Vec::new();
    let mut pos = offset;
    loop {
        let query = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: pos as i64,
            Length: (end - pos) as i64,
        };
        let mut results: [FILE_ALLOCATED_RANGE_BUFFER; 64] = unsafe { std::mem::zeroed() };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                FSCTL_QUERY_ALLOCATED_RANGES,
                &query as *const _ as *const _,
                std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                results.as_mut_ptr() as *mut _,
                std::mem::size_of_val(&results) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = ok == 0 && io::Error::last_os_error().raw_os_error()
            == Some(ERROR_MORE_DATA as i32);
        if ok == 0 && !more {
            return Err(io::Error::last_os_error());
        }
        let count = returned as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        for r in &results[..count] {
            let sub = r.FileOffset as u64;
            let sub_end = (sub + r.Length as u64).min(end);
            out.push((sub, sub_end - sub));
            pos = sub_end;
        }
        if !more || count == 0 || pos >= end {
            break;
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        done += n;
    }
    Ok(())
}

/// Physical bytes currently allocated to the file (to report reclaimed space).
#[cfg(unix)]
pub fn allocated_bytes(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.blocks() * 512)
}

#[cfg(windows)]
pub fn allocated_bytes(file: &File) -> io::Result<u64> {
    // Compressed/sparse size on disk. GetFileInformationByHandleEx with
    // FileStandardInfo gives AllocationSize.
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandleEx, FileStandardInfo, FILE_STANDARD_INFO,
    };
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileStandardInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok != 0 {
        Ok(info.AllocationSize as u64)
    } else {
        Err(io::Error::last_os_error())
    }
}
