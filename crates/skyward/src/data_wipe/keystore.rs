//! Identify the same Keystore directory and database from Recovery and its service.

use super::{
    DataRoot,
    app_keys::{KeystoreFiles, same_file},
    entries, mount_id,
};
use crate::storage::{DIRECTORY, open_regular};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self as at, FileType, Mode};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};

pub(crate) struct KeystoreTarget<'a> {
    files: &'a KeystoreFiles,
    path: PathBuf,
}

impl<'a> KeystoreTarget<'a> {
    pub(crate) const UID: u32 = 1017;

    pub(crate) fn open(
        root: &DataRoot,
        data_path: &Path,
        files: &'a KeystoreFiles,
    ) -> Result<Self> {
        let directory = &files.directory;
        let uid_is_keystore =
            |metadata: &Metadata| metadata.uid() == Self::UID && metadata.gid() == Self::UID;
        let metadata = directory.metadata()?;
        ensure!(
            uid_is_keystore(&metadata) && metadata.mode() & 0o777 == 0o700,
            "Android Keystore directory owner or mode is unexpected"
        );
        for entry in entries(directory)? {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            match super::entry_type(directory, entry.file_name())? {
                FileType::Directory if name.as_encoded_bytes().starts_with(b"user_") => {
                    let child = root
                        .open_child(directory, name)?
                        .context("invalid legacy Android Keystore directory")?;
                    let metadata = child.metadata()?;
                    ensure!(
                        uid_is_keystore(&metadata) && metadata.mode() & 0o022 == 0,
                        "unexpected legacy Keystore directory permissions"
                    );
                    for entry in entries(&child)? {
                        let entry = entry?;
                        check_file(
                            root,
                            &child,
                            OsStr::from_bytes(entry.file_name().to_bytes()),
                        )?;
                    }
                }
                FileType::RegularFile => check_file(root, directory, name)?,
                _ => anyhow::bail!("unsupported entry in the Android Keystore directory"),
            }
        }
        let target = Self {
            files,
            path: data_path.join("misc/keystore"),
        };
        target.check_path()?;
        Ok(target)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn check_path(&self) -> Result<()> {
        let directory = File::from(at::open(&self.path, DIRECTORY, Mode::empty())?);
        self.files.check(&directory)
    }

    pub(crate) fn check_service_database(&self, pid: u32) -> Result<()> {
        self.check_path()?;
        let database = self.files.database.metadata()?;
        let entries =
            fs::read_dir(format!("/proc/{pid}/fd")).context("inspect Keystore's open files")?;
        for entry in entries {
            if let Ok(opened) = fs::metadata(entry?.path())
                && same_file(&opened, &database)
            {
                return Ok(());
            }
        }
        anyhow::bail!("Keystore did not open the inspected Android database");
    }
}

fn check_file(root: &DataRoot, directory: &File, name: &OsStr) -> Result<()> {
    let opened = open_regular(directory, name).context("open an Android Keystore file")?;
    let metadata = opened.metadata()?;
    ensure!(
        mount_id(&opened)? == root.mount
            && metadata.nlink() == 1
            && metadata.uid() == KeystoreTarget::UID
            && metadata.mode() & 0o022 == 0,
        "unexpected Android Keystore file owner, mode, links, or mount"
    );
    Ok(())
}
