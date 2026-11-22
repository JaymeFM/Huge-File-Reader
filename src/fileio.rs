//! Low-level file access: opening files for scanning, positional reads and
//! detecting whether a file lives on a spinning disk.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Opens a file for reading. `sequential` hints the OS that the file will be
/// scanned front to back, which enables aggressive read-ahead on Windows.
pub fn open_read(path: &Path, sequential: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        if sequential {
            opts.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
        }
    }
    #[cfg(not(windows))]
    let _ = sequential;
    opts.open(path)
}

/// Reads up to `buf.len()` bytes at `offset` without touching any shared
/// cursor state the caller relies on. May return fewer bytes than requested.
#[inline]
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
}

/// Fills `buf` from `offset`, stopping early only at end of file.
/// Returns the number of bytes read.
pub fn read_full_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match read_at(file, &mut buf[filled..], offset) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// What kind of storage a file is on, which decides how we read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    /// No seek penalty (SSD / NVMe): many reads in flight is fastest.
    Ssd,
    /// Spinning disk: strictly sequential reads are fastest.
    Hdd,
    /// Could not tell (network share, unusual volume, non-Windows).
    Unknown,
}

impl DriveKind {
    pub fn label(self) -> &'static str {
        match self {
            DriveKind::Ssd => "SSD",
            DriveKind::Hdd => "hard drive",
            DriveKind::Unknown => "drive type unknown",
        }
    }

    /// How many chunk reads may be in flight at once for this kind of drive.
    pub fn io_parallelism(self) -> usize {
        match self {
            DriveKind::Ssd => 8,
            DriveKind::Hdd | DriveKind::Unknown => 1,
        }
    }
}

/// Asks Windows whether the volume holding `path` incurs a seek penalty.
pub fn detect_drive_kind(path: &Path) -> DriveKind {
    #[cfg(windows)]
    {
        win::seek_penalty(path)
            .map(|penalty| if penalty { DriveKind::Hdd } else { DriveKind::Ssd })
            .unwrap_or(DriveKind::Unknown)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        DriveKind::Unknown
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetVolumePathNameW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn seek_penalty(path: &Path) -> Option<bool> {
        let abs = std::fs::canonicalize(path).ok()?;
        let file_w = wide(abs.as_os_str());
        let mut vol = [0u16; 1024];
        // SAFETY: both buffers are valid, NUL-terminated / sized as passed.
        let ok = unsafe { GetVolumePathNameW(file_w.as_ptr(), vol.as_mut_ptr(), vol.len() as u32) };
        if ok == 0 {
            return None;
        }
        let len = vol.iter().position(|&c| c == 0)?;
        let vol_path = std::ffi::OsString::from_wide(&vol[..len]);
        let vol_str = vol_path.to_string_lossy().to_string();
        // canonicalize() yields \\?\C:\..., GetVolumePathName then returns \\?\C:\
        let trimmed = vol_str.trim_start_matches(r"\\?\").trim_end_matches('\\');
        // Only plain drive letters ("C:") map cleanly onto a device path.
        if trimmed.len() != 2 || !trimmed.ends_with(':') {
            return None;
        }
        let device = wide(std::ffi::OsStr::new(&format!(r"\\.\{trimmed}")));
        // SAFETY: device is a valid NUL-terminated wide string. Access 0 is
        // enough to query storage properties and needs no admin rights.
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut desc = DEVICE_SEEK_PENALTY_DESCRIPTOR { Version: 0, Size: 0, IncursSeekPenalty: false };
        let mut returned = 0u32;
        // SAFETY: in/out pointers reference properly sized local structs.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                &mut desc as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: handle came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(handle) };
        if ok == 0 || (returned as usize) < std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
            return None;
        }
        Some(desc.IncursSeekPenalty)
    }
}
