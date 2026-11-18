//! Independent, read-only verification of an existing backup.

use crate::store::Reader;
use crate::{Manifest, FORMAT, MANIFEST};
use anyhow::{ensure, Context, Result};
use rustix::fs::{self as at, Mode, OFlags};
use skyward_files::Cancelled;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const CHUNK: usize = 1 << 20;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// A single read pass over the backup's contents.
#[derive(Debug)]
pub enum Event {
    Plan(Vec<(String, u64)>),
    At { item: usize, done: u64, of: u64 },
}

fn stopped(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}

fn open_file(folder: &File, name: &str) -> io::Result<File> {
    let file = File::from(at::openat(folder, name, FILE, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular backup file"));
    }
    Ok(file)
}

/// Read only this backup's manifest and numbered volumes. This checks their
/// recorded lengths and decompressed SHA-256; it does not decide whether a
/// restore is compatible with the phone or with its current encryption key.
pub fn verify(folder: &Path, cancel: &AtomicBool, tell: &mut dyn FnMut(Event)) -> Result<()> {
    stopped(cancel)?;
    let folder = Arc::new(File::from(at::open(folder, DIRECTORY, Mode::empty()).context("open the backup folder")?));
    let manifest = open_file(&folder, MANIFEST).context("open backup.toml")?;
    ensure!(manifest.metadata()?.len() <= MAX_MANIFEST_BYTES, "backup.toml is too large");
    let mut text = String::new();
    manifest.take(MAX_MANIFEST_BYTES + 1).read_to_string(&mut text).context("read backup.toml")?;
    ensure!(text.len() as u64 <= MAX_MANIFEST_BYTES, "backup.toml is too large");
    let manifest: Manifest = toml::from_str(&text).context("read backup.toml")?;
    ensure!(manifest.format == FORMAT, "unsupported backup layout {}", manifest.format);
    ensure!(!manifest.items.is_empty(), "the backup contains no parts");
    let mut names = BTreeSet::new();
    let mut all_files = BTreeSet::new();
    let mut volumes = Vec::new();
    let mut total = 0u64;
    for item in &manifest.items {
        stopped(cancel)?;
        item.verify_files().with_context(|| format!("check {}", item.name))?;
        ensure!(names.insert(&item.name), "duplicate backup part {}", item.name);
        ensure!(item.stored.sha256.len() == 64 && item.stored.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid SHA-256 for {}", item.name);
        total = total.checked_add(item.stored.size).context("backup content length overflows")?;
        let mut bytes = 0u64;
        let mut files = Vec::new();
        for name in &item.stored.files {
            stopped(cancel)?;
            ensure!(all_files.insert(name), "duplicate backup volume {name}");
            let file = open_file(&folder, name).with_context(|| format!("open {name}"))?;
            let len = file.metadata()?.len();
            bytes = bytes.checked_add(len).context("backup volume length overflows")?;
            files.push((name.clone(), len));
        }
        ensure!(bytes == item.stored.stored,
            "stored byte count differs for {} ({}): {bytes} found, {} recorded",
            item.name, item.stored.files[0], item.stored.stored);
        volumes.push(files);
    }
    stopped(cancel)?;
    tell(Event::Plan(manifest.items.iter().map(|item| (item.name.clone(), item.stored.size)).collect()));
    let mut buf = vec![0; CHUNK];
    for (k, (item, files)) in manifest.items.iter().zip(volumes).enumerate() {
        let checked = (|| {
            stopped(cancel)?;
            let source_bytes = Arc::new(AtomicU64::new(0));
            let source = Volumes { folder: folder.clone(), files, next: 0, open: None, source_bytes: source_bytes.clone() };
            let mut reader = Reader::from_source(source, item.stored.compressed);
            let mut done = 0;
            tell(Event::At { item: k, done, of: item.stored.size });
            while done < item.stored.size {
                stopped(cancel)?;
                let want = (item.stored.size - done).min(buf.len() as u64) as usize;
                let read = reader.read(&mut buf[..want]).context("read backup contents")?;
                if read == 0 { break; }
                done += read as u64;
                tell(Event::At { item: k, done, of: item.stored.size });
            }
            stopped(cancel)?;
            ensure!(reader.read(&mut buf[..1])? == 0, "contents exceed their recorded length");
            reader.check(&item.stored)?;
            // The LZ4 decoder stops at its first end marker. A valid content
            // digest must not hide unread bytes in this or a later volume.
            ensure!(source_bytes.load(Ordering::Relaxed) == item.stored.stored,
                "not all recorded volume bytes were read: {} read, {} recorded",
                source_bytes.load(Ordering::Relaxed), item.stored.stored);
            stopped(cancel)
        })();
        checked.with_context(|| format!("check {}", item.name))?;
    }
    stopped(cancel)
}

/// Each volume is opened relative to the pinned directory. Its type and
/// length are checked again when read, without following a replacement link.
struct Volumes {
    folder: Arc<File>,
    files: Vec<(String, u64)>,
    next: usize,
    open: Option<BufReader<File>>,
    /// Bytes returned to the decoder, excluding BufReader's prefetch.
    source_bytes: Arc<AtomicU64>,
}

impl Read for Volumes {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(file) = &mut self.open {
                let n = file.read(buf)?;
                self.source_bytes.fetch_add(n as u64, Ordering::Relaxed);
                if n > 0 || buf.is_empty() { return Ok(n); }
            }
            let Some((name, expected)) = self.files.get(self.next) else { return Ok(0) };
            let file = open_file(&self.folder, name)
                .map_err(|error| io::Error::new(error.kind(), format!("open {name}: {error}")))?;
            if file.metadata()?.len() != *expected {
                return Err(io::Error::other(format!("{name} changed size during verification")));
            }
            self.open = Some(BufReader::with_capacity(CHUNK, file));
            self.next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Writer;
    use crate::{Item, Manifest, Part, FORMAT};
    use skyward_files::{Cancelled, Ownership};
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn backup(root: &Path, compressed: bool) -> Manifest {
        let name = Item::file_name(Part::Data, "data", compressed);
        let mut writer = Writer::create(root, &name, compressed, Ownership::Kept);
        let bytes: Vec<u8> = (0..200_000).map(|i| ((i * 73 + i / 127) % 251) as u8).collect();
        writer.write_all(&bytes).unwrap();
        let stored = writer.finish().unwrap();
        let manifest = Manifest {
            format: FORMAT, device: "lmi".into(), made: 1_793_000_000,
            slot: "".into(), system: None, data_key: None,
            items: vec![Item { part: Part::Data, name: "data".into(), stored }],
        };
        manifest.write(root, Ownership::Kept).unwrap();
        manifest
    }

    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(root).unwrap().map(|entry| {
            let path = entry.unwrap().path();
            (path.file_name().unwrap().to_str().unwrap().to_string(), fs::read(path).unwrap())
        }).collect();
        files.sort();
        files
    }

    #[test]
    fn plain_and_compressed_backups_are_checked_without_changing_their_files() {
        for compressed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let manifest = backup(root.path(), compressed);
            let before = snapshot(root.path());
            let mut events = Vec::new();
            verify(root.path(), &AtomicBool::new(false), &mut |event| events.push(event)).unwrap();
            assert_eq!(snapshot(root.path()), before);
            assert!(matches!(events.first(), Some(Event::Plan(plan)) if plan == &vec![("data".into(), manifest.items[0].stored.size)]));
            assert!(matches!(events.last(), Some(Event::At { item: 0, done, of }) if done == of && *of == 200_000));
        }
    }

    #[test]
    fn numbered_volumes_are_read_in_order_for_plain_and_compressed_backups() {
        for compressed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut manifest = backup(root.path(), compressed);
            let item = &mut manifest.items[0];
            let first = root.path().join(&item.stored.files[0]);
            let bytes = fs::read(&first).unwrap();
            fs::remove_file(first).unwrap();
            item.stored.files.clear();
            for (k, chunk) in bytes.chunks(173).enumerate() {
                let name = format!("{}.{k:03}", Item::file_name(Part::Data, "data", compressed));
                fs::write(root.path().join(&name), chunk).unwrap();
                item.stored.files.push(name);
            }
            fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
            verify(root.path(), &AtomicBool::new(false), &mut |_| {}).unwrap();
        }
    }

    #[test]
    fn changed_contents_report_the_part_and_leave_it_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let manifest = backup(root.path(), false);
        let file = root.path().join(&manifest.items[0].stored.files[0]);
        let mut bytes = fs::read(&file).unwrap();
        bytes[37] ^= 1;
        fs::write(&file, &bytes).unwrap();
        let before = snapshot(root.path());
        let error = verify(root.path(), &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        let why = format!("{error:#}");
        assert!(why.contains("data") && why.contains("digest"), "{why}");
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn missing_and_truncated_volumes_are_not_accepted() {
        let root = tempfile::tempdir().unwrap();
        let manifest = backup(root.path(), false);
        let name = &manifest.items[0].stored.files[0];
        fs::write(root.path().join(name), b"truncated").unwrap();
        let why = format!("{:#}", verify(root.path(), &AtomicBool::new(false), &mut |_| {}).unwrap_err());
        assert!(why.contains(name), "{why}");
        fs::remove_file(root.path().join(name)).unwrap();
        let why = format!("{:#}", verify(root.path(), &AtomicBool::new(false), &mut |_| {}).unwrap_err());
        assert!(why.contains(name), "{why}");
    }

    #[test]
    fn symbolic_backup_folders_manifests_and_volumes_are_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let manifest = backup(root.path(), false);
        let link = parent.path().join("backup");
        symlink(root.path(), &link).unwrap();
        assert!(verify(&link, &AtomicBool::new(false), &mut |_| {}).is_err());
        let path = root.path().join(crate::MANIFEST);
        let text = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let target = parent.path().join(crate::MANIFEST);
        fs::write(&target, &text).unwrap();
        symlink(&target, &path).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, text).unwrap();
        let file = root.path().join(&manifest.items[0].stored.files[0]);
        let outside = parent.path().join("outside");
        fs::rename(&file, &outside).unwrap();
        symlink(&outside, &file).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn manifest_cannot_name_files_outside_its_backup() {
        let root = tempfile::tempdir().unwrap();
        let mut manifest = backup(root.path(), false);
        manifest.items[0].stored.files[0] = "../outside".into();
        fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn an_empty_or_duplicated_manifest_is_not_a_verified_backup() {
        let root = tempfile::tempdir().unwrap();
        let mut manifest = backup(root.path(), false);
        manifest.items.push(manifest.items[0].clone());
        fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
        manifest.items.clear();
        fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn oversized_or_invalid_manifests_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(crate::MANIFEST), vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
        fs::write(root.path().join(crate::MANIFEST), b"not a backup").unwrap();
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn cancellation_never_changes_or_removes_an_existing_backup() {
        let root = tempfile::tempdir().unwrap();
        backup(root.path(), true);
        let before = snapshot(root.path());
        let cancel = AtomicBool::new(true);
        let error = verify(root.path(), &cancel, &mut |_| {}).unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        cancel.store(false, Ordering::Relaxed);
        let error = verify(root.path(), &cancel, &mut |event| {
            if matches!(event, Event::At { done, .. } if done > 0) {
                cancel.store(true, Ordering::Relaxed);
            }
        }).unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn compressed_verification_rejects_trailing_bytes_in_the_same_or_another_volume() {
        for another in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut manifest = backup(root.path(), true);
            let item = &mut manifest.items[0];
            let extra = b"trailing bytes that the first LZ4 end marker must not hide";
            if another {
                let name = format!("{}.001", Item::file_name(Part::Data, "data", true));
                fs::write(root.path().join(&name), extra).unwrap();
                item.stored.files.push(name);
            } else {
                let path = root.path().join(&item.stored.files[0]);
                let mut bytes = fs::read(&path).unwrap();
                bytes.extend(extra);
                fs::write(&path, bytes).unwrap();
            }
            item.stored.stored += extra.len() as u64;
            fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
            assert!(verify(root.path(), &AtomicBool::new(false), &mut |_| {}).is_err(),
                    "bytes following the compressed end marker were left unread");
        }
    }

    #[test]
    fn decompression_stops_if_contents_exceed_the_declared_size() {
        let root = tempfile::tempdir().unwrap();
        let mut manifest = backup(root.path(), true);
        manifest.items[0].stored.size = 7;
        fs::write(root.path().join(crate::MANIFEST), toml::to_string(&manifest).unwrap()).unwrap();
        let mut last = 0;
        assert!(verify(root.path(), &AtomicBool::new(false), &mut |event| {
            if let Event::At { done, .. } = event { last = done; }
        }).is_err());
        assert!(last <= 7);
    }
}
