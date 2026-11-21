//! Discover app-owned Keystore data without changing the mounted Android store.

use super::{DataRoot, entries, mount_id, package_uid::Registry, user_id};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const UID_PER_USER: u32 = 100_000;
pub(super) const DATABASE: &str = "persistent.sqlite";
const LEGACY_DATABASE: &str = "vpnprofilestore.sqlite";
const MAX_FILE_BYTES: u64 = 96 * 1024 * 1024;

pub(super) struct Snapshot {
    pub(super) targets: BTreeSet<u32>,
    protected: [u8; 32],
}

impl Snapshot {
    pub(super) fn verify_cleared(&self, after: &Self) -> Result<()> {
        ensure!(
            after.targets.is_empty(),
            "app Keystore entries or grants remain"
        );
        ensure!(
            self.protected == after.protected,
            "protected Keystore entries changed"
        );
        Ok(())
    }
}

/// Pin the directory and live database from inventory through verification.
pub(crate) struct KeystoreFiles {
    pub(super) directory: File,
    pub(super) database: File,
}

impl KeystoreFiles {
    pub(super) fn open(root: &DataRoot) -> Result<Self> {
        let directory = open_store(root)?;
        let database = crate::storage::open_regular(&directory, OsStr::new(DATABASE))
            .context("open Android Keystore database")?;
        ensure!(
            mount_id(&database)? == root.mount,
            "Keystore database is on a different mount"
        );
        Ok(Self {
            directory,
            database,
        })
    }

    pub(super) fn check(&self, directory: &File) -> Result<()> {
        Self::require_same(directory, &self.directory)?;
        let database = crate::storage::open_regular(directory, OsStr::new(DATABASE))?;
        Self::require_same(&database, &self.database)
    }

    fn require_same(opened: &File, expected: &File) -> Result<()> {
        ensure!(
            same_file(&opened.metadata()?, &expected.metadata()?)
                && mount_id(opened)? == mount_id(expected)?,
            "Android Keystore path no longer refers to the inspected files"
        );
        Ok(())
    }
}

pub(super) fn same_file(one: &Metadata, other: &Metadata) -> bool {
    (one.dev(), one.ino()) == (other.dev(), other.ino())
}

fn open_store(root: &DataRoot) -> Result<File> {
    let store = root
        .open_relative(Path::new("misc/keystore"))?
        .context("missing Android Keystore directory")?;
    // Probe relative to the validated parent so a symlink cannot hide a
    // second store behind a lookup through a different mount.
    let misc = root
        .open_relative(Path::new("misc"))?
        .context("missing Android misc directory")?;
    ensure!(
        matches!(
            rustix::fs::statat(&misc, "keystore2", rustix::fs::AtFlags::SYMLINK_NOFOLLOW),
            Err(rustix::io::Errno::NOENT)
        ),
        "unsupported second Android Keystore directory"
    );
    Ok(store)
}

