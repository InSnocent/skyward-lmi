//! Compare the protected Android credential files across native maintenance.

use super::{DataRoot, entries, mount_id};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self as at, AtFlags, FileType};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;

pub(super) fn fingerprint(root: &DataRoot, users: impl Iterator<Item = u32>) -> Result<[u8; 32]> {
    let mut scan = Scanner {
        hash: Sha256::new(),
        bytes_left: 64 * 1024 * 1024,
        entries_left: 32_768,
    };
    for path in [
        "unencrypted",
        "misc/vold",
        "misc/gatekeeper",
        "per_boot",
        "drm/kek.dat",
        "system/locksettings.db",
        "system/locksettings.db-wal",
        "system/locksettings.db-shm",
        "system/locksettings.db-journal",
        "system/gatekeeper.password.key",
        "system/gatekeeper.pattern.key",
        "system/gatekeeper.profile.key",
        "system/users",
    ] {
        scan.path(root, Path::new(path))?;
    }
    for user in users {
        for path in [
            format!("system_de/{user}/spblob"),
            format!("misc_ce/{user}/vold/volume_keys"),
            format!("misc_de/{user}/vold/volume_keys"),
            format!("vendor_de/{user}/fpdata"),
        ] {
            scan.path(root, Path::new(&path))?;
        }
    }
    Ok(scan.hash.finalize().into())
}

struct Scanner {
    hash: Sha256,
    bytes_left: u64,
    entries_left: u32,
}

impl Scanner {
    fn path(&mut self, root: &DataRoot, path: &Path) -> Result<()> {
        let folder = path.parent().context("credential path has no parent")?;
        let name = path.file_name().context("credential path has no name")?;
        let Some(folder) = root.open_relative(folder)? else {
            return Ok(());
        };
        self.digest_bytes(path.as_os_str().as_bytes());
        self.node(root, &folder, name, 0)?;
        Ok(())
    }

    fn node(&mut self, root: &DataRoot, parent: &File, name: &OsStr, depth: usize) -> Result<bool> {
        let stat = match at::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => {
                self.hash.update(b"absent");
                return Ok(false);
            }
            Err(error) => return Err(error).context("stat protected Android credentials"),
        };
        ensure!(
            depth < 64 && self.entries_left > 0,
            "too many protected credential entries"
        );
        self.entries_left -= 1;
        self.digest_bytes(name.as_bytes());
        self.hash.update(stat.st_mode.to_le_bytes());
        self.hash.update(stat.st_ino.to_le_bytes());
        self.hash.update(stat.st_uid.to_le_bytes());
        self.hash.update(stat.st_gid.to_le_bytes());
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let folder = root
                    .open_child(parent, name)?
                    .context("protected credential directory changed")?;
                ensure!(
                    folder.metadata()?.ino() == stat.st_ino,
                    "protected credential directory changed"
                );
                let mut children = entries(&folder)?
                    .map(|entry| Ok(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned()))
                    .collect::<Result<Vec<_>>>()?;
                children.sort_unstable();
                for child in children {
                    ensure!(
                        self.node(root, &folder, &child, depth + 1)?,
                        "protected credential file disappeared"
                    );
                }
                self.hash.update(b"directory end");
            }
            FileType::RegularFile => {
                let file = crate::storage::open_regular(parent, name)
                    .context("read protected Android credential")?;
                let meta = file.metadata()?;
                ensure!(
                    meta.ino() == stat.st_ino && mount_id(&file)? == root.mount,
                    "protected credential file changed mount or inode"
                );
                ensure!(
                    meta.len() <= self.bytes_left,
                    "protected Android credentials are too large to compare"
                );
                self.bytes_left -= meta.len();
                self.hash.update(meta.len().to_le_bytes());
                let copied = io::copy(&mut (&file).take(meta.len() + 1), &mut self.hash)?;
                let after = file.metadata()?;
                ensure!(
                    copied == meta.len()
                        && (meta.ctime(), meta.ctime_nsec()) == (after.ctime(), after.ctime_nsec()),
                    "protected credential file changed during inspection"
                );
            }
            _ => bail!("protected credential is not a directory or regular file"),
        }
        Ok(true)
    }

    fn digest_bytes(&mut self, bytes: &[u8]) {
        self.hash.update((bytes.len() as u64).to_le_bytes());
        self.hash.update(bytes);
    }
}
