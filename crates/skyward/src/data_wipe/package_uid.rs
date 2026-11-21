//! Resolve installed APK ownership before making any Keystore change.

use super::{
    DataRoot,
    identity::{self, Attrs, Value, Xml, integer, value, visit_xml},
    mount_id,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{ErrorKind, Read};
use std::path::Path;

const MAX_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;
const APP_ID_LIMIT: u32 = 100_000;
const SYSTEM_FLAGS: i32 = 1 | (1 << 7);
const PRIVILEGED: i32 = 1 << 3;

// AOSP PackageManager selects the interrupted-write backup first.
pub(super) const REGISTRY_FILES: [&str; 3] = [
    "packages-backup.xml",
    "packages.xml",
    "packages.xml.reservecopy",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    Removable,
    System,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Assignment {
    id: u32,
    shared: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Registry {
    owners: BTreeMap<u32, Owner>,
    digest: [u8; 32],
    users: BTreeMap<u32, i32>,
}

impl Registry {
    pub(super) fn read(root: &DataRoot) -> Result<Self> {
        let users = identity::check(root)?;
        let system = root
            .open_relative(Path::new("system"))?
            .context("missing Android system data")?;
        for name in REGISTRY_FILES {
            match crate::storage::open_regular(&system, OsStr::new(name)) {
                Ok(file) => {
                    ensure!(
                        mount_id(&file)? == root.mount,
                        "package registry is on another mount"
                    );
                    let mut bytes = Vec::new();
                    file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() as u64 <= MAX_REGISTRY_BYTES,
                        "package registry is too large"
                    );
                    return Self::parse(&bytes, users)
                        .context("inspect the active Android package registry");
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("open the Android package registry"),
            }
        }
        bail!("missing Android package registry");
    }

    fn parse(bytes: &[u8], users: BTreeMap<u32, i32>) -> Result<Self> {
        let mut active = BTreeMap::new();
        let mut updated = BTreeMap::new();
        let mut shared = BTreeSet::new();
        let mut root = false;
        visit_xml(bytes, 500_000, |event| {
            let Xml::Start(depth, tag, attrs) = event else {
                return Ok(());
            };
            match (depth, tag) {
                (0, b"packages") => {
                    ensure!(!root, "duplicate Android package registry root");
                    root = true;
                }
                (0, _) => bail!("invalid Android package registry root"),
                (1, b"package") => {
                    let name = required_text(attrs, b"name")?;
                    let setting = (assignment(attrs)?, owner(attrs)?);
                    ensure!(
                        active.insert(name.to_owned(), setting).is_none(),
                        "duplicate Android package record"
                    );
                }
                (1, b"updated-package") => {
                    let name = required_text(attrs, b"name")?;
                    let base = required_text(attrs, b"codePath")?;
                    ensure!(
                        system_path(base),
                        "invalid base path for an updated system APK"
                    );
                    ensure!(
                        updated
                            .insert(name.to_owned(), assignment(attrs)?)
                            .is_none(),
                        "duplicate updated system package record"
                    );
                }
                (1, b"shared-user") => {
                    required_text(attrs, b"name")?;
                    let assignment = assignment(attrs)?;
                    ensure!(!assignment.shared, "invalid Android shared UID record");
                    ensure!(shared.insert(assignment.id), "duplicate shared UID record");
                }
                _ => {}
            }
            Ok(())
        })?;
        ensure!(root && !active.is_empty(), "empty Android package registry");
        for (name, base_assignment) in updated {
            if let Some((current_assignment, current_owner)) = active.get_mut(&name) {
                ensure!(
                    *current_assignment == base_assignment,
                    "updated system app changed its Android UID"
                );
                *current_owner = Owner::System;
            } else {
                active.insert(name, (base_assignment, Owner::System));
            }
        }
        let mut index = BTreeMap::new();
        for (assignment, owner) in active.into_values() {
            ensure!(
                shared.contains(&assignment.id) == assignment.shared,
                "inconsistent shared UID in the Android package registry"
            );
            match index.entry(assignment.id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(owner);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    ensure!(
                        assignment.shared,
                        "multiple Android packages claim a dedicated UID"
                    );
                    if *entry.get() != owner {
                        entry.insert(Owner::Unknown);
                    }
                }
            }
        }
        Ok(Self {
            owners: index,
            digest: Sha256::digest(bytes).into(),
            users,
        })
    }

    pub(super) fn users(&self) -> impl Iterator<Item = u32> + '_ {
        self.users.keys().copied()
    }

    pub(super) fn should_clear(&self, full_uid: u32) -> Result<bool> {
        ensure!(full_uid <= i32::MAX as u32, "invalid Android Keystore UID");
        let id = match full_uid % APP_ID_LIMIT {
            0..=9_999 => return Ok(false),
            aid @ 10_000..=19_999 => aid,
            // A sandbox UID is the owning app ID plus 10000.
            aid @ 20_000..=29_999 => aid - 10_000,
            _ => bail!("Keystore UID has no stable Android package owner"),
        };
        ensure!(
            self.users.contains_key(&(full_uid / APP_ID_LIMIT)),
            "Keystore app UID belongs to an unrecognized Android user"
        );
        match self.owners.get(&id) {
            Some(Owner::Removable) => Ok(true),
            Some(Owner::System) => Ok(false),
            Some(Owner::Unknown) | None => bail!("Keystore UID has no unambiguous APK owner"),
        }
    }
}

fn assignment(attrs: &Attrs<'_>) -> Result<Assignment> {
    let (id, shared) = match (integer(attrs, b"userId")?, integer(attrs, b"sharedUserId")?) {
        (Some(id), None) => (id, false),
        (None, Some(id)) => (id, true),
        _ => bail!("Android package record has no unique UID"),
    };
    ensure!(
        (1..APP_ID_LIMIT as i32).contains(&id),
        "invalid Android package app ID"
    );
    Ok(Assignment {
        id: id as u32,
        shared,
    })
}

fn required_text<'a>(attrs: &Attrs<'a>, field: &[u8]) -> Result<&'a str> {
    match value(attrs, field) {
        Some(Value::Text(bytes)) if !bytes.is_empty() => Ok(std::str::from_utf8(bytes)?),
        _ => bail!("missing text field in Android package record"),
    }
}

