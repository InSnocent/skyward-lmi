//! Clear selected contents without leaving the opened lmi data mount.

use crate::storage::DIRECTORY;
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self as at, AtFlags, Dir, DirEntry, FileType, Mode, OFlags, StatVfsMountFlags};
use rustix::io::Errno;
use std::ffi::{CStr, OsStr};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

mod app_keys;
mod credentials;
mod identity;
mod package_uid;
#[cfg(target_os = "android")]
mod keystore;
#[cfg(target_os = "android")]
pub(crate) use keystore::KeystoreTarget;

pub(crate) const APP_CACHES: [&str; 2] = ["cache", "code_cache"];
pub(crate) const SYSTEM_CACHES: [&str; 3] = ["system/package_cache", "resource-cache", "cache"];
const ART_CACHES: [&str; 2] = ["dalvik-cache", "misc/apexdata/com.android.art/dalvik-cache"];

#[derive(Clone, Copy)]
pub(crate) enum Group {
    /// Data contents, keeping shared storage and the current encryption state.
    DataKeepMedia,
    AppData,
    Media,
    AppCaches,
    SystemCaches,
    Art,
}

impl Group {
    fn clears_entries(self) -> bool {
        matches!(self, Self::DataKeepMedia | Self::AppData | Self::Media | Self::AppCaches)
    }
}

/// A keep-media wipe cannot be combined with any group that writes under media.
pub(crate) fn validate_groups(groups: impl IntoIterator<Item = Group>) -> Result<()> {
    let mut keeps_media = false;
    let mut clears_media = false;
    for group in groups {
        keeps_media |= matches!(group, Group::DataKeepMedia);
        clears_media |= matches!(group, Group::Media | Group::AppCaches);
    }
    ensure!(!keeps_media || !clears_media,
        "Data (keep internal storage) conflicts with Internal storage and App caches");
    Ok(())
}

/// A group whose directories were walked before any selected group is changed.
pub(crate) struct Planned<'root> {
    source: &'root DataRoot,
    group: Group,
    paths: Vec<PathBuf>,
    total: u64,
    keys_checked: bool,
}

impl Planned<'_> {
    fn check_root(&self, root: &DataRoot) -> Result<()> {
        ensure!(std::ptr::eq(self.source, root), "the wipe plan belongs to a different open data root");
        Ok(())
    }
}

/// App ownership and protected credentials checked before a broad data wipe.
pub(crate) struct KeyCleanup {
    owners: package_uid::Registry,
    before: app_keys::Snapshot,
    store: app_keys::KeystoreFiles,
    protected_files: [u8; 32],
}

impl KeyCleanup {
    pub(crate) fn prepare(root: &DataRoot, scratch_parent: &Path) -> Result<Self> {
        let owners = package_uid::Registry::read(root)?;
        #[cfg(target_os = "android")]
        root.check_lmi_user_keys(owners.users())?;
        let protected_files = credentials::fingerprint(root, owners.users())?;
        let store = app_keys::KeystoreFiles::open(root)?;
        let before = Self::inspect(root, scratch_parent, &owners, &store)?;
        Ok(Self { owners, before, store, protected_files })
    }

    /// The maintenance worker returns only after stopping its Keystore.
    /// An error leaves the file deletion plan unverified.
    pub(crate) fn apply(
        self,
        root: &DataRoot,
        plan: &mut Planned<'_>,
        scratch_parent: &Path,
        maintenance: impl FnOnce(&std::collections::BTreeSet<u32>, &app_keys::KeystoreFiles) -> Result<()>,
    ) -> Result<()> {
        plan.check_root(root)?;
        ensure!(matches!(plan.group, Group::DataKeepMedia) && !plan.keys_checked,
            "expected an unverified Data wipe plan");
        ensure!(package_uid::Registry::read(root)? == self.owners, "Android package ownership changed");
        ensure!(credentials::fingerprint(root, self.owners.users())? == self.protected_files,
            "protected Android credential files changed before maintenance");
        self.store.check(&root.open_relative(Path::new("misc/keystore"))?
            .context("missing inspected Keystore directory")?)?;
        maintenance(&self.before.targets, &self.store)?;
        at::syncfs(&root.directory).context("sync the Keystore results")?;
        let after = Self::inspect(root, scratch_parent, &self.owners, &self.store)?;
        self.before.verify_cleared(&after)?;
        root.check_preserved_mounts()?;
        ensure!(package_uid::Registry::read(root)? == self.owners, "Android package ownership changed");
        ensure!(credentials::fingerprint(root, self.owners.users())? == self.protected_files,
            "protected Android credential files changed during app key cleanup");
        #[cfg(target_os = "android")]
        root.check_lmi_user_keys(self.owners.users())?;
        plan.keys_checked = true;
        Ok(())
    }

    fn inspect(root: &DataRoot, scratch_parent: &Path, owners: &package_uid::Registry, store: &app_keys::KeystoreFiles) -> Result<app_keys::Snapshot> {
        let scratch = tempfile::tempdir_in(scratch_parent).context("create private Keystore inspection directory")?;
        #[cfg(target_os = "android")]
        {
            let fd = File::from(at::open(scratch.path(), DIRECTORY, Mode::empty())?);
            ensure!(at::fstatfs(&fd)?.f_type == 0x0102_1994, "Keystore inspection must use tmpfs");
        }
        app_keys::inspect(root, scratch.path(), owners, store)
    }
}

pub(crate) struct DataRoot {
    directory: File,
    mount: u64,
    #[cfg(target_os = "android")]
    live_lmi: bool,
}

