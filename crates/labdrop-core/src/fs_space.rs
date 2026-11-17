use crate::error::{LabDropError, Result};
use std::path::Path;

/// Disk space statistics for a filesystem volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpaceInfo {
    pub available_bytes: u64,
    pub total_bytes: u64,
}

/// Queries available and total disk space for the volume containing `path` (or its nearest existing ancestor).
pub fn query_disk_space(path: &Path) -> Result<DiskSpaceInfo> {
    let mut probe = path;
    while !probe.exists() {
        if let Some(parent) = probe.parent() {
            if parent.as_os_str().is_empty() {
                probe = Path::new(".");
                break;
            }
            probe = parent;
        } else {
            probe = Path::new(".");
            break;
        }
    }

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(probe.as_os_str().as_bytes()).map_err(|e| {
            LabDropError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path for statvfs: {e}"),
            ))
        })?;

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return Err(LabDropError::IoPath {
                path: probe.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }

        let frsize = if stat.f_frsize > 0 {
            stat.f_frsize as u64
        } else {
            stat.f_bsize as u64
        };
        let available_bytes = (stat.f_bavail as u64).saturating_mul(frsize);
        let total_bytes = (stat.f_blocks as u64).saturating_mul(frsize);
        Ok(DiskSpaceInfo {
            available_bytes,
            total_bytes,
        })
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                lpDirectoryName: *const u16,
                lpFreeBytesAvailableToCaller: *mut u64,
                lpTotalNumberOfBytes: *mut u64,
                lpTotalNumberOfFreeBytes: *mut u64,
            ) -> i32;
        }

        let wide: Vec<u16> = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_caller: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut total_free: u64 = 0;

        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_caller,
                &mut total_bytes,
                &mut total_free,
            )
        };
        if ok == 0 {
            return Err(LabDropError::IoPath {
                path: probe.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }

        Ok(DiskSpaceInfo {
            available_bytes: free_caller,
            total_bytes,
        })
    }
}

/// Formats a byte count into a concise human-readable string (e.g., `14.2 MiB`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;

    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.2} MiB", b / MIB)
    } else if b < TIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.2} TiB", b / TIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_disk_space_should_return_nonzero_for_temp_dir() {
        let info = query_disk_space(&std::env::temp_dir()).expect("should query temp dir space");
        assert!(info.total_bytes > 0);
        assert!(info.available_bytes > 0);
    }

    #[test]
    fn format_bytes_should_format_units_accurately() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
    }
}
