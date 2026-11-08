//! Mounted filesystems and their capacity, for the start screen.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Mount {
    pub path: PathBuf,
    pub device: String,
    pub fs_type: String,
    pub total: u64,
    pub free: u64,
}

const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "zfs", "bcachefs", "vfat", "exfat", "ntfs",
    "ntfs3", "fuseblk", "jfs", "reiserfs", "nfs", "nfs4", "cifs", "smb3", "apfs", "hfsplus",
];

/// Total and available bytes on the filesystem holding `path`.
pub fn capacity(path: &Path) -> Option<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_blocks as u64 * frsize, st.f_bavail as u64 * frsize))
}

pub fn list() -> Vec<Mount> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut seen_dev = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(dev), Some(mnt), Some(ty)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !REAL_FS.contains(&ty) {
            continue;
        }
        let mnt = unescape(mnt);
        if mnt.starts_with("/snap/") || mnt.starts_with("/var/lib/docker") {
            continue;
        }
        // btrfs subvolumes share a device; keep the first (shortest) mount.
        if !seen_dev.insert(dev.to_string()) {
            continue;
        }
        let path = PathBuf::from(&mnt);
        let Some((total, free)) = capacity(&path) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        out.push(Mount {
            path,
            device: dev.to_string(),
            fs_type: ty.to_string(),
            total,
            free,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// /proc/mounts escapes spaces and friends as octal (`\040`).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