impl DataRoot {
    fn open(path: &Path) -> Result<Self> {
        let directory = File::from(
            at::open(path, DIRECTORY, Mode::empty())
                .with_context(|| format!("open {}", path.display()))?,
        );
        ensure!(
            !at::fstatvfs(&directory)?
                .f_flag
                .contains(StatVfsMountFlags::RDONLY),
            "{} is read-only",
            path.display()
        );
        let mount = mount_id(&directory)?;
        Ok(Self { directory, mount, #[cfg(target_os = "android")] live_lmi: false })
    }

    /// Only use the writable F2FS mapping that the lmi decryption path made.
    #[cfg(target_os = "android")]
    pub(crate) fn open_lmi_data() -> Result<Self> {
        use procfs::process::Process;
        use skyward_block::dm;
        use skyward_crypto::data;

        let path = Path::new(data::MOUNT);
        let mut opened = Self::open(path)?;
        let expected = dm::status(data::MAPPED)?.context("decrypt the data partition first")?;
        ensure!(
            !dm::readonly(data::MAPPED)?,
            "the data mapping is read-only"
        );
        // A bind of a userdata subdirectory has the same device and filesystem.
        // Match the open descriptor to its own mount record and require the root.
        let mounts = Process::myself()?.mountinfo()?;
        check_data_mount(&mounts, opened.mount, path, expected)?;
        let physical = Path::new(skyward_block::BY_NAME).join(data::MAPPED);
        let (major, minor) = skyward_block::devnum(&physical)?;
        let mut slaves = fs::read_dir(format!("/sys/dev/block/{}:{}/slaves", expected.0, expected.1))?;
        let slave = slaves.next().transpose()?.context("userdata mapping has no backing device")?;
        let backing = fs::read_to_string(slave.path().join("dev"))?;
        ensure!(slaves.next().transpose()?.is_none() && backing.trim() == format!("{major}:{minor}"),
            "userdata mapping is not backed exclusively by the userdata partition");
        // The metadata-encryption target covers the entire partition in 4 KiB units.
        let sectors = skyward_block::size_bytes(&physical)? / 4096 * 8;
        let targets = dm::table_status(data::MAPPED)?;
        ensure!(sectors > 0 && matches!(&targets[..], [target]
            if target.start == 0 && target.sectors == sectors && target.kind == "default-key"),
            "unexpected userdata device-mapper target");
        opened.live_lmi = true;
        Ok(opened)
    }

    #[cfg(target_os = "android")]
    fn check_lmi_user_keys(&self, users: impl Iterator<Item = u32>) -> Result<()> {
        if !self.live_lmi {
            return Ok(());
        }
        use skyward_crypto::fscrypt;
        for user in users {
            for kind in ["de", "ce"] {
                ensure!(self.open_relative(&Path::new("misc/vold/user_keys").join(kind).join(user.to_string()))?.is_some(),
                    "Android user {user} has no {kind} key directory");
            }
            for base in ["media", "user", "user_de", "system_ce", "system_de", "misc_ce", "misc_de", "vendor_ce", "vendor_de"] {
                let relative = Path::new(base).join(user.to_string());
                if let Some(folder) = self.open_relative(&relative)? {
                    let fd_path = format!("/proc/self/fd/{}", folder.as_raw_fd());
                    ensure!(fscrypt::unlocked(Path::new(&fd_path))?, "Android user {user} still has locked files");
                }
            }
            if user == 0 {
                let folder = self.open_relative(Path::new("data"))?.context("missing Android owner data")?;
                let fd_path = format!("/proc/self/fd/{}", folder.as_raw_fd());
                ensure!(fscrypt::unlocked(Path::new(&fd_path))?, "Android owner files are still locked");
            }
        }
        Ok(())
    }

    pub(crate) fn prepare(&self, group: Group) -> Result<Planned<'_>> {
        let paths = match group {
            Group::DataKeepMedia => {
                // Verify the directories holding user data and keys before
                // any other selected operation can start.
                for path in ["media", "unencrypted", "misc/vold/user_keys"] {
                    ensure!(self.open_relative(Path::new(path))?.is_some(), "missing required data directory {path}");
                }
                self.check_preserved_mounts()?;
                identity::check(self)?;
                vec![PathBuf::new()]
            }
            Group::AppData => self.private_apps()?,
            Group::Media => self.user_folders(Path::new("media"))?,
            Group::AppCaches => self.app_caches()?,
            Group::SystemCaches => SYSTEM_CACHES.iter().map(PathBuf::from).collect(),
            Group::Art => ART_CACHES.iter().map(PathBuf::from).collect(),
        };
        let mut total = 0;
        self.visit_paths(&paths, group, &mut |directory, name, flags| {
            if flags.is_empty() {
                let fd = at::openat(directory, name, OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW, Mode::empty())
                    .with_context(|| format!("hold the selected entry {name:?}"))?;
                ensure!(mount_id(&File::from(fd))? == self.mount, "a separate filesystem is mounted at {name:?}");
            }
            total += 1;
            Ok(())
        })?;
        Ok(Planned { source: self, group, paths, total, keys_checked: false })
    }

    pub(crate) fn execute(&self, planned: Planned<'_>, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        planned.check_root(self)?;
        ensure!(!matches!(planned.group, Group::DataKeepMedia) || planned.keys_checked,
            "confirm Android Keystore cleanup before wiping data");
        self.erase(planned, progress)
    }

    fn erase(&self, planned: Planned<'_>, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        let Planned { group, paths, total, .. } = planned;
        let mut done = 0;
        progress(done, total);
        self.visit_paths(&paths, group, &mut |directory, name, flags| {
            at::unlinkat(directory, name, flags).with_context(|| format!("remove selected entry {name:?}"))?;
            done += 1;
            progress(done, total.max(done));
            Ok(())
        })?;
        if matches!(group, Group::AppData) {
            for path in paths {
                if let Some(app) = self.open_relative(&path)? {
                    forget_cache_inodes(&app)?;
                }
            }
        }
        at::syncfs(&self.directory).context("sync the cleared data")?;
        let total = total.max(done);
        progress(total, total);
        Ok(())
    }

    #[cfg(test)]
    fn clear(&self, group: Group, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        self.erase(self.prepare(group)?, progress)
    }

    fn open_child(&self, parent: &File, name: &OsStr) -> Result<Option<File>> {
        let fd = match at::openat(parent, name, DIRECTORY, Mode::empty()) {
            Ok(fd) => File::from(fd),
            // A missing directory or a link where one was expected is never a target.
            Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("open data directory {name:?}"));
            }
        };
        ensure!(
            mount_id(&fd)? == self.mount,
            "a separate filesystem is mounted at {name:?}"
        );
        Ok(Some(fd))
    }

    fn open_relative(&self, path: &Path) -> Result<Option<File>> {
        let mut folder = self.directory.try_clone()?;
        for component in path.components() {
            let Component::Normal(name) = component else {
                bail!("invalid data path {}", path.display())
            };
            let Some(child) = self.open_child(&folder, name)? else {
                return Ok(None);
            };
            folder = child;
        }
        Ok(Some(folder))
    }

    // The deletion walk skips preserved subtrees. A temporary bind inside
    // those subtrees could otherwise make the recovery use credentials that
    // won't be available to Android at the next boot.
    fn check_preserved_mounts(&self) -> Result<()> {
        let path = fs::read_link(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))?;
        let mounts = procfs::process::Process::myself()?.mountinfo()?;
        for mount in mounts.iter() {
            let Ok(relative) = mount.mount_point.strip_prefix(&path) else { continue };
            if relative.as_os_str().is_empty() { continue; }
            let changes_preserved = PROTECTED.iter().any(|pattern| matches_pattern(relative, pattern, true))
                || relative.ancestors().any(protected_subtree);
            ensure!(!changes_preserved, "another filesystem is mounted within the Android files to preserve");
        }
        Ok(())
    }

    // Android may bind /data/data at /data/user/0. It is safe to skip only
    // when the already-opened directory is exactly the same inode.
    fn is_owner_app_alias(&self, parent: &File, name: &OsStr) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let alias = File::from(at::openat(parent, name, DIRECTORY, Mode::empty())
            .context("open owner app-data alias")?);
        let Some(owner) = self.open_relative(Path::new("data"))? else { return Ok(false) };
        let a = alias.metadata()?;
        let b = owner.metadata()?;
        Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
    }

    fn user_folders(&self, base: &Path) -> Result<Vec<PathBuf>> {
        let Some(users) = self.open_relative(base)? else { return Ok(Vec::new()) };
        Ok(user_ids(&users)?.into_iter().map(|user| base.join(user.to_string())).collect())
    }

    fn app_dirs(&self, base: &Path) -> Result<Vec<PathBuf>> {
        let Some(apps) = self.open_relative(base)? else { return Ok(Vec::new()) };
        let mut paths = Vec::new();
        for entry in entries(&apps)? {
            let entry = entry?;
            let name = entry.file_name();
            if entry_type(&apps, name)? == FileType::Directory {
                paths.push(base.join(OsStr::from_bytes(name.to_bytes())));
            }
        }
        Ok(paths)
    }

    fn private_apps(&self) -> Result<Vec<PathBuf>> {
        let mut paths = self.app_dirs(Path::new("data"))?;
        // /data/user/0 can be a directory or a symlink alias of /data/data.
        for base in ["user", "user_de"].map(Path::new) {
            for user in self.user_folders(base)? {
                paths.extend(self.app_dirs(&user)?);
            }
        }
        Ok(paths)
    }

    fn app_caches(&self) -> Result<Vec<PathBuf>> {
        let mut paths: Vec<_> = self.private_apps()?.into_iter()
            .flat_map(|app| APP_CACHES.map(|cache| app.join(cache))).collect();
        for user in self.user_folders(Path::new("media"))? {
            paths.extend(self.app_dirs(&user.join("Android/data"))?.into_iter().map(|app| app.join("cache")));
        }
        Ok(paths)
    }

    fn visit_paths(&self, paths: &[PathBuf], group: Group, visit: &mut Visitor<'_>) -> Result<()> {
        for path in paths {
            if let Some(cache) = self
                .open_relative(path)
                .with_context(|| format!("open {}", path.display()))?
            {
                self.walk(&cache, path, group, 0, visit)
                    .with_context(|| format!("clear {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn walk(
        &self,
        folder: &File,
        relative: &Path,
        group: Group,
        depth: usize,
        visit: &mut Visitor<'_>,
    ) -> Result<()> {
        ensure!(depth < 128, "data directory nesting exceeds the limit");
        // Read this directory before removing entries so iteration sees a stable list.
        for entry in entries(folder)?.collect::<Result<Vec<_>>>()? {
            let name = entry.file_name();
            let path = relative.join(OsStr::from_bytes(name.to_bytes()));
            if matches!(group, Group::DataKeepMedia) && protected_subtree(&path) {
                continue;
            }
            let kind = entry_type(folder, name)?;
            if kind == FileType::Directory {
                if matches!(group, Group::DataKeepMedia) && path == Path::new("user/0")
                    && self.is_owner_app_alias(folder, OsStr::from_bytes(name.to_bytes()))?
                {
                    continue;
                }
                let child = self
                    .open_child(folder, OsStr::from_bytes(name.to_bytes()))?
                    .context("a data directory changed while opening it")?;
                self.walk(&child, &path, group, depth + 1, visit)?;
                if group.clears_entries() && !(matches!(group, Group::DataKeepMedia) && protected_directory(&path)) {
                    visit(folder, name, AtFlags::REMOVEDIR)?;
                }
            } else if (kind == FileType::RegularFile || group.clears_entries())
                && !(matches!(group, Group::DataKeepMedia) && protected_directory(&path))
            {
                // unlinkat operates on the link itself.
                visit(folder, name, AtFlags::empty())?;
            }
        }
        Ok(())
    }
}

// TWRP's FBE exclusions and Android's user-key layout must stay on disk
// while shared storage remains encrypted with the existing keys.
const PROTECTED: &[&str] = &[
    "media", "unencrypted", "per_boot", "lost+found", "gsi",
    "misc/vold", "misc/gatekeeper", "misc/keystore", "misc/keystore2",
    "misc_ce/*/vold/volume_keys", "misc_de/*/vold/volume_keys",
    "drm/kek.dat", "skyward/systems",
    "system/packages.xml", "system/packages-backup.xml", "system/packages.xml.reservecopy",
    "system/packages.list",
    "system/locksettings.db", "system/locksettings.db-wal",
    "system/locksettings.db-shm", "system/locksettings.db-journal",
    "system/gatekeeper.password.key", "system/gatekeeper.pattern.key",
    "system/gatekeeper.profile.key", "system/users/userlist.xml",
    "system/users/*/gatekeeper.password.key", "system/users/*/gatekeeper.pattern.key",
    "system/users/*/gatekeeper.profile.key", "system/users/*/locksettings.db",
    "system/users/*/locksettings.db-wal", "system/users/*/locksettings.db-shm",
    "system/users/*/locksettings.db-journal", "system_de/*/spblob", "vendor_de/*/fpdata",
];

/// Encryption policies belong to these directory inodes. Preserve the
/// directory and remove its contents, except protected descendants.
const POLICY_DIRECTORIES: &[&str] = &[
    "data", "user/*", "user_de/*", "system_ce/*", "system_de/*",
    "misc_ce/*", "misc_de/*", "vendor_ce/*", "vendor_de/*",
];

fn matches_pattern(path: &Path, pattern: &str, allow_parent: bool) -> bool {
    let mut wanted = pattern.split('/');
    for component in path.components() {
        let Component::Normal(name) = component else { return false };
        let Some(part) = wanted.next() else { return false };
        if part != "*" && name != OsStr::new(part) {
            return false;
        }
    }
    allow_parent || wanted.next().is_none()
}

fn user_identity_file(path: &Path) -> bool {
    let Some(name) = path.strip_prefix("system/users").ok().and_then(Path::file_name) else { return false };
    if path.parent() != Some(Path::new("system/users")) {
        return false;
    }
    let Some(name) = name.to_str() else { return false };
    let name = name.strip_suffix(".backup").or_else(|| name.strip_suffix(".reservecopy")).unwrap_or(name);
    name == "userlist.xml" || name.strip_suffix(".xml")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn protected_subtree(path: &Path) -> bool {
    user_identity_file(path) || PROTECTED.iter().any(|pattern| matches_pattern(path, pattern, false))
}

fn protected_directory(path: &Path) -> bool {
    PROTECTED.iter().any(|pattern| matches_pattern(path, pattern, true))
        || POLICY_DIRECTORIES.iter().any(|pattern| matches_pattern(path, pattern, true))
}

fn forget_cache_inodes(app: &File) -> Result<()> {
    for name in [c"user.inode_cache", c"user.inode_code_cache"] {
        if unsafe { libc::fremovexattr(app.as_raw_fd(), name.as_ptr()) } != 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)) {
                return Err(error).context("forget cached directory inodes");
            }
        }
    }
    Ok(())
}

type Visitor<'a> = dyn FnMut(&File, &CStr, AtFlags) -> Result<()> + 'a;

fn mount_id(directory: &File) -> Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", directory.as_raw_fd()))
        .context("read the data directory mount identity")?;
    let id = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .context("mount ID is unavailable")?;
    id.trim()
        .parse()
        .context("invalid cache directory mount ID")
}

