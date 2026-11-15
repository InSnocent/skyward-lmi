//! Treat early-boot placeholder timestamps on ephemeral filesystems as unknown.
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const START_OF_1971: i64 = 31_536_000;

// These transient filesystems can create entries before Recovery has set the
// wall clock. An early-1970 timestamp there is not a useful modification date.
fn meaningful(filesystem: u64, modified: i64) -> bool {
    let ephemeral = matches!(
        filesystem,
        0x6265_6570 // configfs
            | 0x6265_6572 // sysfs
            | 0x0000_9fa0 // procfs
            | 0x0102_1994 // tmpfs
            | 0x8584_58f6 // ramfs/rootfs
            | 0x0000_1cd1 // devpts
    );
    !ephemeral || !(0..START_OF_1971).contains(&modified)
}

/// Whether an entry's recorded time is meaningful, without changing it.
/// On a statfs error, preserve and display the recorded value.
pub fn known(path: &Path, modified: i64) -> bool {
    if !(0..START_OF_1971).contains(&modified) {
        return true;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else { return true };
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return true;
    }
    let filesystem = unsafe { info.assume_init() }.f_type as u64 & 0xffff_ffff;
    meaningful(filesystem, modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_uninitialised_times_on_ephemeral_filesystems_are_unknown() {
        for magic in [0x6265_6570, 0x6265_6572, 0x0000_9fa0, 0x0102_1994, 0x8584_58f6, 0x0000_1cd1] {
            assert!(!meaningful(magic, 1));
            assert!(meaningful(magic, 1_684_080_000));
        }
        assert!(meaningful(0xf2f5_2010, 1), "a real F2FS timestamp stays visible");
        assert!(meaningful(0xef53, 1), "a real ext4 timestamp stays visible");
        assert!(known(Path::new("/a/nonexistent/skyward-test-entry"), 1), "unknown filesystems keep their recorded value");
    }

    #[test]
    fn procfs_is_recognised_as_ephemeral() {
        assert!(!known(Path::new("/proc"), 1));
    }
}
