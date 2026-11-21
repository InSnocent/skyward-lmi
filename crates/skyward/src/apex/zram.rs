//! An installed APEX on a private RAM disk. On lmi the recovery kernel
//! cannot reliably read the installed APEX files through a loop device.
use super::mountable_image;
use anyhow::{Context, Result, ensure};
use skyward_block::mount;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const ZRAM_CONTROL: &str = "/sys/class/zram-control";

pub struct Mounted {
    index: Option<u32>,
    attrs: PathBuf,
    device: Option<(u32, u32)>,
    target: Option<PathBuf>,
    directory: TempDir,
}

impl Mounted {
    pub fn open(path: &Path, target: PathBuf) -> Result<Self> {
        ensure!(!mount::is_mount_point(&target)?, "{} already has a mount", target.display());
        let payload = mountable_image(path)?;
        let filesystem = payload.filesystem;
        let length = payload.length;
        let mut disk = Self::allocate()?;
        let node = disk.initialize(length)?;
        {
            let mut output = OpenOptions::new().write(true).open(&node)
                .context("open the private APEX RAM disk")?;
            payload.copy_to(&mut output).context("copy the installed APEX into RAM")?;
            output.sync_all().context("flush the APEX to the RAM disk")?;
        }

        let mut options = ["ro", "nodev", "nosuid"].map(str::to_owned).to_vec();
        if filesystem == "ext4" {
            options.push("noload".into());
        }
        mount::mount(&node, &target, filesystem, &options)?;
        disk.target = Some(target);
        Ok(disk)
    }

    fn allocate() -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("skyward-apex-").tempdir_in("/dev")
            .context("create the APEX block node directory")?;
        let directory_fd = File::open(directory.path())?;
        ensure!(rustix::fs::fstatfs(&directory_fd)?.f_type == 0x0102_1994,
            "the APEX block node directory is not in tmpfs");

        let control = Path::new(ZRAM_CONTROL);
        ensure!(control.join("hot_remove").is_file(), "this recovery cannot release dynamic zram devices");
        let index: u32 = fs::read_to_string(control.join("hot_add"))
            .context("allocate a RAM disk for the installed APEX")?.trim().parse()
            .context("read the allocated zram index")?;
        let attrs = Path::new("/sys/devices/virtual/block").join(format!("zram{index}"));
        Ok(Self { index: Some(index), attrs, device: None, target: None, directory })
    }

    fn initialize(&mut self, length: u64) -> Result<PathBuf> {
        let name = self.attrs.file_name().expect("zram attributes have a device name");
        ensure!(fs::canonicalize(Path::new("/sys/block").join(name))? == self.attrs,
            "the allocated disk is not a virtual zram device");
        let device = self.device_number()?;
        self.device = Some(device);
        self.require_no_writeback()?;
        ensure!(fs::read_to_string(self.attrs.join("initstate"))?.trim() == "0"
            && fs::read_to_string(self.attrs.join("disksize"))?.trim() == "0",
            "the allocated zram device is already initialized");
        fs::write(self.attrs.join("disksize"), format!("{length}\n")).context("size the APEX RAM disk")?;

        let node = self.directory.path().join("block");
        rustix::fs::mknodat(
            rustix::fs::CWD, &node, rustix::fs::FileType::BlockDevice,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            rustix::fs::makedev(device.0, device.1),
        ).context("create the APEX RAM block node")?;
        ensure!(skyward_block::devnum(&node)? == device && skyward_block::size_bytes(&node)? == length,
            "the block node does not match the newly allocated APEX RAM disk");
        Ok(node)
    }

    fn device_number(&self) -> Result<(u32, u32)> {
        let text = fs::read_to_string(self.attrs.join("dev"))?;
        let (major, minor) = text.trim().split_once(':').context("invalid zram device number")?;
        Ok((major.parse()?, minor.parse()?))
    }

    fn require_no_writeback(&self) -> Result<()> {
        match fs::read_to_string(self.attrs.join("backing_dev")) {
            Ok(backing) => ensure!(backing.trim() == "none", "the APEX RAM disk has a writeback device"),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read the APEX RAM disk writeback state"),
        }
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        if let Some(target) = &self.target {
            ensure!(mount::visible(target)?.map(|mount| mount.device) == self.device,
                "the APEX mount at {} changed", target.display());
            mount::umount(target)
                .with_context(|| format!("unmount the installed APEX at {}", target.display()))?;
            self.target = None;
        }
        if let Some(index) = self.index {
            if let Some(device) = self.device {
                ensure!(self.device_number()? == device, "the APEX RAM disk identity changed");
                self.require_no_writeback()?;
            }
            fs::write(Path::new(ZRAM_CONTROL).join("hot_remove"), format!("{index}\n"))
                .context("release the APEX RAM disk")?;
            self.index = None;
        }
        Ok(())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            let _ = skyward_sys::kmsg(&format!("APEX RAM disk cleanup failed: {error:#}"));
        }
    }
}
