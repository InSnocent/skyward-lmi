//! Recovery settings on unencrypted metadata, available before unlocking data.

use anyhow::{ensure, Context, Result};
use rustix::fs::{self as at, Mode, OFlags};
use rustix::io::Errno;
use skyward_files::Ownership;
use skyward_ui::settings::{Kept, Settings};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

const METADATA: &str = "/metadata";
const FOLDER: &str = "skyward";
const NAME: &str = "settings.json";
const MAX_BYTES: u64 = 64 * 1024;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(target_os = "android")]
pub fn read() -> Result<Kept> {
    match with_metadata(|| read_file(Path::new(METADATA))) {
        Ok(kept) => Ok(kept),
        Err(error) => {
            skyward_sys::kmsg(&format!("the settings were not read: {error:#}"))?;
            Ok(Kept::Unreadable)
        }
    }
}

#[cfg(target_os = "android")]
pub fn write(settings: &Settings) -> Result<()> {
    with_metadata(|| {
        skyward_vab::metadata_writable(true)?;
        let wrote = write_file(Path::new(METADATA), settings);
        let readonly = skyward_vab::metadata_writable(false);
        wrote?;
        readonly
    })
}

fn folder(root: &Path, create: bool) -> Result<Option<File>> {
    let root = File::from(at::open(root, DIRECTORY, Mode::empty()).context("open the settings volume")?);
    let open = || at::openat(&root, FOLDER, DIRECTORY, Mode::empty());
    match open() {
        Ok(folder) => Ok(Some(folder.into())),
        Err(Errno::NOENT) if create => {
            match at::mkdirat(&root, FOLDER, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(error).context("create the settings directory"),
            }
            root.sync_all().context("sync the settings volume")?;
            Ok(Some(File::from(open().context("open the new settings directory")?)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error).context("open the settings directory"),
    }
}

fn read_file(root: &Path) -> Result<Kept> {
    let Some(folder) = folder(root, false)? else { return Ok(Kept::Nothing) };
    let file = match crate::storage::open_regular(&folder, OsStr::new(NAME)) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Kept::Nothing),
        Err(error) => return Err(error).context("open the settings file"),
    };
    ensure!(file.metadata()?.len() <= MAX_BYTES, "the settings file is too large");
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text).context("read the settings file")?;
    ensure!(text.len() as u64 <= MAX_BYTES, "the settings file is too large");
    match Settings::read(&text) {
        Ok(settings) => Ok(Kept::Settings(settings)),
        Err(error) => {
            #[cfg(target_os = "android")]
            let _ = skyward_sys::kmsg(&format!("the settings JSON is invalid: {error:#}"));
            #[cfg(not(target_os = "android"))]
            let _ = error;
            Ok(Kept::Nothing)
        }
    }
}

fn write_file(root: &Path, settings: &Settings) -> Result<()> {
    let text = settings.text();
    ensure!(text.len() as u64 <= MAX_BYTES, "the settings are too large to save");
    let folder = folder(root, true)?.context("no settings directory")?;
    crate::wallpaper::replace(&folder, NAME, Ownership::Kept, |output| {
        output.write_all(text.as_bytes())?;
        Ok(())
    })
}

/// Mount only the metadata volume, leaving its original mount in place.
#[cfg(target_os = "android")]
fn with_metadata<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    use skyward_block::mount;

    let point = Path::new(METADATA);
    let device = Path::new(skyward_block::BY_NAME).join("metadata");
    if let Some(mounted) = mount::visible(point)? {
        ensure!(mounted.device == skyward_block::devnum(&device)?, "unexpected filesystem mounted at /metadata");
        return work();
    }
    mount::mount_ro(&device, point)?;
    let done = work();
    mount::umount(point)?;
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn metadata_settings_round_trip_preserves_existing_full_settings_callers() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_file(root.path()).unwrap(), Kept::Nothing);
        let mut settings = Settings { dark: true, brightness: 0.6, ..Settings::default() };
        settings.install.check_signature = true;
        settings.file_transfer = false;
        write_file(root.path(), &settings).unwrap();
        assert_eq!(read_file(root.path()).unwrap(), Kept::Settings(settings));
    }

    #[test]
    fn metadata_settings_reject_links_and_bounded_reads_keep_corrupt_files_available() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join(FOLDER)).unwrap();
        assert!(write_file(root.path(), &Settings::default()).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_file(root.path().join(FOLDER)).unwrap();
        write_file(root.path(), &Settings::default()).unwrap();
        let saved = root.path().join(FOLDER).join(NAME);
        fs::write(&saved, b"broken JSON").unwrap();
        assert_eq!(read_file(root.path()).unwrap(), Kept::Nothing);
        assert_eq!(fs::read(&saved).unwrap(), b"broken JSON");
        fs::write(&saved, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(read_file(root.path()).is_err());
        fs::remove_file(&saved).unwrap();
        let target = outside.path().join(NAME);
        fs::write(&target, Settings::default().text()).unwrap();
        symlink(&target, &saved).unwrap();
        assert!(read_file(root.path()).is_err());
    }

    #[test]
    fn failed_metadata_settings_write_keeps_the_previous_record() {
        let root = tempfile::tempdir().unwrap();
        let saved = Settings { dark: true, ..Settings::default() };
        write_file(root.path(), &saved).unwrap();
        let opened = folder(root.path(), false).unwrap().unwrap();
        let failed = crate::wallpaper::replace(&opened, NAME, Ownership::Kept, |output| {
            output.write_all(b"{partial")?;
            anyhow::bail!("simulated interrupted settings write")
        });
        assert!(failed.is_err());
        assert_eq!(read_file(root.path()).unwrap(), Kept::Settings(saved));
        assert_eq!(fs::read_dir(root.path().join(FOLDER)).unwrap().count(), 1);
    }
}
