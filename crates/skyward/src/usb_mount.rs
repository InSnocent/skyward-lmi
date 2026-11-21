//! Pin the original OTG mount for a single backup or restore.

use crate::storage::{DIRECTORY, open_regular_in};
use anyhow::{ensure, Context, Result};
use rustix::fs::{self as at, Mode};
use skyward_block::{mount, usb::{self, Drive}};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub(crate) struct UsbMount {
    drive: Drive,
    point: PathBuf,
    mount_id: i32,
    root: File,
}

impl UsbMount {
    pub(crate) fn open(drive: Drive, point: &Path, mount_id: i32) -> Result<Self> {
        ensure!(mount_at(point)? == Some(mount_id), "the USB drive is no longer mounted");
        let visible = mount::visible(point)?.context("the USB drive is no longer mounted")?;
        ensure!(matches!(visible.filesystem.as_str(), "vfat" | "exfat"), "expected a FAT32 or exFAT USB drive");
        let root = File::from(at::open(point, DIRECTORY, Mode::empty()).context("open the mounted USB drive")?);
        ensure!(file_mount(&root)? == mount_id, "the mount changed while opening the USB drive");
        let usb = Self { drive, point: point.into(), mount_id, root };
        usb.attached()?;
        Ok(usb)
    }

    pub(crate) fn attached(&self) -> Result<()> {
        ensure!(mount_at(&self.point)? == Some(self.mount_id) && usb::drives()?.contains(&self.drive),
            "the USB drive was removed or replaced");
        Ok(())
    }

    /// Open a ZIP relative to the held mount and use that file for the
    /// entire inspection and installation.
    pub(crate) fn open_zip(&self, relative: &Path) -> Result<File> {
        let file = open_regular_in(self.root.try_clone()?, relative).context("open the ZIP on the USB drive")?;
        ensure!(file_mount(&file)? == self.mount_id, "the ZIP is outside the selected USB mount");
        self.attached()?;
        Ok(file)
    }

    fn root_path(&self) -> PathBuf {
        Path::new("/proc/self/fd").join(self.root.as_raw_fd().to_string()).join(".")
    }

    pub(crate) fn backups(&self) -> PathBuf {
        // Even after an unplug this can only reach the old filesystem, not
        // the directory under /mnt/usb or a replacement drive.
        self.root_path().join(skyward_backup::FOLDER)
    }

    pub(crate) fn writable(&self, writable: bool) -> Result<()> {
        if writable {
            self.attached()?;
        } else if mount_at(&self.point)? != Some(self.mount_id) {
            // An unplug already detached the mount from the recovery.
            return Ok(());
        }
        let result = mount::remount_writable(&self.root_path(), writable);
        if !writable && result.is_err() && mount_at(&self.point)? == Some(self.mount_id) {
            // If the filesystem refuses read-only, stop exposing it. The
            // probe will notice the missing mount and reopen it read-only.
            if let Err(detach) = mount::umount_detach(&self.root_path())
                && mount_at(&self.point)? == Some(self.mount_id)
            {
                return Err(detach).context("detach the USB drive after its read-only remount failed");
            }
        }
        result
    }

    pub(crate) fn sync(&self) -> Result<()> {
        self.attached()?;
        if unsafe { libc::syncfs(self.root.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("sync the USB drive");
        }
        self.attached()
    }
}

fn file_mount(file: &File) -> Result<i32> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).context("identify the opened USB mount")?;
    info.lines().find_map(|line| line.strip_prefix("mnt_id:")).and_then(|id| id.trim().parse().ok())
        .context("the opened file has no mount ID")
}

pub(crate) fn mount_at(point: &Path) -> Result<Option<i32>> {
    Ok(mount::mount_ids()?.into_iter().rev().find(|(_, at)| at == point).map(|(id, _)| id))
}
