use anyhow::{ensure, Context, Result};
use rustix::fs::{self as at, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

#[cfg(target_os = "android")]
pub(crate) const OWNER_STORAGE: &str = "/data/media/0";
pub(crate) const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(crate) fn open_folder(root: &Path, relative: &Path) -> Result<File> {
    let directory = File::from(at::open(root, DIRECTORY, Mode::empty()).context("open internal storage")?);
    open_folder_in(directory, relative)
}

fn open_folder_in(mut directory: File, relative: &Path) -> Result<File> {
    for part in relative.components() {
        let Component::Normal(name) = part else {
            anyhow::bail!("the path leaves the selected storage");
        };
        directory = File::from(
            at::openat(&directory, name, DIRECTORY, Mode::empty()).context("open a storage folder")?,
        );
    }
    Ok(directory)
}

pub(crate) fn open_regular_in(root: File, relative: &Path) -> Result<File> {
    let name = relative.file_name().context("choose a regular file")?;
    let parent = relative.parent().context("the file has no parent folder")?;
    let directory = open_folder_in(root, parent)?;
    open_regular(&directory, name).context("open the selected file")
}

pub(crate) fn open_regular(folder: &File, name: &OsStr) -> io::Result<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = File::from(at::openat(folder, name, flags, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(ErrorKind::InvalidData, "not a regular file"));
    }
    Ok(file)
}

/// `/sdcard` is a bind mount in recovery. Resolve either name relative to the
/// owner's directory, then keep the opened file through inspection so a rename
/// in the picker cannot substitute another file.
pub(crate) fn open_owner_file(owner: &Path, shared: &Path, selected: &Path) -> Result<File> {
    let relative = selected.strip_prefix(owner).or_else(|_| selected.strip_prefix(shared))
        .context("choose a file from owner internal storage")?;
    let directory = open_folder(owner, Path::new(""))?;
    let file = open_regular_in(directory, relative).context("open the selected file in owner storage")?;
    let opened = file.metadata()?;
    let shown = fs::symlink_metadata(selected)?;
    ensure!(shown.is_file() && (shown.dev(), shown.ino()) == (opened.dev(), opened.ino()),
        "the selected file changed or is outside owner internal storage");
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    #[test]
    fn usb_zip_is_opened_from_the_selected_directory_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let point = tmp.path().join("usb");
        let former = tmp.path().join("former");
        fs::create_dir_all(point.join("ROMs")).unwrap();
        fs::write(point.join("ROMs/update.zip"), b"selected").unwrap();
        symlink("ROMs", point.join("linked-folder")).unwrap();
        symlink("update.zip", point.join("ROMs/linked.zip")).unwrap();
        let root = File::open(&point).unwrap();
        fs::rename(&point, &former).unwrap();
        fs::create_dir_all(point.join("ROMs")).unwrap();
        fs::write(point.join("ROMs/update.zip"), b"replacement").unwrap();

        let mut contents = Vec::new();
        open_regular_in(root.try_clone().unwrap(), Path::new("ROMs/update.zip"))
            .unwrap().read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"selected");
        for path in ["../usb/ROMs/update.zip", "/ROMs/update.zip", "linked-folder/update.zip", "ROMs/linked.zip", "ROMs"] {
            assert!(open_regular_in(root.try_clone().unwrap(), Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn img_picker_and_file_manager_open_the_same_owner_file() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = tmp.path().join("data/media/0");
        let shared = tmp.path().join("sdcard");
        let leaf = Path::new("Download/boot.img");
        for root in [&owner, &shared] { fs::create_dir_all(root.join("Download")).unwrap(); }
        fs::write(owner.join(leaf), b"current boot").unwrap();
        // A bind mount gives the same inode two names; realpath retains the mount alias.
        fs::hard_link(owner.join(leaf), shared.join(leaf)).unwrap();
        for path in [shared.join(leaf), owner.join(leaf)] {
            let mut contents = Vec::new();
            open_owner_file(&owner, &shared, &path).unwrap().read_to_end(&mut contents).unwrap();
            assert_eq!(contents, b"current boot");
        }
        fs::remove_file(shared.join(leaf)).unwrap();
        fs::write(shared.join(leaf), b"different file at the mount point").unwrap();
        assert!(open_owner_file(&owner, &shared, &shared.join(leaf)).is_err());
    }

    #[test]
    fn img_picker_refuses_links_traversal_and_other_owners() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = tmp.path().join("0");
        let other = tmp.path().join("10");
        for dir in [&owner, &other] { fs::create_dir(dir).unwrap(); }
        fs::write(owner.join("ok.img"), b"owner").unwrap();
        fs::write(other.join("other.img"), b"other user").unwrap();
        symlink(owner.join("ok.img"), owner.join("link.img")).unwrap();
        symlink(&owner, owner.join("linked-folder")).unwrap();
        for path in [other.join("other.img"), owner.join("../0/ok.img"), owner.join("link.img"),
                     owner.join("linked-folder/ok.img"), owner.clone()] {
            assert!(open_owner_file(&owner, &owner, &path).is_err(), "{}", path.display());
        }
    }
}
