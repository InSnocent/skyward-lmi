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
    let mut current =
        File::from(at::open(root, DIRECTORY, Mode::empty()).context("open internal storage")?);
    for part in relative.components() {
        let Component::Normal(name) = part else {
            anyhow::bail!("the path leaves owner internal storage")
        };
        current = File::from(
            at::openat(&current, name, DIRECTORY, Mode::empty())
                .context("open a shared storage folder")?,
        );
    }
    Ok(current)
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
/// in the picker cannot substitute another image.
pub(crate) fn open_owner_image(owner: &Path, shared: &Path, selected: &Path) -> Result<File> {
    let relative = selected.strip_prefix(owner).or_else(|_| selected.strip_prefix(shared))
        .context("choose an image from owner internal storage")?;
    let name = relative.file_name().context("choose an image file")?;
    let parent = relative.parent().context("the image has no parent folder")?;
    let directory = open_folder(owner, parent)?;
    let file = open_regular(&directory, name).context("open the selected image in owner storage")?;
    let opened = file.metadata()?;
    let shown = fs::symlink_metadata(selected)?;
    ensure!(shown.is_file() && (shown.dev(), shown.ino()) == (opened.dev(), opened.ino()),
        "the selected image changed or is outside owner internal storage");
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

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
            open_owner_image(&owner, &shared, &path).unwrap().read_to_end(&mut contents).unwrap();
            assert_eq!(contents, b"current boot");
        }
        fs::remove_file(shared.join(leaf)).unwrap();
        fs::write(shared.join(leaf), b"different file at the mount point").unwrap();
        assert!(open_owner_image(&owner, &shared, &shared.join(leaf)).is_err());
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
            assert!(open_owner_image(&owner, &owner, &path).is_err(), "{}", path.display());
        }
    }
}