pub(super) fn inspect(
    root: &DataRoot,
    scratch: &Path,
    owners: &Registry,
    files: &KeystoreFiles,
) -> Result<Snapshot> {
    files.check(&open_store(root)?)?;
    let store = &files.directory;
    let modern = snapshot(root, store, scratch, DATABASE, Some(&files.database))?
        .context("missing Android Keystore database")?;
    let mut protected = Sha256::new();
    let mut targets = BTreeSet::new();
    let mut removed_keys = BTreeSet::<i64>::new();
    // Keystore may collect old blobs and interrupted creations as it starts.
    // Compare the live key and the newest blob of each subcomponent.
    hash_rows(
        &modern,
        &mut protected,
        "keyentry",
        "SELECT id, key_type, domain, namespace, alias, state, km_uuid
         FROM keyentry WHERE state = 1 ORDER BY id",
        |row| {
            let is_app = row.get::<_, i32>(1)? == 0 && row.get::<_, Option<i32>>(2)? == Some(0);
            if is_app {
                let uid = row
                    .get(3)
                    .context("invalid app namespace in Android Keystore")?;
                if owners.should_clear(uid)? {
                    targets.insert(uid);
                    removed_keys.insert(row.get(0)?);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;

    for (section, sql) in [
        ("blobentry",
         "SELECT k.id, b.id, b.subcomponent_type, b.blob
          FROM keyentry k JOIN blobentry b ON b.keyentryid = k.id
          WHERE k.state = 1 AND b.id = (SELECT max(c.id) FROM blobentry c
             WHERE c.keyentryid = k.id AND c.subcomponent_type = b.subcomponent_type)
          ORDER BY k.id, b.subcomponent_type"),
        ("blobmetadata",
         "SELECT k.id, m.blobentryid, m.tag, m.data
          FROM keyentry k JOIN blobentry b ON b.keyentryid = k.id JOIN blobmetadata m ON m.blobentryid = b.id
          WHERE k.state = 1 AND b.id = (SELECT max(c.id) FROM blobentry c
             WHERE c.keyentryid = k.id AND c.subcomponent_type = b.subcomponent_type)
          ORDER BY k.id, m.blobentryid, m.tag, m.data"),
        ("keyparameter",
         "SELECT k.id, p.tag, p.data, p.security_level
          FROM keyentry k JOIN keyparameter p ON p.keyentryid = k.id WHERE k.state = 1
          ORDER BY k.id, p.tag, p.security_level, p.data"),
        ("keymetadata",
         "SELECT k.id, m.tag, m.data
          FROM keyentry k JOIN keymetadata m ON m.keyentryid = k.id WHERE k.state = 1
          ORDER BY k.id, m.tag, m.data"),
    ] {
        hash_rows(&modern, &mut protected, section, sql, |row|
            Ok(!removed_keys.contains(&row.get(0)?)))?;
    }

    hash_rows(
        &modern,
        &mut protected,
        "grant",
        "SELECT g.keyentryid, g.id, g.grantee, g.access_vector, k.state
         FROM \"grant\" g LEFT JOIN keyentry k ON k.id = g.keyentryid ORDER BY g.id",
        |row| {
            ensure!(
                row.get::<_, Option<i32>>(4)? == Some(1),
                "Keystore grant has no live key"
            );
            let uid = row.get(2).context("invalid grantee in Android Keystore")?;
            let clears_grantee = owners.should_clear(uid)?;
            if clears_grantee {
                targets.insert(uid);
            }
            Ok(!clears_grantee && !removed_keys.contains(&row.get(0)?))
        },
    )?;

    // This database is optional, and the native listener may create it
    // empty when it first processes a namespace.
    if let Some(legacy) = snapshot(root, store, scratch, LEGACY_DATABASE, None)? {
        hash_rows(
            &legacy,
            &mut protected,
            "vpnprofiles",
            "SELECT owner, alias, profile FROM profiles ORDER BY owner, alias, profile",
            |row| {
                let uid = row
                    .get(0)
                    .context("invalid owner in Android legacy credential store")?;
                let remove = owners.should_clear(uid)?;
                if remove {
                    targets.insert(uid);
                }
                Ok(!remove)
            },
        )?;
    }

    inspect_legacy_files(root, store, owners, &mut targets, &mut protected)?;
    files.check(&open_store(root)?)?;
    Ok(Snapshot {
        targets,
        protected: protected.finalize().into(),
    })
}

fn hash_rows(
    db: &Connection,
    hash: &mut Sha256,
    section: &str,
    sql: &str,
    mut protect: impl FnMut(&Row<'_>) -> Result<bool>,
) -> Result<()> {
    let mut query = db
        .prepare(sql)
        .with_context(|| format!("inspect Android Keystore table {section}"))?;
    let columns = query.column_count();
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        if !protect(row)? {
            continue;
        }
        frame(hash, section.as_bytes());
        for column in 0..columns {
            match row.get_ref(column)? {
                ValueRef::Null => hash.update([0]),
                ValueRef::Integer(value) => {
                    hash.update([1]);
                    hash.update(value.to_le_bytes());
                }
                ValueRef::Real(value) => {
                    hash.update([2]);
                    hash.update(value.to_bits().to_le_bytes());
                }
                ValueRef::Text(bytes) => {
                    hash.update([3]);
                    frame(hash, bytes);
                }
                ValueRef::Blob(bytes) => {
                    hash.update([4]);
                    frame(hash, bytes);
                }
            }
        }
    }
    Ok(())
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn inspect_legacy_files(
    root: &DataRoot,
    store: &File,
    owners: &Registry,
    targets: &mut BTreeSet<u32>,
    protected: &mut Sha256,
) -> Result<()> {
    for name in sorted_names(store)? {
        let Some(user) = name.to_str().and_then(|name| name.strip_prefix("user_")) else {
            continue;
        };
        let user = user_id(user).context("unrecognized legacy Keystore directory")?;
        let folder = root
            .open_child(store, &name)?
            .context("invalid legacy Keystore directory")?;
        for file in sorted_names(&folder)? {
            // Legacy filenames start with a full UID, followed by a private alias.
            let mut parts = file.as_encoded_bytes().splitn(2, |&byte| byte == b'_');
            let uid = match (parts.next(), parts.next()) {
                (Some(prefix), Some(_)) => std::str::from_utf8(prefix).ok().and_then(user_id),
                _ => None,
            };
            if let Some(uid) = uid {
                ensure!(
                    uid / UID_PER_USER == user,
                    "legacy Keystore entry is in the wrong user's directory"
                );
                if owners.should_clear(uid)? {
                    targets.insert(uid);
                    continue;
                }
            }
            let opened = crate::storage::open_regular(&folder, &file)
                .context("open protected legacy Keystore file")?;
            ensure!(
                mount_id(&opened)? == root.mount,
                "legacy Keystore file is on another filesystem"
            );
            let length = opened.metadata()?.len();
            ensure!(
                length <= MAX_FILE_BYTES,
                "legacy Keystore file is too large"
            );
            frame(protected, b"legacy file");
            frame(protected, name.as_encoded_bytes());
            frame(protected, file.as_encoded_bytes());
            protected.update(length.to_le_bytes());
            let mut file_hash = Sha256::new();
            let copied = io::copy(&mut opened.take(length + 1), &mut file_hash)?;
            ensure!(
                copied == length,
                "legacy Keystore file changed during inspection"
            );
            protected.update(file_hash.finalize());
        }
    }
    Ok(())
}

fn sorted_names(directory: &File) -> Result<Vec<OsString>> {
    let mut names = entries(directory)?
        .map(|entry| Ok(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort_unstable();
    Ok(names)
}

// SQLite may update its shared-memory index even on a read-only connection,
// and immutable mode ignores the WAL. Replay journals against a private
// scratch copy instead. The source Android services are stopped in Recovery.
fn snapshot(
    root: &DataRoot,
    store: &File,
    scratch: &Path,
    name: &str,
    primary: Option<&File>,
) -> Result<Option<Connection>> {
    let mut files = Vec::new();
    for suffix in ["", "-wal", "-journal"] {
        let intended = format!("{name}{suffix}");
        match crate::storage::open_regular(store, OsStr::new(&intended)) {
            Ok(file) => {
                ensure!(
                    mount_id(&file)? == root.mount,
                    "Keystore file is on another filesystem"
                );
                if let Some(primary) = primary.filter(|_| suffix.is_empty()) {
                    KeystoreFiles::require_same(&file, primary)?;
                }
                let meta = file.metadata()?;
                ensure!(
                    meta.len() <= MAX_FILE_BYTES,
                    "Android Keystore file is too large to inspect"
                );
                files.push((suffix, intended, file, meta));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("open {intended} in Android Keystore"));
            }
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    ensure!(
        files[0].0.is_empty(),
        "Android Keystore journal has no database"
    );
    ensure!(files[0].3.len() > 0, "Android Keystore database is empty");
    for (_, filename, file, meta) in &files {
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(scratch.join(filename))
            .context("create private Keystore inspection file")?;
        let copied = io::copy(&mut file.take(meta.len() + 1), &mut copy)
            .context("copy Android Keystore bookkeeping")?;
        ensure!(copied == meta.len(), "Android Keystore changed during copy");
    }
    // Compare the complete set after every copy. An offline database must
    // not acquire a new journal or have any of its original files replaced.
    for suffix in ["", "-wal", "-journal"] {
        let filename = format!("{name}{suffix}");
        let before = files.iter().find(|(kind, _, _, _)| *kind == suffix);
        match (
            before,
            crate::storage::open_regular(store, OsStr::new(&filename)),
        ) {
            (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
            (Some((_, _, _, meta)), Ok(file)) => {
                let after = file.metadata()?;
                ensure!(
                    mount_id(&file)? == root.mount
                        && (after.dev(), after.ino(), after.len())
                            == (meta.dev(), meta.ino(), meta.len())
                        && after.modified()? == meta.modified()?
                        && (after.ctime(), after.ctime_nsec()) == (meta.ctime(), meta.ctime_nsec()),
                    "Android Keystore changed during inspection",
                );
            }
            _ => bail!("Android Keystore journals changed during inspection"),
        }
    }

    let path = scratch.join(name);
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db = Connection::open_with_flags(path, flags)
        .with_context(|| format!("open the isolated copy of {name}"))?;
    db.pragma_update(None, "trusted_schema", false)?;
    db.pragma_update(None, "temp_store", "MEMORY")?;
    db.pragma_update(None, "query_only", true)?;
    Ok(Some(db))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SCHEMA: &str = "
        CREATE TABLE keyentry (id INTEGER UNIQUE, key_type INTEGER, domain INTEGER, namespace INTEGER, alias BLOB, state INTEGER, km_uuid BLOB);
        CREATE TABLE blobentry (id INTEGER PRIMARY KEY, keyentryid INTEGER, subcomponent_type INTEGER, blob BLOB);
        CREATE TABLE keyparameter (keyentryid INTEGER, tag INTEGER, data ANY, security_level INTEGER);
        CREATE TABLE keymetadata (keyentryid INTEGER, tag INTEGER, data ANY);
        CREATE TABLE blobmetadata (id INTEGER PRIMARY KEY, blobentryid INTEGER, tag INTEGER, data ANY);
        CREATE TABLE grant (id INTEGER UNIQUE, grantee INTEGER, keyentryid INTEGER, access_vector INTEGER);";

    fn registry(data: &Path) -> Registry {
        let system = data.join("system");
        fs::create_dir_all(system.join("users")).unwrap();
        fs::write(
            system.join("users/userlist.xml"),
            br#"<users><user id="0"/><user id="10"/></users>"#,
        )
        .unwrap();
        fs::write(
            system.join("users/0.xml"),
            br#"<user id="0" serialNumber="0"/>"#,
        )
        .unwrap();
        fs::write(
            system.join("users/10.xml"),
            br#"<user id="10" serialNumber="47"/>"#,
        )
        .unwrap();
        fs::write(
            system.join("packages.xml"),
            br#"<packages>
                <package name="fixture.app" codePath="/data/app/fixture" publicFlags="4" privateFlags="0" userId="10001"/>
                <package name="fixture.preload" codePath="/product/app/preload" publicFlags="1" privateFlags="0" userId="10002"/>
                <package name="fixture.other" codePath="/data/app/other" publicFlags="4" privateFlags="0" userId="10003"/>
                <package name="fixture.grantee" codePath="/data/app/grantee" publicFlags="4" privateFlags="0" userId="10004"/>
                <package name="fixture.legacy" codePath="/data/app/legacy" publicFlags="4" privateFlags="0" userId="10006"/>
                <package name="fixture.vpn" codePath="/data/app/vpn" publicFlags="4" privateFlags="0" userId="10007"/>
            </packages>"#,
        ).unwrap();
        Registry::read(&DataRoot::open(data).unwrap()).unwrap()
    }

    #[test]
    fn cleanup_rejects_residual_app_keys_and_a_changed_screen_lock_blob() {
        let data = tempfile::tempdir().unwrap();
        let store = data.path().join("misc/keystore");
        fs::create_dir_all(&store).unwrap();
        let db = Connection::open(store.join(DATABASE)).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "INSERT INTO keyentry VALUES (1, 0, 0, 10001, 'app', 1, X'00'), (2, 0, 2, 103, 'screen-lock', 1, X'00'),
                (3, 0, 0, 10002, 'preload', 1, X'00'), (4, 1, 0, 0, 'super-key', 1, X'00');
             INSERT INTO blobentry VALUES (101, 1, 0, X'4101'), (102, 2, 0, X'4201'), (103, 2, 0, X'4202');
             INSERT INTO blobmetadata VALUES (1, 103, 11, 'protected metadata');
             INSERT INTO grant VALUES (1, 1000, 1, 1), (2, 10001, 2, 1), (3, 1000, 3, 1);",
        ).unwrap();
        let source = DataRoot::open(data.path()).unwrap();
        let owners = registry(data.path());
        let read = || {
            let scratch = tempfile::tempdir().unwrap();
            inspect(
                &source,
                scratch.path(),
                &owners,
                &KeystoreFiles::open(&source).unwrap(),
            )
            .unwrap()
        };
        let before = read();
        assert_eq!(before.targets, BTreeSet::from([10001]));
        assert!(before.verify_cleared(&read()).is_err());

        db.execute_batch(
            "DELETE FROM \"grant\" WHERE id IN (1, 2);
                          DELETE FROM keyentry WHERE id = 1;
                          DELETE FROM blobentry WHERE id = 102;",
        )
        .unwrap();
        assert!(before.verify_cleared(&read()).is_ok());

        db.execute("UPDATE \"grant\" SET access_vector = 2 WHERE id = 3", [])
            .unwrap();
        assert!(before.verify_cleared(&read()).is_err());
        db.execute("UPDATE \"grant\" SET access_vector = 1 WHERE id = 3", [])
            .unwrap();

        db.execute("UPDATE blobmetadata SET data = 'changed' WHERE id = 1", [])
            .unwrap();
        assert!(before.verify_cleared(&read()).is_err());
        db.execute(
            "UPDATE blobmetadata SET data = 'protected metadata' WHERE id = 1",
            [],
        )
        .unwrap();

        db.execute("UPDATE blobentry SET blob = X'4203' WHERE id = 103", [])
            .unwrap();
        assert!(before.verify_cleared(&read()).is_err());
    }

    #[test]
    fn data_files_are_removed_only_after_a_verified_key_stage() {
        use super::super::{Group, KeyCleanup};

        for case in [
            "wrong-plan",
            "changed-registry",
            "replaced-keystore",
            "partial-error",
            "residual-grant",
            "changed-protected-key",
            "changed-credential-file",
            "success",
        ] {
            let data = tempfile::tempdir().unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let store = data.path().join("misc/keystore");
            fs::create_dir_all(&store).unwrap();
            registry(data.path());
            let retained: [(&str, &[u8]); 5] = [
                (
                    "system/users/userlist.xml",
                    br#"<users><user id="0"/></users>"#,
                ),
                ("system/users/0.xml", br#"<user id="0" serialNumber="0"/>"#),
                ("unencrypted/key", b"encryption sample"),
                ("misc/vold/user_keys/ce/0/key", b"FBE sample"),
                ("media/0/DCIM/kept.jpg", b"shared sample"),
            ];
            for (name, bytes) in retained {
                let file = data.path().join(name);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, bytes).unwrap();
            }
            let private = data.path().join("data/fixture.app/private.txt");
            fs::create_dir_all(private.parent().unwrap()).unwrap();
            fs::write(&private, b"app sample").unwrap();
            let db = Connection::open(store.join(DATABASE)).unwrap();
            db.execute_batch(SCHEMA).unwrap();
            db.execute_batch(
                "INSERT INTO keyentry VALUES (1, 0, 0, 10001, 'app', 1, X'00'), (2, 0, 2, 103, 'screen-lock', 1, X'00');
                 INSERT INTO blobentry VALUES (2, 2, 0, X'01');
                 INSERT INTO grant VALUES (1, 10001, 2, 1);",
            ).unwrap();
            let root = DataRoot::open(data.path()).unwrap();
            let reopened = DataRoot::open(data.path()).unwrap();
            let planned_root = if case == "wrong-plan" {
                &reopened
            } else {
                &root
            };
            let mut plan = planned_root.prepare(Group::DataKeepMedia).unwrap();
            let keys = KeyCleanup::prepare(&root, scratch.path()).unwrap();
            if case == "changed-registry" {
                let file = data.path().join("system/packages.xml");
                let xml = fs::read_to_string(&file).unwrap();
                fs::write(file, xml.replace("fixture.app", "fixture.replacement")).unwrap();
            }
            if case == "replaced-keystore" {
                let saved = store.join("original.sqlite");
                fs::rename(store.join(DATABASE), &saved).unwrap();
                fs::copy(saved, store.join(DATABASE)).unwrap();
            }
            let result = keys.apply(&root, &mut plan, scratch.path(), |targets, _| {
                assert!(
                    !matches!(
                        case,
                        "wrong-plan" | "changed-registry" | "replaced-keystore"
                    ),
                    "maintenance ran with unverified input"
                );
                assert_eq!(targets, &BTreeSet::from([10001]));
                db.execute("DELETE FROM keyentry WHERE id = 1", [])?;
                if case == "partial-error" {
                    bail!("simulated service failure after removing one app key");
                }
                if case != "residual-grant" {
                    db.execute("DELETE FROM \"grant\" WHERE id = 1", [])?;
                }
                if case == "changed-protected-key" {
                    db.execute("UPDATE blobentry SET blob = X'02' WHERE id = 2", [])?;
                }
                if case == "changed-credential-file" {
                    fs::write(data.path().join("unencrypted/key"), b"changed sample")?;
                }
                Ok(())
            });
            assert_eq!(result.is_ok(), case == "success", "{case}");
            assert!(private.exists(), "{case}");
            assert_eq!(
                root.execute(plan, &mut |_, _| {}).is_ok(),
                case == "success",
                "{case}"
            );
            assert_eq!(private.exists(), case != "success", "{case}");
            for (name, bytes) in retained {
                if case == "changed-credential-file" && name == "unencrypted/key" {
                    assert_eq!(fs::read(data.path().join(name)).unwrap(), b"changed sample");
                    continue;
                }
                assert_eq!(
                    fs::read(data.path().join(name)).unwrap(),
                    bytes,
                    "{case}: {name}"
                );
            }
        }
    }

    #[test]
    fn app_key_inventory_reads_uncheckpointed_keys_grants_and_legacy_records() {
        let data = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let store = data.path().join("misc/keystore");
        fs::create_dir_all(store.join("user_10")).unwrap();

        let path = store.join(DATABASE);
        let db = Connection::open(&path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             INSERT INTO keyentry VALUES
                (1, 0, 0, 10001, 'app', 1, X'00'),
                (2, 0, 0, 1010003, 'second user', 1, X'00'),
                (3, 0, 0, 1000, 'system', 1, X'00'),
                (4, 0, 2, 10002, 'selinux', 1, X'00'),
                (5, 1, 0, 10005, 'super key', 1, X'00');
             INSERT INTO grant VALUES (1, 10004, 3, 1), (2, 1000, 1, 1);",
        )
        .unwrap();
        let original_main = fs::read(&path).unwrap();
        let journal = store.join("persistent.sqlite-wal");
        let original_wal = fs::read(&journal).unwrap();
        assert!(!original_wal.is_empty());

        let legacy = Connection::open(store.join(LEGACY_DATABASE)).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE profiles (owner INTEGER, alias BLOB, profile BLOB);
             INSERT INTO profiles VALUES (1000, 'system-sample', X'0102');
             INSERT INTO profiles VALUES (10007, 'app-sample', X'0304');",
            )
            .unwrap();
        fs::write(
            store.join("user_10/1010006_USRCERT_fixture"),
            b"certificate sample",
        )
        .unwrap();
        fs::write(store.join("user_10/.masterkey"), b"protected sample").unwrap();

        let root = DataRoot::open(data.path()).unwrap();
        let candidates = inspect(
            &root,
            scratch.path(),
            &registry(data.path()),
            &KeystoreFiles::open(&root).unwrap(),
        )
        .unwrap()
        .targets;
        assert_eq!(
            candidates,
            BTreeSet::from([10001, 10004, 10007, 1010003, 1010006])
        );
        assert_eq!(fs::read(path).unwrap(), original_main);
        assert_eq!(fs::read(journal).unwrap(), original_wal);
    }
}