fn owner(attrs: &Attrs<'_>) -> Result<Owner> {
    let path = required_text(attrs, b"codePath")?;
    let public = integer(attrs, b"publicFlags")?;
    let private = integer(attrs, b"privateFlags")?;
    if system_path(path)
        || public.is_some_and(|flags| flags & SYSTEM_FLAGS != 0)
        || private.is_some_and(|flags| flags & PRIVILEGED != 0)
    {
        return Ok(Owner::System);
    }
    if public.is_some()
        && private.is_some()
        && canonical_path(path)
        && path.starts_with("/data/app/")
    {
        return Ok(Owner::Removable);
    }
    Ok(Owner::Unknown)
}

fn canonical_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|path| {
        path.split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
    })
}

fn system_path(path: &str) -> bool {
    canonical_path(path)
        && [
            "/system/",
            "/system_ext/",
            "/product/",
            "/vendor/",
            "/odm/",
            "/apex/",
            "/oem/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGES: &[u8] = br#"<packages>
        <package name="fixture.app" codePath="/data/app/app" publicFlags="4" privateFlags="0" userId="10001"/>
        <package name="fixture.preload" codePath="/product/app/preload" publicFlags="1" privateFlags="0" userId="10002"/>
        <package name="fixture.updated" codePath="/data/app/updated" publicFlags="4" privateFlags="0" userId="10003"/>
        <updated-package name="fixture.updated" codePath="/system/app/updated" userId="10003"/>
        <package name="fixture.priv" codePath="/data/app/priv" publicFlags="4" privateFlags="8" userId="10004"/>
        <package name="fixture.shared.a" codePath="/data/app/a" publicFlags="4" privateFlags="0" sharedUserId="10005"/>
        <package name="fixture.shared.b" codePath="/system/app/b" publicFlags="1" privateFlags="0" sharedUserId="10005"/>
        <shared-user name="fixture.shared" userId="10005"/>
        <package name="fixture.adopted" codePath="/mnt/expand/0000/app/c" publicFlags="4" privateFlags="0" userId="10006"/>
        <package name="fixture.alpha" codePath="/data/app/d" publicFlags="4" privateFlags="0" sharedUserId="10007"/>
        <package name="fixture.beta" codePath="/data/app/e" publicFlags="4" privateFlags="0" sharedUserId="10007"/>
        <shared-user name="fixture.alpha.shared" userId="10007"/>
    </packages>"#;

    fn parse_fixture(bytes: &[u8]) -> Result<Registry> {
        Registry::parse(bytes, BTreeMap::from([(0, 0), (1, 1)]))
    }

    #[test]
    fn only_targets_uids_owned_exclusively_by_removed_apps() {
        let registry = parse_fixture(PACKAGES).unwrap();
        for uid in [10001, 20001, 100_000 + 10001, 10007] {
            assert!(registry.should_clear(uid).unwrap(), "{uid}");
        }
        for uid in [1000, 10002, 10003, 10004] {
            assert!(!registry.should_clear(uid).unwrap(), "{uid}");
        }
        for uid in [10005, 10006, 10099, 99001, 210001, 220001] {
            assert!(registry.should_clear(uid).is_err(), "{uid}");
        }
    }

    #[test]
    fn rejects_conflicting_uid_associations_and_missing_flags() {
        let owner = r#"<package name="fixture.app" codePath="/data/app/fixture" publicFlags="4" privateFlags="0" userId="10001"/>"#;
        let second = r#"<package name="fixture.duplicate" codePath="/data/app/other" publicFlags="4" privateFlags="0" userId="10001"/>"#;
        let xml = format!("<packages>{owner}{second}</packages>");
        assert!(parse_fixture(xml.as_bytes()).is_err());

        let missing = br#"<packages><package name="fixture.unknown" codePath="/data/app/fixture" userId="10001"/></packages>"#;
        assert!(parse_fixture(missing).unwrap().should_clear(10001).is_err());

        let traversal = br#"<packages><package name="fixture.unknown" codePath="/data/app/../local/fixture" publicFlags="4" privateFlags="0" userId="10001"/></packages>"#;
        assert!(
            parse_fixture(traversal)
                .unwrap()
                .should_clear(10001)
                .is_err()
        );
    }

    #[test]
    fn reads_android_binary_package_flags_and_ids() {
        const ABX: &[u8] = b"ABX\x00\x10\
            \x32\xff\xff\x00\x08packages\
            \x32\xff\xff\x00\x07package\
            \x2f\xff\xff\x00\x04name\x00\x07fixture\
            \x2f\xff\xff\x00\x08codePath\x00\x0d/data/app/apk\
            \x6f\xff\xff\x00\x0bpublicFlags\x00\x00\x00\x04\
            \x6f\xff\xff\x00\x0cprivateFlags\x00\x00\x00\x00\
            \x6f\xff\xff\x00\x06userId\x00\x00\x27\x11\
            \x33\x00\x01\x33\x00\x00\x11";
        let registry = parse_fixture(ABX).unwrap();
        assert!(registry.should_clear(10001).unwrap());
        assert!(registry.should_clear(10002).is_err());
    }

    #[test]
    fn interrupted_package_registry_write_uses_the_backup() {
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().join("system");
        std::fs::create_dir_all(folder.join("users")).unwrap();
        std::fs::write(
            folder.join("users/userlist.xml"),
            br#"<users><user id="0"/></users>"#,
        )
        .unwrap();
        std::fs::write(
            folder.join("users/0.xml"),
            br#"<user id="0" serialNumber="0"/>"#,
        )
        .unwrap();
        std::fs::write(folder.join("packages-backup.xml"), PACKAGES).unwrap();
        std::fs::write(folder.join("packages.xml"), b"partial").unwrap();
        let root = DataRoot::open(fixture.path()).unwrap();
        assert!(Registry::read(&root).unwrap().should_clear(10001).unwrap());
        std::fs::write(folder.join("packages-backup.xml"), b"broken").unwrap();
        std::fs::write(folder.join("packages.xml"), PACKAGES).unwrap();
        assert!(Registry::read(&root).is_err());
    }
}
