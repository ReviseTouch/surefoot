//! Asking a path what it is standing on.
//!
//! Case, mode and speed belong to the mounted filesystem, not to the format and
//! not to the operating system: one NTFS volume is case-sensitive under Linux's
//! ntfs3 and insensitive under Windows. Every question here is asked once per
//! root, never per entry.

use std::path::Path;

/// Everything this module can say about one path, asked together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub medium: Medium,
    /// See [`disk_of`].
    pub disk: Option<String>,
    /// The mounted filesystem's own name for itself: `btrfs`, `ntfs3`, `NTFS`,
    /// `apfs`. As the platform spells it.
    pub filesystem: Option<String>,
    pub traits: FsTraits,
}

/// Ask every question about `path`.
pub fn probe(path: &Path) -> Probe {
    Probe {
        medium: medium_of(path),
        disk: disk_of(path),
        filesystem: filesystem_of(path),
        traits: traits_of(path),
    }
}

/// The mounted filesystem's name for itself, or nothing if it cannot be asked.
pub fn filesystem_of(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    return linux::filesystem_of(path);
    #[cfg(windows)]
    return windows_impl::filesystem_of(path);
    #[cfg(target_os = "macos")]
    return macos::filesystem_of(path);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = path;
        None
    }
}

/// How a mount behaves under a walk.
/// Speed, not correctness: exFAT has no stable ids and is fast, NFS the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// A local device with deep hardware queues. Parallelism is free.
    Solid,
    /// A spinning disk. Every concurrent reader is a seek, so parallelism hurts.
    Spinning,
    /// Reached over a network. Bounded by round trips rather than by the device.
    Network,
    /// In memory. tmpfs, ramfs.
    Memory,
    /// Could not be determined.
    Unknown,
}

impl Medium {
    /// How many threads a walk of this medium is worth when nothing downstream
    /// is the limit. Measured on one NVMe volume, 152,530 directories: 1.10 s on
    /// two threads, 0.33 s on eight, 0.22 s on sixteen — eight, so a walk does
    /// not take the desktop. On a spinning disk no wall time came from threads
    /// and another reader's p90 went 160 → 820 ms from one to eight.
    pub fn walk_threads(self, cores: usize) -> usize {
        match self {
            Medium::Spinning => 1,
            // Bounded by round trips rather than by the device.
            Medium::Network => 4,
            Medium::Solid | Medium::Memory | Medium::Unknown => (cores / 2).clamp(2, 8),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Medium::Solid => "solid-state",
            Medium::Spinning => "spinning",
            Medium::Network => "network",
            Medium::Memory => "memory",
            Medium::Unknown => "unknown",
        }
    }
}

/// What one mounted filesystem promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsTraits {
    /// Two names differing only in case are two files.
    pub case_sensitive: bool,
    /// Withheld wherever a mount supplies the mode: an NTFS volume mounted
    /// `fmask=0022` reports every file executable.
    pub real_modes: bool,
}

impl FsTraits {
    /// What to assume when the filesystem cannot be identified.
    /// Not "no idea, allow everything": every field takes the cheaper mistake.
    pub const UNKNOWN: FsTraits = FsTraits {
        case_sensitive: cfg!(unix),
        // Claiming it wrongly calls every file on the volume executable.
        real_modes: false,
    };

    /// The narrower of two, for a source spanning more than one filesystem.
    pub fn and(self, other: FsTraits) -> FsTraits {
        FsTraits {
            case_sensitive: self.case_sensitive && other.case_sensitive,
            real_modes: self.real_modes && other.real_modes,
        }
    }
}

/// What the mount under `path` is like to read: whether `statfs` calls the
/// filesystem a network or memory one, then whether the kernel calls its device
/// rotational.
pub fn medium_of(path: &Path) -> Medium {
    #[cfg(target_os = "linux")]
    return linux::medium_of(path);
    #[cfg(windows)]
    return windows_impl::medium_of(path);
    #[cfg(target_os = "macos")]
    return macos::medium_of(path);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    return fallback::medium_of(path);
}