fn entries(folder: &File) -> Result<impl Iterator<Item = Result<DirEntry>>> {
    Ok(Dir::read_from(folder)?.filter_map(|entry| match entry {
        Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
        other => Some(other.context("read data directory entries")),
    }))
}

fn entry_type(directory: &File, name: &CStr) -> Result<FileType> {
    let meta = at::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .with_context(|| format!("inspect data entry {name:?}"))?;
    Ok(FileType::from_raw_mode(meta.st_mode))
}

fn user_id(name: &str) -> Option<u32> {
    let id: u32 = name.parse().ok()?;
    (name == id.to_string()).then_some(id)
}

fn user_ids(folder: &File) -> Result<Vec<u32>> {
    let mut users = Vec::new();
    for entry in entries(folder)? {
        let entry = entry?;
        if let Some(uid) = entry.file_name().to_str().ok().and_then(user_id)
            && entry_type(folder, entry.file_name())? == FileType::Directory
        {
            users.push(uid);
        }
    }
    Ok(users)
}

fn check_data_mount(
    mounts: &procfs::process::MountInfos,
    id: u64,
    path: &Path,
    device: (u32, u32),
) -> Result<()> {
    let mount = mounts
        .iter()
        .find(|mount| mount.mnt_id as u64 == id)
        .context("the opened data mount is no longer visible")?;
    let expected = format!("{}:{}", device.0, device.1);
    ensure!(
        mount.mount_point == path
            && mount.root == "/"
            && mount.fs_type == "f2fs"
            && mount.majmin == expected
            && mount.mount_options.contains_key("rw")
            && mount.super_options.contains_key("rw"),
        "the opened directory is not the root of the writable lmi data mapping"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    fn clear(root: &Path, group: Group, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        DataRoot::open(root)?.clear(group, progress)
    }

    fn create(root: &Path, names: &[&str]) {
        for name in names {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            let secondary = root.join("media/10").is_dir() || names.iter().any(|path| path.starts_with("media/10/"));
            fs::write(file, fixture_content(name, secondary)).unwrap();
        }
    }

    fn unchanged(root: &Path, names: &[&str]) {
        for name in names {
            assert_eq!(
                fs::read(root.join(name)).unwrap(),
                fixture_content(name, root.join("media/10").is_dir()),
                "{name}"
            );
        }
    }

    fn fixture_content(name: &str, has_secondary: bool) -> &[u8] {
        let identity = name.strip_suffix(".backup").or_else(|| name.strip_suffix(".reservecopy")).unwrap_or(name);
        match identity {
            "system/users/userlist.xml" if has_secondary => br#"<users><user id="0"/><user id="10"/></users>"#,
            "system/users/userlist.xml" => br#"<users><user id="0"/></users>"#,
            "system/users/0.xml" => br#"<user id="0" serialNumber="0"/>"#,
            "system/users/10.xml" => br#"<user id="10" serialNumber="47"/>"#,
            _ => name.as_bytes(),
        }
    }

    #[test]
    fn a_keep_media_plan_rejects_groups_that_clear_shared_files() {
        for group in [Group::Media, Group::AppCaches] {
            assert!(validate_groups([Group::DataKeepMedia, group]).is_err());
            assert!(validate_groups([group, Group::DataKeepMedia]).is_err());
        }
        assert!(validate_groups([Group::DataKeepMedia, Group::SystemCaches, Group::Art]).is_ok());
        assert!(validate_groups([Group::Media, Group::AppCaches]).is_ok());
    }

    #[test]
    fn data_wipe_keeps_shared_files_and_encryption_material_for_all_users() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let kept = [
            "media/0/DCIM/photo.jpg",
            "media/10/Download/other-user.txt",
            "media/0/Skyward/backups/backup.toml",
            "unencrypted/key/version",
            "unencrypted/ref",
            "misc/vold/user_keys/ce/0/key",
            "misc/vold/user_keys/de/10/key",
            "misc/vold/volume_keys/adopted",
            "misc_ce/0/vold/volume_keys/adopted-one",
            "misc_ce/10/vold/volume_keys/adopted-two",
            "misc_de/0/vold/volume_keys/adopted-one",
            "misc_de/10/vold/volume_keys/adopted-two",
            "misc/keystore2/persistent.sqlite",
            "misc/gatekeeper/0",
            "system/packages.xml",
            "system/packages-backup.xml",
            "system/packages.xml.reservecopy",
            "system/packages.list",
            "system/locksettings.db",
            "system/locksettings.db-wal",
            "system/users/userlist.xml",
            "system/users/userlist.xml.backup",
            "system/users/userlist.xml.reservecopy",
            "system/users/0.xml",
            "system/users/0.xml.backup",
            "system/users/0.xml.reservecopy",
            "system/users/10.xml",
            "system/users/10.xml.backup",
            "system/users/10.xml.reservecopy",
            "system/users/10/gatekeeper.password.key",
            "system_de/0/spblob/credential",
            "system_de/10/spblob/credential",
            "vendor_de/0/fpdata/fingerprint",
            "drm/kek.dat",
            "per_boot/key_ref",
            "lost+found/fsck",
            "gsi/system.img",
            "skyward/systems/second-system.img",
        ];
        let removed = [
            "data/org.example/files/private",
            "user/10/org.example/databases/accounts.db",
            "user_de/0/org.example/state",
            "system/users/0/settings_global.xml",
            "system/users/10/settings_secure.xml",
            "system_de/0/notification_policy.xml",
            "system_ce/0/accounts_ce.db",
            "misc_ce/0/wifi/networks",
            "misc_de/10/app-data",
            "vendor_ce/0/settings",
            "app/org.example/base.apk",
            "adb/modules/module/file",
            "dalvik-cache/arm64/example.art",
            "ota/pending",
        ];
        create(root, &kept);
        create(root, &removed);
        create(outside.path(), &["outside-sentinel"]);
        symlink(outside.path(), root.join("external-link")).unwrap();
        symlink(root.join("data"), root.join("user/0")).unwrap();
        let media_inode=fs::metadata(root.join("media")).unwrap().ino();
        let user_root=fs::metadata(root.join("system_ce/0")).unwrap().ino();
        clear(root, Group::DataKeepMedia, &mut |_, _| {}).unwrap();

        unchanged(root, &kept);
        unchanged(outside.path(), &["outside-sentinel"]);
        for name in removed { assert!(!root.join(name).exists(), "{name}"); }
        assert!(!root.join("external-link").exists());
        assert!(root.join("user/0").is_symlink());
        assert_eq!(fs::metadata(root.join("media")).unwrap().ino(), media_inode);
        assert_eq!(fs::metadata(root.join("system_ce/0")).unwrap().ino(), user_root);
    }

    #[test]
    fn data_wipe_refuses_missing_or_redirected_encryption_material_before_deletion() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = fixture.path();
        create(root, &["data/org.example/value", "unencrypted/ref", "media/0/fixture"]);
        create(outside.path(), &["user_keys/ce/0/key"]);
        fs::create_dir_all(root.join("misc")).unwrap();
        symlink(outside.path(), root.join("misc/vold")).unwrap();
        assert!(DataRoot::open(root).unwrap().prepare(Group::DataKeepMedia).is_err());
        unchanged(root, &["data/org.example/value", "unencrypted/ref", "media/0/fixture"]);
        unchanged(outside.path(), &["user_keys/ce/0/key"]);
    }

    #[test]
    fn data_wipe_refuses_a_missing_key_directory_before_deletion() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let files = ["data/org.example/value", "unencrypted/ref", "media/0/fixture"];
        create(root, &files);
        assert!(DataRoot::open(root).unwrap().prepare(Group::DataKeepMedia).is_err());
        unchanged(root, &files);
    }

    #[test]
    fn data_wipe_requires_nonempty_identity_files_before_any_deletion() {
        for broken in ["missing-userlist", "empty-owner", "redirected-owner", "missing-secondary"] {
            let fixture = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let root = fixture.path();
            let files = [
                "data/org.example/value", "unencrypted/ref", "media/0/fixture",
                "media/10/fixture", "misc/vold/user_keys/ce/0/key",
                "system/users/userlist.xml", "system/users/0.xml", "system/users/10.xml",
            ];
            create(root, &files);
            match broken {
                "missing-userlist" => fs::remove_file(root.join("system/users/userlist.xml")).unwrap(),
                "empty-owner" => fs::write(root.join("system/users/0.xml"), b"").unwrap(),
                "redirected-owner" => {
                    fs::remove_file(root.join("system/users/0.xml")).unwrap();
                    create(outside.path(), &["identity"]);
                    symlink(outside.path().join("identity"), root.join("system/users/0.xml")).unwrap();
                }
                "missing-secondary" => fs::remove_file(root.join("system/users/10.xml")).unwrap(),
                _ => unreachable!(),
            }
            assert!(DataRoot::open(root).unwrap().prepare(Group::DataKeepMedia).is_err(), "{broken}");
            unchanged(root, &["data/org.example/value", "media/0/fixture", "media/10/fixture"]);
            if broken == "redirected-owner" { unchanged(outside.path(), &["identity"]); }
        }
    }

    #[test]
    fn data_wipe_refuses_an_invalid_or_stale_android_user_registry() {
        for invalid in ["corrupt", "stale-backup", "wrong-record", "stale-serial", "partial", "bad-type", "bad-clock", "noncanonical"] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path();
            let protected = ["media/0/DCIM/photo", "media/10/Pictures/photo", "data/org.example/file"];
            create(root, &protected);
            create(root, &["misc/vold/user_keys/ce/0/key", "unencrypted/ref", "misc_ce/10/value"]);
            let users = root.join("system/users");
            fs::create_dir_all(&users).unwrap();
            fs::write(users.join("userlist.xml"), br#"<users><user id="0"/><user id="10"/></users>"#).unwrap();
            fs::write(users.join("0.xml"), br#"<user id="0" serialNumber="0"/>"#).unwrap();
            fs::write(users.join("10.xml"), br#"<user id="10" serialNumber="47"/>"#).unwrap();
            match invalid {
                "corrupt" => fs::write(users.join("userlist.xml"), [65, 66, 88, 0, 16, 50, 255]).unwrap(),
                "stale-backup" => fs::write(users.join("userlist.xml.backup"), br#"<users><user id="0"/></users>"#).unwrap(),
                "wrong-record" => fs::write(users.join("10.xml"), br#"<user id="11" serialNumber="47"/>"#).unwrap(),
                "stale-serial" => {
                    let dir = File::open(root.join("misc_ce/10")).unwrap();
                    assert_eq!(unsafe { libc::fsetxattr(dir.as_raw_fd(), c"user.serial".as_ptr(), b"46".as_ptr().cast(), 2, 0) }, 0);
                }
                "partial" => fs::write(users.join("10.xml"), br#"<user id="10" serialNumber="47" partial="true"/>"#).unwrap(),
                "bad-type" => fs::write(users.join("10.xml"), br#"<user id="10" serialNumber="47" type="unknown"><userProperties/></user>"#).unwrap(),
                "bad-clock" => fs::write(users.join("10.xml"), br#"<user id="10" serialNumber="47"><lastRequestQuietModeEnabledCall>not-a-time</lastRequestQuietModeEnabledCall></user>"#).unwrap(),
                "noncanonical" => fs::create_dir_all(root.join("system_de/010")).unwrap(),
                _ => unreachable!(),
            };
            let root_handle = DataRoot::open(root).unwrap();
            assert!(root_handle.prepare(Group::DataKeepMedia).is_err(), "{invalid} passed preflight");
            unchanged(root, &protected);
        }
    }

    #[test]
    fn data_wipe_accepts_valid_identity_backup_copies_when_primary_is_absent() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let kept = [
            "unencrypted/ref", "media/0/fixture", "media/10/fixture",
            "misc/vold/user_keys/ce/0/key", "system/users/userlist.xml.reservecopy",
            "system/users/0.xml.backup", "system/users/10.xml.reservecopy",
        ];
        create(root, &kept);
        create(root, &["data/org.example/value"]);
        clear(root, Group::DataKeepMedia, &mut |_, _| {}).unwrap();
        unchanged(root, &kept);
        assert!(!root.join("data/org.example/value").exists());
    }

    #[test]
    #[ignore = "requires a private mount namespace with bind-mount permission"]
    fn owner_bind_alias_is_skipped_but_another_mounted_directory_is_rejected() {
        use std::process::Command;

        for scenario in ["owner alias", "unrelated bind", "temporary key"] {
            let fixture = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let root = fixture.path();
            let kept = [
                "unencrypted/ref", "media/0/photo", "misc/vold/user_keys/ce/0/key",
                "system/users/userlist.xml", "system/users/0.xml",
            ];
            create(root, &kept);
            create(root, &["data/org.example/value"]);
            create(outside.path(), &["outside-value"]);
            fs::create_dir_all(root.join("user/0")).unwrap();
            let (source, target) = match scenario {
                "owner alias" => (root.join("data"), root.join("user/0")),
                "unrelated bind" => (outside.path().to_path_buf(), root.join("user/0")),
                "temporary key" => (outside.path().join("outside-value"), root.join("misc/vold/user_keys/ce/0/key")),
                _ => unreachable!(),
            };
            assert!(Command::new("mount").arg("--bind").arg(&source).arg(&target).status().unwrap().success());
            let handle = DataRoot::open(root).unwrap();
            let planned = handle.prepare(Group::DataKeepMedia);
            if scenario == "owner alias" {
                handle.erase(planned.unwrap(), &mut |_, _| {}).unwrap();
                assert!(!root.join("data/org.example/value").exists());
            } else {
                assert!(planned.is_err(), "an unsafe mount was accepted: {scenario}");
                unchanged(root, &["data/org.example/value"]);
            }
            assert!(Command::new("umount").arg(&target).status().unwrap().success());
            unchanged(root, &kept);
            unchanged(outside.path(), &["outside-value"]);
        }
    }

    #[test]
    fn wipe_plan_cannot_be_applied_to_another_open_data_root() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let private = ["data/org.example/files/protected"];
        create(first.path(), &private);
        create(second.path(), &private);
        let planned_root = DataRoot::open(first.path()).unwrap();
        let other_root = DataRoot::open(second.path()).unwrap();
        let plan = planned_root.prepare(Group::AppData).unwrap();

        assert!(other_root.execute(plan, &mut |_, _| {}).is_err());
        unchanged(first.path(), &private);
        unchanged(second.path(), &private);
    }

    #[test]
    fn clearing_app_data_keeps_shared_files_and_the_application_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let private = [
            "data/org.skyward.demo/files/settings",
            "user/10/org.skyward.demo/databases/demo",
            "user_de/0/org.skyward.demo/no_backup/state",
        ];
        let shared = [
            "media/0/DCIM/photo.jpg",
            "media/0/Android/media/org.skyward.demo/album",
            "media/0/Android/data/org.skyward.demo/files/settings",
            "system/users/0.xml",
            "misc/vold/keyring",
            "app/org.skyward.demo/base.apk",
        ];
        create(root, &private);
        create(root, &shared);
        symlink(root.join("data"), root.join("user/0")).unwrap();
        let app = root.join("data/org.skyward.demo");
        let inode = fs::metadata(&app).unwrap().ino();
        let file = File::open(&app).unwrap();
        assert_eq!(unsafe { libc::fsetxattr(file.as_raw_fd(), c"user.inode_cache".as_ptr(),
            b"inode".as_ptr().cast(), 5, 0) }, 0);
        clear(root, Group::AppData, &mut |_, _| {}).unwrap();

        for item in private { assert!(!root.join(item).exists(), "{item}"); }
        assert_eq!(fs::metadata(&app).unwrap().ino(), inode);
        assert!(root.join("user/10/org.skyward.demo").is_dir());
        assert!(fs::symlink_metadata(root.join("user/0")).unwrap().is_symlink());
        assert_eq!(unsafe { libc::fgetxattr(file.as_raw_fd(), c"user.inode_cache".as_ptr(), std::ptr::null_mut(), 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENODATA));
        unchanged(root, &shared);
    }

    #[test]
    fn clearing_media_keeps_user_roots_and_app_data_and_never_follows_a_link() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let media = ["media/0/Download/test", "media/10/Pictures/fixture", "media/0/Skyward/backups/backup.toml"];
        let kept = ["data/org.skyward.demo/files/state", "user_de/10/org.skyward.demo/state", "media/not-a-user/marker"];
        create(root, &media);
        create(root, &kept);
        create(outside.path(), &["outside-sample"]);
        symlink(outside.path(), root.join("media/0/link")).unwrap();
        symlink(outside.path(), root.join("media/11")).unwrap();
        let inode = fs::metadata(root.join("media/0")).unwrap().ino();
        clear(root, Group::Media, &mut |_, _| {}).unwrap();

        for name in ["media/0", "media/10"] { assert_eq!(fs::read_dir(root.join(name)).unwrap().count(), 0); }
        assert_eq!(fs::metadata(root.join("media/0")).unwrap().ino(), inode);
        assert!(root.join("media/11").is_symlink());
        unchanged(root, &kept);
        unchanged(outside.path(), &["outside-sample"]);
    }

    #[test]
    fn clearing_art_touches_only_the_compiled_cache_files() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let compiled = ["dalvik-cache/arm64/one", "misc/apexdata/com.android.art/dalvik-cache/two"];
        let kept = ["ota/install-file", "app/org.skyward.demo/base.apk", "media/0/Download/rom.zip"];
        create(root, &compiled);
        create(root, &kept);
        clear(root, Group::Art, &mut |_, _| {}).unwrap();
        for item in compiled { assert!(!root.join(item).exists(), "{item}"); }
        assert!(root.join("dalvik-cache/arm64").is_dir());
        unchanged(root, &kept);
    }

    #[test]
    fn the_data_handle_must_point_to_the_whole_expected_f2fs_mount() {
        use procfs::{FromBufRead, process::MountInfos};

        let records = b"40 1 253:5 / /data rw - f2fs /dev/block/dm-5 rw\n\
                        41 40 253:5 /media/0 /data rw - f2fs /dev/block/dm-5 rw\n\
                        42 40 253:5 / /sdcard rw - f2fs /dev/block/dm-5 rw\n\
                        43 1 253:6 / /data rw - f2fs /dev/block/dm-6 rw\n\
                        44 1 253:5 / /data rw - ext4 /dev/block/dm-5 rw\n\
                        45 1 253:5 / /data ro - f2fs /dev/block/dm-5 ro\n";
        let mounts = MountInfos::from_buf_read(&records[..]).unwrap();
        assert!(check_data_mount(&mounts, 40, Path::new("/data"), (253, 5)).is_ok());
        for id in [41, 42, 43, 44, 45, 46] {
            assert!(
                check_data_mount(&mounts, id, Path::new("/data"), (253, 5)).is_err(),
                "accepted mount {id}"
            );
        }
    }

    #[test]
    fn app_cache_cleanup_preserves_cache_roots_and_apps_private_and_shared_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cached = [
            "data/com.example.notes/cache/images/preview",
            "data/com.example.notes/code_cache/code",
            "user/10/com.example.notes/cache/list",
            "user_de/10/com.example.notes/code_cache/boot",
            "media/0/Android/data/com.example.notes/cache/web/thumb",
        ];
        let kept = [
            "data/com.example.notes/databases/notes.db",
            "user/10/com.example.notes/files/photo",
            "user_de/10/com.example.notes/shared_prefs/prefs.xml",
            "media/0/Android/data/com.example.notes/files/report",
            "media/0/Android/media/com.example.notes/cache/memo",
            "media/0/Android/obb/com.example.notes/cache/asset",
            "media/0/DCIM/photo.jpg",
            "media/0/Download/file.zip",
            "system/package_cache/parsed.xml",
        ];
        create(root, &cached);
        create(root, &kept);
        symlink("../data", root.join("user/0")).unwrap();
        let cache = root.join("data/com.example.notes/cache");
        let cache_inode = cache.metadata().unwrap().ino();
        symlink(root.join("media/0/Download"), cache.join("link")).unwrap();
        let mut progress = None;

        clear(root, Group::AppCaches, &mut |done, total| {
            progress = Some((done, total))
        })
        .unwrap();

        for name in cached {
            assert!(!root.join(name).exists(), "{name} remains");
        }
        assert_eq!(cache.metadata().unwrap().ino(), cache_inode);
        assert!(!cache.join("link").is_symlink());
        assert_eq!(progress, Some((8, 8)));
        unchanged(root, &kept);
    }

    #[test]
    fn app_cache_paths_never_follow_links_at_any_level() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let outside = tmp.path().join("outside");
        fs::create_dir(&root).unwrap();
        create(&outside, &["0/com.example/cache/outside"]);
        let kept = [
            "media/0/Download/com.example/cache/archive",
            "media/0/Download/plain.txt",
            "data/com.example/files/notes.txt",
        ];
        create(&root, &kept);
        fs::create_dir_all(root.join("media/0/Android")).unwrap();
        symlink(
            root.join("media/0/Download"),
            root.join("media/0/Android/data"),
        )
        .unwrap();
        symlink(&outside, root.join("user_de")).unwrap();
        symlink(
            root.join("data/com.example/files"),
            root.join("data/com.example/cache"),
        )
        .unwrap();
        create(&root, &["user/0/com.other/cache/real-cache"]);

        clear(&root, Group::AppCaches, &mut |_, _| {}).unwrap();

        unchanged(&root, &kept);
        unchanged(&outside, &["0/com.example/cache/outside"]);
        assert!(!root.join("user/0/com.other/cache/real-cache").exists());
    }

    #[test]
    fn system_cache_cleanup_removes_regular_files_and_preserves_its_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cached = [
            "system/package_cache/package",
            "resource-cache/overlay/entry",
            "cache/staged",
        ];
        let kept = [
            "system/packages.xml",
            "data/com.example.notes/cache/app",
            "misc/apexdata/com.android.art/dalvik-cache/code",
            "media/0/Download/file.zip",
        ];
        create(root, &cached);
        create(root, &kept);
        let dir = root.join("resource-cache/overlay");
        let inode = dir.metadata().unwrap().ino();
        symlink(root.join("media/0/Download"), dir.join("link")).unwrap();

        clear(root, Group::SystemCaches, &mut |_, _| {}).unwrap();

        for name in cached {
            assert!(!root.join(name).exists(), "{name} remains");
        }
        assert_eq!(dir.metadata().unwrap().ino(), inode);
        assert!(dir.join("link").is_symlink());
        unchanged(root, &kept);
    }

    #[test]
    fn system_caches_do_not_follow_a_link_in_a_parent_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let outside = tmp.path().join("outside");
        fs::create_dir(&root).unwrap();
        create(&outside, &["package_cache/keep.txt"]);
        symlink(&outside, root.join("system")).unwrap();
        create(&root, &["cache/clear.txt"]);

        clear(&root, Group::SystemCaches, &mut |_, _| {}).unwrap();

        unchanged(&outside, &["package_cache/keep.txt"]);
        assert!(!root.join("cache/clear.txt").exists());
    }
}