/// The physical disk under `path`, as a name two paths share exactly when they
/// share the disk's head: `nvme0n1`, `disk1`, `net:server`. Walks of
/// different disks run side by side; walks of one disk take turns. None when
/// nothing can be said, and such a source takes its turn with the others.
///
/// macOS, when it comes: `statfs`'s `f_mntfromname` (`/dev/disk3s1`) names an
/// APFS volume; its physical store is `diskutil info -plist`'s
/// `APFSPhysicalStores`, or IOKit walking from the media object up to the
/// `IOBlockStorageDevice`. Every volume of one APFS container shares a disk.
pub fn disk_of(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    return linux::disk_of(path);
    #[cfg(windows)]
    return windows_impl::disk_of(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        None
    }
}

/// What the filesystem under `path` promises: one `statfs` per root at start-up,
/// not per entry. A root spanning a nested mount is judged by the root itself,
/// because the narrower promise is the safe one.
pub fn traits_of(path: &Path) -> FsTraits {
    #[cfg(target_os = "linux")]
    return linux::traits_of(path);
    #[cfg(windows)]
    return windows_impl::traits_of(path);
    #[cfg(target_os = "macos")]
    return macos::traits_of(path);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    return fallback::traits_of(path);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::FsTraits;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    // Read with `stat -f -c %t`, cross-checked against `linux/magic.h`.
    const EXT: i64 = 0xEF53; // ext2, ext3, ext4 — all three
    const BTRFS: i64 = 0x9123_683E;
    const XFS: i64 = 0x5846_5342;
    const F2FS: i64 = 0xF2F5_2010;
    const TMPFS: i64 = 0x0102_1994;
    const ZFS: i64 = 0x2FC1_2FC1;
    const OVERLAY: i64 = 0x794C_7630;
    const MSDOS: i64 = 0x4D44;
    const EXFAT: i64 = 0x2011_BAB0;
    // Two NTFS drivers, two magics; the second is `ntfs` as little-endian bytes.
    const NTFS_3G: i64 = 0x5346_544E;
    const NTFS3: i64 = 0x7366_746E;
    // Reached over a network. A local FUSE mount read as remote costs parallelism.
    const NFS: i64 = 0x6969;
    const SMB: i64 = 0x517B;
    const CIFS: i64 = 0xFF53_4D42;
    const SMB2: i64 = 0xFE53_4D42;
    const FUSE: i64 = 0x6573_5546;
    const NINEP: i64 = 0x0102_1997;
    const AFS: i64 = 0x5346_414F;
    const CEPH: i64 = 0x00C3_6400;
    const RAMFS: i64 = 0x8584_5846;

    pub fn traits_of(path: &Path) -> FsTraits {
        let Some(magic) = magic(path) else {
            return FsTraits::UNKNOWN;
        };
        match magic {
            EXT | BTRFS | XFS | F2FS | TMPFS | ZFS | OVERLAY => FsTraits {
                case_sensitive: true,
                real_modes: true,
            },
            // ntfs3 is case-sensitive unless mounted `nocase`, invisible from here.
            NTFS_3G | NTFS3 => FsTraits {
                case_sensitive: true,
                // `stat` returns `fmask`/`dmask`, the same value for every file.
                real_modes: false,
            },
            MSDOS | EXFAT => FsTraits {
                case_sensitive: false,
                real_modes: false,
            },
            _ => FsTraits::UNKNOWN,
        }
    }

    pub fn medium_of(path: &Path) -> super::Medium {
        use super::Medium;
        match magic(path) {
            Some(NFS | SMB | CIFS | SMB2 | FUSE | NINEP | AFS | CEPH) => return Medium::Network,
            Some(TMPFS | RAMFS) => return Medium::Memory,
            _ => {}
        }
        match rotational(path) {
            Some(true) => Medium::Spinning,
            Some(false) => Medium::Solid,
            None => Medium::Unknown,
        }
    }

    /// One line of `/proc/self/mountinfo` that matters here.
    pub(super) struct Mount {
        pub(super) fstype: String,
        /// `/dev/nvme0n1p5`, `server:/export`, `tmpfs`.
        pub(super) source: String,
    }

    /// The mount `path` lives on: the longest mount point above it, and of two
    /// mounts at one point the later, which is the one on top.
    fn mount_of(path: &Path) -> Option<Mount> {
        let real = std::fs::canonicalize(path).ok()?;
        let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        mount_in(&table, &real)
    }

    pub(super) fn mount_in(table: &str, real: &Path) -> Option<Mount> {
        let mut best: Option<(usize, Mount)> = None;
        for line in table.lines() {
            // `id parent maj:min root mount-point options [tags…] - fstype source super`
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let Some(point) = left.split(' ').nth(4) else {
                continue;
            };
            let point = unescape(point);
            if !real.starts_with(&point) {
                continue;
            }
            let mut right = right.split(' ');
            let (Some(fstype), Some(source)) = (right.next(), right.next()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(len, _)| point.len() >= *len) {
                best = Some((
                    point.len(),
                    Mount {
                        fstype: fstype.to_owned(),
                        source: unescape(source),
                    },
                ));
            }
        }
        best.map(|(_, m)| m)
    }

    /// The kernel writes a space, tab, newline and backslash in a mount point
    /// as `\040`, `\011`, `\012` and `\134`.
    fn unescape(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\'
                && let Some(oct) = field.get(i + 1..i + 4)
                && let Ok(byte) = u8::from_str_radix(oct, 8)
            {
                out.push(byte);
                i += 4;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The mount source under `path`, `/dev/nvme0n1p5`.
    fn source_of(path: &Path) -> Option<String> {
        let src = mount_of(path)?.source;
        (!src.is_empty()).then_some(src)
    }

    pub fn filesystem_of(path: &Path) -> Option<String> {
        mount_of(path).map(|m| m.fstype)
    }

    pub fn disk_of(path: &Path) -> Option<String> {
        if matches!(
            magic(path),
            Some(NFS | SMB | CIFS | SMB2 | NINEP | AFS | CEPH)
        ) {
            // One lane a server: its link is the shared head.
            let src = source_of(path)?;
            let host = src
                .trim_start_matches('/')
                .split([':', '/'])
                .next()
                .unwrap_or(&src);
            return Some(format!("net:{host}"));
        }
        let src = source_of(path)?;
        if !src.starts_with("/dev/") {
            return None;
        }
        let md = std::fs::metadata(&src).ok()?;
        use std::os::unix::fs::MetadataExt;
        let (maj, min) = (libc::major(md.rdev()), libc::minor(md.rdev()));
        let dev = std::fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")).ok()?;
        Some(disk_above(&dev, 0))
    }

    /// The whole disk a block device is part of: a partition's parent, and for a
    /// mapped device (LUKS, LVM) the disk of the first device beneath it.
    fn disk_above(dev: &Path, depth: u32) -> String {
        let name = || {
            dev.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if dev.join("partition").exists()
            && let Some(parent) = dev.parent()
        {
            return parent
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(name);
        }
        if depth < 4
            && let Ok(mut below) = std::fs::read_dir(dev.join("slaves"))
            && let Some(Ok(first)) = below.next()
            && let Ok(real) = std::fs::canonicalize(first.path())
        {
            return disk_above(&real, depth + 1);
        }
        name()
    }

    /// Does the kernel call the device under this path rotational?
    /// A partition's sysfs entry has no `queue/`, hence the `..` fallback.
    fn rotational(path: &Path) -> Option<bool> {
        let src = source_of(path)?;
        if !src.starts_with("/dev/") {
            return None;
        }
        let md = std::fs::metadata(&src).ok()?;
        use std::os::unix::fs::MetadataExt;
        let (maj, min) = (libc::major(md.rdev()), libc::minor(md.rdev()));
        let base = format!("/sys/dev/block/{maj}:{min}");
        for p in [
            format!("{base}/queue/rotational"),
            format!("{base}/../queue/rotational"),
        ] {
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Some(s.trim() == "1");
            }
        }
        None
    }

    /// `statfs(2)`'s `f_type`, or `None` if the path cannot be reached.
    fn magic(path: &Path) -> Option<i64> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: a zeroed `statfs` is valid to write into, and `c` outlives the call.
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c.as_ptr(), &mut buf) };
        if rc != 0 {
            return None;
        }
        Some(buf.f_type as i64)
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{FsTraits, Medium};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };

    // Written out rather than imported: `windows-sys` moves them across versions.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;

    /// The volume root a path lives on: `C:\` for `C:\Users\x`.
    /// Windows keys everything it can say about a filesystem to the volume.
    fn volume_root(path: &Path) -> Option<Vec<u16>> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut root = vec![0u16; 260];
        // SAFETY: both buffers are valid for the lengths passed; the input is NUL-terminated.
        let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
        (ok != 0).then_some(root)
    }

    /// The same answer for every volume Windows can reach, so nothing is
    /// asked. Not `FILE_CASE_SENSITIVE_SEARCH`: NTFS sets it on every volume,
    /// for what the filesystem could do, while Windows looks names up without
    /// case — the opposite of the same disk under ntfs3. A directory can be
    /// made sensitive one at a time (`fsutil file setCaseSensitiveInfo`); that
    /// is not asked either.
    pub fn traits_of(_path: &Path) -> FsTraits {
        FsTraits {
            case_sensitive: false,
            // Windows has ACLs, not a mode bit.
            real_modes: false,
        }
    }

    pub fn filesystem_of(path: &Path) -> Option<String> {
        let root = volume_root(path)?;
        let mut name = [0u16; 64];
        // SAFETY: null is accepted for unwanted outputs; the buffer is valid.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        let end = name.iter().position(|&c| c == 0).unwrap_or(0);
        (ok != 0 && end > 0).then(|| String::from_utf16_lossy(&name[..end]))
    }

    pub fn medium_of(path: &Path) -> Medium {
        let Some(root) = volume_root(path) else {
            return Medium::Unknown;
        };
        // SAFETY: `root` is a NUL-terminated wide string from Windows itself.
        match unsafe { GetDriveTypeW(root.as_ptr()) } {
            DRIVE_REMOTE => Medium::Network,
            // A fixed disk says whether it seeks; a removable one is usually
            // flash, and assuming spinning would slow every USB stick.
            DRIVE_FIXED if seeks(&root) == Some(true) => Medium::Spinning,
            DRIVE_REMOVABLE | DRIVE_FIXED => Medium::Solid,
            _ => Medium::Unknown,
        }
    }

    /// `\\.\C:` for the root `C:\`, opened for nothing but questions: no
    /// administrator needed for those. None for a root that is not a letter.
    fn volume(root: &[u16]) -> Option<isize> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let letter = char::from_u32(*root.first()? as u32)?;
        if !letter.is_ascii_alphabetic() || root.get(1) != Some(&(b':' as u16)) {
            return None;
        }
        let name: Vec<u16> = format!(r"\\.\{letter}:")
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: a NUL-terminated name; the handle is closed by the caller.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        (h != INVALID_HANDLE_VALUE).then_some(h as isize)
    }

    fn ask(root: &[u16], code: u32, input: &[u8], out: &mut [u8]) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::IO::DeviceIoControl;
        let Some(h) = volume(root) else {
            return false;
        };
        let mut got = 0u32;
        // SAFETY: buffers of the sizes given; the handle is closed below.
        let ok = unsafe {
            DeviceIoControl(
                h as _,
                code,
                input.as_ptr() as *const _,
                input.len() as u32,
                out.as_mut_ptr() as *mut _,
                out.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(h as _) };
        ok != 0
    }

    /// Whether the disk under a volume pays for seeking: a spinning one.
    fn seeks(root: &[u16]) -> Option<bool> {
        const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
        // StorageDeviceSeekPenaltyProperty, PropertyStandardQuery.
        let mut query = [0u8; 12];
        query[0..4].copy_from_slice(&7u32.to_le_bytes());
        // DEVICE_SEEK_PENALTY_DESCRIPTOR: version, size, IncursSeekPenalty.
        let mut out = [0u8; 12];
        ask(root, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut out).then_some(out[8] != 0)
    }

    pub fn disk_of(path: &Path) -> Option<String> {
        let root = volume_root(path)?;
        // SAFETY: `root` is a NUL-terminated wide string from Windows itself.
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
            let n = root.iter().position(|&c| c == 0).unwrap_or(root.len());
            let text = String::from_utf16_lossy(&root[..n]);
            let host = text
                .trim_start_matches('\\')
                .split('\\')
                .next()
                .unwrap_or(&text);
            return Some(format!("net:{}", host.to_lowercase()));
        }
        const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
        // VOLUME_DISK_EXTENTS: a count, then { DiskNumber, StartingOffset,
        // ExtentLength } each 24 bytes, the first at 8.
        let mut out = [0u8; 8 + 24 * 4];
        if !ask(
            root.as_slice(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            &[],
            &mut out,
        ) {
            return None;
        }
        let disk = u32::from_le_bytes(out[8..12].try_into().ok()?);
        Some(format!("disk{disk}"))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{FsTraits, Medium};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `statfs`'s own name for the filesystem: "apfs", "hfs", "exfat", "msdos",
    /// "nfs", "smbfs", "webdav".
    fn fstype(path: &Path) -> Option<(String, u32)> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: a zeroed `statfs` is valid to write into, and `c` outlives the call.
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
            return None;
        }
        let name: Vec<u8> = buf
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some((String::from_utf8_lossy(&name).into_owned(), buf.f_flags))
    }

    pub fn filesystem_of(path: &Path) -> Option<String> {
        fstype(path).map(|(fs, _)| fs)
    }

    pub fn traits_of(path: &Path) -> FsTraits {
        let Some((fs, _)) = fstype(path) else {
            return FsTraits::UNKNOWN;
        };
        match fs.as_str() {
            // Either can be formatted case-sensitive and `statfs` does not say.
            "apfs" | "hfs" => FsTraits {
                case_sensitive: false,
                real_modes: true,
            },
            "msdos" | "exfat" => FsTraits {
                case_sensitive: false,
                real_modes: false,
            },
            _ => FsTraits::UNKNOWN,
        }
    }

    pub fn medium_of(path: &Path) -> Medium {
        let Some((fs, flags)) = fstype(path) else {
            return Medium::Unknown;
        };
        // `MNT_LOCAL` is off for anything reached over a network, whatever it calls itself.
        if flags & libc::MNT_LOCAL as u32 == 0 {
            return Medium::Network;
        }
        match fs.as_str() {
            "nfs" | "smbfs" | "afpfs" | "webdav" | "ftp" => Medium::Network,
            // APFS is not offered on rotational media; an external HFS+ disk is missed.
            "apfs" => Medium::Solid,
            _ => Medium::Unknown,
        }
    }
}

/// Platforms with no implementation yet.
#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod fallback {
    use super::{FsTraits, Medium};
    use std::path::Path;

    pub fn traits_of(_path: &Path) -> FsTraits {
        FsTraits::UNKNOWN
    }
    pub fn medium_of(_path: &Path) -> Medium {
        Medium::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanning_two_filesystems_takes_the_narrower_promise() {
        let good = FsTraits {
            case_sensitive: true,
            real_modes: true,
        };
        let fat = FsTraits {
            case_sensitive: false,
            real_modes: false,
        };
        assert_eq!(good.and(fat), fat);
        assert_eq!(good.and(good), good);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_real_directory_is_identified() {
        // A temp directory is on a recognised filesystem, whatever this machine runs.
        let t = traits_of(&std::env::temp_dir());
        assert!(t.case_sensitive, "{t:?}");
    }

    #[test]
    fn an_unreachable_path_does_not_claim_anything() {
        assert_eq!(
            traits_of(Path::new("/nonexistent-surefoot-test-path")),
            FsTraits::UNKNOWN
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_longest_mount_point_above_a_path_is_its_mount_and_the_later_wins_a_tie() {
        let table = "\
22 1 0:20 / / rw - btrfs /dev/nvme0n1p5 rw,subvol=/@
30 22 0:25 /@home /home rw shared:5 - btrfs /dev/nvme0n1p5 rw,subvol=/@home
41 22 8:2 / /mnt/my\\040disk rw - ntfs3 /dev/sda2 rw
50 22 0:40 / /mnt/over rw - tmpfs tmpfs rw
51 22 0:41 / /mnt/over rw - nfs server:/export rw
";
        let at = |p: &str| {
            let m = linux::mount_in(table, Path::new(p)).unwrap();
            (m.fstype, m.source)
        };
        assert_eq!(at("/etc"), ("btrfs".into(), "/dev/nvme0n1p5".into()));
        assert_eq!(at("/home/a/b"), ("btrfs".into(), "/dev/nvme0n1p5".into()));
        // Not a prefix of the text: `/homework` is under `/`, not under `/home`.
        assert_eq!(at("/homework"), ("btrfs".into(), "/dev/nvme0n1p5".into()));
        assert_eq!(at("/mnt/my disk/x"), ("ntfs3".into(), "/dev/sda2".into()));
        assert_eq!(at("/mnt/over/x"), ("nfs".into(), "server:/export".into()));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_real_directory_names_its_filesystem() {
        assert!(filesystem_of(&std::env::temp_dir()).is_some());
    }
}
