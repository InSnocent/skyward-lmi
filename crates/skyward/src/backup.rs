//! Backing up and restoring on a thread of its own, telling the screen how
//! far it has come.

use crate::usb_mount::UsbMount;
use anyhow::{ensure, Context, Result};
use skyward_backup::archive::{self, Skipped};
use skyward_backup::store::{self, Reader, Stored, Writer};
use skyward_backup::{Item, Manifest, Part, FORMAT};
use skyward_block::{mount, BY_NAME};
use skyward_files::{Cancelled, Ownership};
use skyward_vab::{Slot, SnapshotState};
use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most often how far the work has come is told.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// How much is read or written at a time between two checks of whether
/// the work is to stop.
const CHUNK: usize = 4 << 20;

/// The data partition, and the reference of the key its own folders are
/// encrypted with: one made afresh has another.
const DATA: &str = "/data";
const DATA_KEY: &str = "/data/unencrypted/ref";

/// What the data partition's archive takes for each entry beyond what a
/// file holds: its headers, and the end of its last block.
const ENTRY_BYTES: u64 = 2048;

/// The slot's partitions that start the phone and check what it starts, in
/// the order a backup takes them: those the phone has.
const BOOT: [&str; 8] = ["boot", "init_boot", "vendor_boot", "dtbo", "vbmeta", "vbmeta_system", "vbmeta_vendor", "recovery"];

/// The order a restore writes the parts in: what starts the phone and what
/// it starts, then the baseband's, then the data partition's files, which
/// take longest.
const RESTORE_ORDER: [Part; 5] = [Part::Boot, Part::System, Part::Efs, Part::Data, Part::SharedFiles];
const SHARED_NAME: &str = "shared-files";

/// What a backup of the data partition leaves out on every phone, and a
/// restore leaves as it is: internal storage, which is the user's own; the
/// keys the partition opens with, and what the lock screen and the key
/// service bind to the secure world, which an older copy would not open;
/// what the system makes again, its caches, compiled code and the updates
/// it downloaded; and the second system, which lies where /metadata keeps
/// it.
const DATA_SKIPPED: [&str; 22] = [
    "media",
    "lost+found",
    "per_boot",
    "unencrypted",
    "misc/vold",
    "misc/keystore",
    "misc/gatekeeper",
    "misc/keystore2",
    "system/locksettings.db",
    "system/locksettings.db-wal",
    "system/locksettings.db-shm",
    "system/locksettings.db-journal",
    "system_de/*/spblob",
    "vendor_de/*/fpdata",
    "dalvik-cache",
    "resource-cache",
    "system/package_cache",
    "misc/apexdata/com.android.art/dalvik-cache",
    "cache",
    "ota",
    "ota_package",
    "skyward/systems",
];

/// What of the data partition backups leave out on this phone: what every
/// phone's do, and what the phone's own secure world keeps in step.
pub fn data_skipped(phone: &[String]) -> Vec<String> {
    DATA_SKIPPED.iter().map(|s| s.to_string()).chain(phone.iter().cloned()).collect()
}

#[derive(Clone, Copy)]
pub enum DataScope {
    Partition,
    LmiFixture,
}

impl DataScope {
    fn root(self) -> &'static Path {
        Path::new(match self {
            Self::Partition => DATA,
            Self::LmiFixture => "/data/data/org.skyward.lmi.backupfixture",
        })
    }

    fn validate_parts(self, parts: [bool; 5]) -> Result<()> {
        if matches!(self, Self::LmiFixture) {
            ensure!(parts == [false, false, true, false, false], "the fixture can only use its data directory");
        }
        Ok(())
    }

    fn item_name(self) -> &'static str {
        match self {
            Self::Partition => "data",
            Self::LmiFixture => "lmi-app-fixture",
        }
    }
}

pub struct BackupTask {
    pub data_scope: DataScope,
    /// Which of [`Part::ALL`] to take.
    pub parts: [bool; 5],
    /// The folder of backups at the place chosen; this one goes into a
    /// folder of its own there, called `stamp`.
    pub into: PathBuf,
    pub stamp: String,
    pub ownership: Ownership,
    pub compress: bool,
    pub verify: bool,
    /// The mount the place is, when it is shown read-only and so is made
    /// writable for the backup, and read-only again after: a USB drive.
    pub remount: Option<PathBuf>,
    /// The pinned lmi USB mount, with verified publication on FAT/exFAT.
    pub usb: Option<UsbMount>,
    /// When the backup is made, in Unix seconds, the phone by its
    /// codename, and the system it runs, by its version.
    pub made: i64,
    pub device: String,
    pub system: Option<String>,
    /// The phone's partitions of the baseband's settings and the sensors'
    /// calibration, and what of the data partition it leaves out.
    pub efs: Vec<String>,
    pub skipped: Vec<String>,
}

pub struct RestoreTask {
    pub data_scope: DataScope,
    pub folder: PathBuf,
    /// An lmi USB backup is staged on the phone before the restore writes.
    pub usb: Option<UsbMount>,
    /// Which of [`Part::ALL`] to restore.
    pub parts: [bool; 5],
    pub device: String,
    pub skipped: Vec<String>,
    /// What has to happen before the slot's partitions in super are written
    /// over, as before an update.
    pub detach: skyward_install::Detach,
}

pub enum Task {
    Backup(BackupTask),
    Restore(RestoreTask),
    /// Inspect an existing backup without opening any restore target.
    Verify { folder: PathBuf, usb: Option<UsbMount> },
}

/// Why backing up or restoring does not start.
#[derive(Debug)]
pub enum Refusal {
    /// Too little room where the backup goes: the bytes it takes, and the
    /// bytes free.
    Room { needed: u64, free: u64 },
    /// An update is in flight, and the slot's partitions in super are its
    /// to change.
    Update(SnapshotState),
    /// The backup was made on this other phone.
    OtherPhone(String),
    /// This partition in super is not the size it was backed up at.
    Resized(String),
    /// The data partition has been made afresh since the backup.
    DataReset,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Room { needed, free } => write!(f, "the backup takes {needed} bytes, and {free} are free"),
            Refusal::Update(state) => write!(f, "an update is in flight ({state:?})"),
            Refusal::OtherPhone(device) => write!(f, "the backup was made on {device}"),
            Refusal::Resized(name) => write!(f, "{name} is not the size it was backed up at"),
            Refusal::DataReset => write!(f, "the data partition has been made afresh since the backup"),
        }
    }
}

impl std::error::Error for Refusal {}

pub enum Event {
    /// Bytes copied from USB to local scratch before a restore can start.
    Staging { done: u64, of: u64 },
    /// The parts the work goes through, each by name with its bytes; told
    /// once they are known, as measuring the data partition's files takes
    /// a while.
    Plan(Vec<(String, u64)>),
    /// The part under way, the bytes of it done of how many in this pass
    /// over it, and whether this is the pass that checks a backup against
    /// what it recorded.
    At { item: usize, done: u64, of: u64, checking: bool },
    Ended(End),
}

pub enum End {
    /// A backup made, in this folder.
    Saved(PathBuf),
    /// All files matched their recorded lengths and content digests.
    Verified,
    /// A backup restored, but for the folders a key of their own began in
    /// that are gone, which only the system makes, with what the backup
    /// held in them.
    Restored { unmade: Vec<PathBuf>, shared: Option<PathBuf> },
    /// Stopped as asked, with nothing of the backup left, or nothing of
    /// the restore written.
    Stopped,
    Refused(Refusal),
    Failed(String),
}

pub struct Worker {
    cancel: Arc<AtomicBool>,
    /// The screen lets it go once the work has ended.
    worker: skyward_sys::worker::Worker<Event>,
}

impl Worker {
    pub fn start(task: Task) -> Result<Worker> {
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let worker = skyward_sys::worker::Worker::start("backup", move |teller| {
            let tell = |event: Event| {
                teller.tell(event);
            };
            let ended = match task {
                Task::Backup(t) => backup(t, &tell, &stop).map(End::Saved),
                Task::Restore(t) => restore(t, &tell, &stop).map(|(unmade, shared)| End::Restored { unmade, shared }),
                Task::Verify { folder, usb } => (|| {
                    if let Some(usb) = &usb { usb.attached()?; }
                    verify(&folder, &tell, &stop, None)?;
                    if let Some(usb) = &usb { usb.attached()?; }
                    Ok(End::Verified)
                })(),
            };
            let end = match ended {
                Ok(end) => end,
                Err(e) if e.is::<Cancelled>() => End::Stopped,
                Err(e) => match e.downcast::<Refusal>() {
                    Ok(refusal) => End::Refused(refusal),
                    Err(e) => End::Failed(format!("{e:#}")),
                },
            };
            let said = match &end {
                End::Saved(folder) => format!("backup saved in {}", folder.display()),
                End::Verified => "backup verification passed without restoring".into(),
                End::Restored { unmade, shared } => format!("backup restored, all but {unmade:?}, shared files {shared:?}"),
                End::Stopped => "backup stopped".into(),
                End::Refused(why) => format!("backup refused: {why}"),
                End::Failed(why) => format!("backup failed: {why}"),
            };
            skyward_sys::kmsg(&said).expect("the kernel log takes a line");
            tell(Event::Ended(end));
        })?;
        Ok(Worker { cancel, worker })
    }

    /// Readable while events wait.
    pub fn fd(&self) -> RawFd {
        self.worker.fd()
    }

    /// The events since the last call.
    pub fn events(&self) -> Vec<Event> {
        self.worker.events()
    }

    /// Ask the work to stop; it does between two chunks, and a restore
    /// only while nothing is written yet.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Where a part comes from for a backup.
enum Source {
    /// A partition of its own, at this device.
    Block(PathBuf),
    /// One of the slot's partitions in super, by its base name.
    Logical(String),
    /// A file archive and the policy for its contents.
    Archive { root: &'static Path, skipped: Skipped },
}

/// A partition by its name among the links the phone gives them.
fn partition(name: &str) -> PathBuf {
    Path::new(BY_NAME).join(name)
}

/// Tells how far a part has come in a pass over it, at most every
/// [`PROGRESS_EVERY`].
struct Telling<'a> {
    tell: &'a dyn Fn(Event),
    item: usize,
    of: u64,
    checking: bool,
    told: Instant,
}

impl Telling<'_> {
    fn new(tell: &dyn Fn(Event), item: usize, of: u64, checking: bool) -> Telling<'_> {
        tell(Event::At { item, done: 0, of, checking });
        Telling { tell, item, of, checking, told: Instant::now() }
    }

    fn done(&mut self, done: u64) {
        if self.told.elapsed() >= PROGRESS_EVERY {
            self.told = Instant::now();
            (self.tell)(Event::At { item: self.item, done, of: self.of, checking: self.checking });
        }
    }
}

/// Copy `len` bytes from `from` to `to`, telling how far, and stopping when
/// asked.
fn copy(from: &mut impl Read, to: &mut impl Write, len: u64, cancel: &AtomicBool, telling: &mut Telling) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < len {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let want = (len - done).min(CHUNK as u64) as usize;
        from.read_exact(&mut buf[..want]).context("read")?;
        to.write_all(&buf[..want]).context("write")?;
        done += want as u64;
        telling.done(done);
    }
    Ok(())
}

/// The reference of the key the data partition's own folders are encrypted
/// with, in hex.
fn data_key() -> Result<String> {
    let bytes = fs::read(DATA_KEY).with_context(|| format!("read {DATA_KEY}"))?;
    Ok(hex::encode(bytes))
}

/// The bytes free where `path` is, or would be made.
pub(crate) fn free_bytes(path: &Path) -> Result<u64> {
    let there = path.ancestors().find(|p| p.exists()).context("the root is always there")?;
    Ok(mount::space(there).with_context(|| format!("look at the room on {}", there.display()))?.available)
}

/// The slot's partitions in super are read or written whole only while no
/// update is changing them.
fn no_update() -> Result<()> {
    let state = skyward_vab::snapshot_state()?;
    if state != SnapshotState::None {
        return Err(Refusal::Update(state).into());
    }
    Ok(())
}

fn backup(t: BackupTask, tell: &dyn Fn(Event), cancel: &AtomicBool) -> Result<PathBuf> {
    let slot = skyward_vab::current_slot()?;
    t.data_scope.validate_parts(t.parts)?;
    let [boot, system, data, efs, shared_files] = t.parts;
    ensure!(!shared_files || t.device == "lmi", "shared files backup is available on lmi");
    ensure!(slot.is_some() || !(boot || system), "boot and system backup require a slot");
    let data_root = t.data_scope.root();
    if system {
        no_update()?;
    }
    let mut plan: Vec<(Part, String, Source, u64)> = Vec::new();
    // What the parts' files take uncompressed: the bytes each holds, and
    // the data partition's archive headers.
    let mut needed = 0;
    if boot {
        let slot = slot.context("boot backup requires a slot")?;
        // The boot partitions are backed up as the installed system's,
        // whose boot set goes back into them first.
        skyward_systems::install::ensure_installed()?;
        for name in BOOT {
            let device = partition(&format!("{name}{}", slot.suffix()));
            if device.exists() {
                let size = skyward_block::size_bytes(&device)?;
                plan.push((Part::Boot, name.to_string(), Source::Block(device), size));
            }
        }
    }
    if system {
        let slot = slot.context("system backup requires a slot")?;
        // A partition of the slot that holds nothing has nothing to keep.
        for (base, size) in skyward_vab::slot_sizes(slot)?.into_iter().filter(|(_, size)| *size > 0) {
            plan.push((Part::System, base.clone(), Source::Logical(base), size));
        }
    }
    for (enabled, part, name) in [(data, Part::Data, t.data_scope.item_name()), (shared_files, Part::SharedFiles, SHARED_NAME)] {
        if !enabled {
            continue;
        }
        let (root, skipped) = if part == Part::SharedFiles {
            (owner_shared_root()?, Skipped::shared_files())
        } else {
            (data_root, Skipped::new(&t.skipped))
        };
        let (bytes, entries) = archive::measure(root, &skipped, cancel).with_context(|| format!("measure {name}"))?;
        plan.push((part, name.into(), Source::Archive { root, skipped }, bytes));
        needed += entries * ENTRY_BYTES;
    }
    if efs {
        for name in &t.efs {
            let device = partition(name);
            ensure!(device.exists(), "the phone has no partition {name}");
            let size = skyward_block::size_bytes(&device)?;
            plan.push((Part::Efs, name.clone(), Source::Block(device), size));
        }
    }
    // Compressed, what the files take is not known until they are written.
    needed += plan.iter().map(|p| p.3).sum::<u64>();
    if let Some(usb) = &t.usb {
        ensure!(t.verify && t.remount.is_none(), "an lmi USB backup must use verified publication");
        usb.attached()?;
    }
    let free = free_bytes(&t.into)?;
    if !t.compress && needed > free {
        return Err(Refusal::Room { needed, free }.into());
    }
    tell(Event::Plan(plan.iter().map(|(_, name, _, size)| (name.clone(), *size)).collect()));
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    if let Some(usb) = &t.usb {
        usb.writable(true)?;
    } else if let Some(at) = &t.remount {
        mount::remount_writable(at, true)?;
    }
    let output = t.into.join(&t.stamp);
    let mut created = false;
    let mut pending = None;
    let made = (|| -> Result<()> {
        skyward_files::make_folders(&t.into, t.ownership)?;
        pending = if t.usb.is_some() {
            ensure!(!output.try_exists()?, "a backup with this timestamp already exists");
            Some(tempfile::Builder::new().prefix(".skyward-incomplete-").tempdir_in(&t.into).context("open a temporary USB backup folder")?)
        } else {
            skyward_files::make_folder(&output, t.ownership)?;
            created = true;
            None
        };
        let folder = pending.as_ref().map_or(output.as_path(), tempfile::TempDir::path);
        let mut items = Vec::new();
        for (k, (part, name, source, size)) in plan.iter().enumerate() {
            let file = Item::file_name(*part, name, t.compress);
            let mut writer = Writer::create(folder, &file, t.compress, t.ownership);
            let mut telling = Telling::new(tell, k, *size, false);
            match source {
                Source::Block(device) => {
                    let mut from = File::open(device).with_context(|| format!("open {}", device.display()))?;
                    copy(&mut from, &mut writer, *size, cancel, &mut telling).with_context(|| format!("back up {name}"))?;
                }
                Source::Logical(base) => {
                    let slot = slot.context("system backup requires a slot")?;
                    let whole = skyward_vab::map_whole(slot, base, &format!("skyward-backup-{base}{}", slot.suffix()), true)?;
                    let copied = File::open(&whole.device)
                        .with_context(|| format!("open {}", whole.device.display()))
                        .and_then(|mut from| copy(&mut from, &mut writer, whole.size, cancel, &mut telling));
                    skyward_vab::unmap_whole(&whole)?;
                    copied.with_context(|| format!("back up {name}"))?;
                }
                Source::Archive { root, skipped } => {
                    archive::pack(root, skipped, &mut writer, cancel, &mut |done| telling.done(done))
                        .with_context(|| format!("back up {name}"))?;
                }
            }
            items.push(Item { part: *part, name: name.clone(), stored: writer.finish()? });
        }
        if t.verify && t.usb.is_none() {
            for (k, item) in items.iter().enumerate() {
                let mut telling = Telling::new(tell, k, item.stored.size, true);
                check(folder, &item.stored, cancel, &mut telling, None).with_context(|| format!("check {}", item.name))?;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let data_key = if data { Some(data_key()?) } else { None };
        let manifest =
            Manifest { format: FORMAT, device: t.device.clone(), made: t.made, slot: slot.map_or("", Slot::suffix).into(), system: t.system.clone(), data_key, items };
        manifest.write(folder, t.ownership)?;
        if let Some(usb) = &t.usb {
            File::open(folder)?.sync_all().context("sync the temporary backup directory")?;
            usb.sync()?;
            verify(folder, tell, cancel, None)?;
            usb.attached()?;
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            store::publish(folder, &output)?;
            let _ = pending.take().expect("external backup is written into a temporary folder").keep();
            created = true;
            File::open(&t.into)?.sync_all().context("sync the published backup directory")?;
            usb.sync()?;
        } else {
            unsafe { libc::sync() };
        }
        Ok(())
    })();
    let made = if let Some(pending) = pending {
        match pending.close() {
            Ok(()) => made,
            Err(cleanup) => match made {
                Ok(()) => Err(cleanup).context("remove the incomplete USB backup"),
                Err(error) => Err(anyhow::anyhow!("{error:#}; could not remove the incomplete USB backup: {cleanup}")),
            },
        }
    } else { made };
    // A non-USB backup, or a USB backup renamed before a final sync
    // failed, belongs to this task only if it created the published path.
    let made = match made {
        Err(e) if created => match fs::remove_dir_all(&output) {
            Ok(()) => Err(e),
            Err(r) => Err(e.context(format!("taking away {} afterwards failed too: {r}", output.display()))),
        },
        other => other,
    };
    let read_only = match (&t.usb, &t.remount) {
        (Some(usb), _) => usb.writable(false),
        (_, Some(at)) => mount::remount_writable(at, false),
        _ => Ok(()),
    };
    match (made, read_only) {
        (Ok(()), Ok(())) => Ok(output),
        (Ok(()), Err(error)) => Err(error.context("the backup was verified and written, but the drive could not return to read-only")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(read_only)) => Err(error.context(format!("could not return the backup drive to read-only: {read_only:#}"))),
    }
}

/// Read a part back from its files whole and see that it is what was
/// stored.
fn check(folder: &Path, stored: &Stored, cancel: &AtomicBool, telling: &mut Telling, restore: Option<(&Path, &Skipped)>) -> Result<u64> {
    let mut reader = Reader::open(folder, stored);
    let needed = if let Some((target, skipped)) = restore {
        let mut done = 0;
        let mut source = (&mut reader).take(stored.size);
        let input = std::io::Read::by_ref(&mut source);
        let input = ReadProgress { input, done: &mut done, cancel, telling };
        let inspected = archive::inspect(input, target, skipped, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        inspected?
    } else {
        copy(&mut reader, &mut io::sink(), stored.size, cancel, telling)?;
        0
    };
    let mut extra = [0];
    ensure!(reader.read(&mut extra).context("finish reading the backup")? == 0, "the backup holds more than was stored");
    reader.check(stored)?;
    Ok(needed)
}

struct ReadProgress<'a, 'b, R> {
    input: R,
    done: &'a mut u64,
    cancel: &'a AtomicBool,
    telling: &'a mut Telling<'b>,
}

impl<R: Read> Read for ReadProgress<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(Cancelled));
        }
        let n = self.input.read(buf)?;
        *self.done += n as u64;
        self.telling.done(*self.done);
        Ok(n)
    }
}

/// A separate task with no restore target, key lookup or write phase.
fn verify(folder: &Path, tell: &dyn Fn(Event), cancel: &AtomicBool, restore: Option<(&Path, &Skipped)>) -> Result<u64> {
    let mut last = None;
    let mut progress = |event| match event {
        skyward_backup::verify::Event::Plan(plan) => tell(Event::Plan(plan)),
        skyward_backup::verify::Event::At { item, done, of } => {
            let now = Instant::now();
            if last.is_none_or(|(previous, at): (usize, Instant)| previous != item || now.duration_since(at) >= PROGRESS_EVERY) || done == of {
                tell(Event::At { item, done, of, checking: true });
                last = Some((item, now));
            }
        }
    };
    match restore {
        Some((root, skipped)) => skyward_backup::verify::verify_restore(folder, root, skipped, cancel, &mut progress),
        None => skyward_backup::verify::verify(folder, cancel, &mut progress).map(|()| 0),
    }
}

/// The index of a part in [`Part::ALL`].
fn index(part: Part) -> usize {
    Part::ALL.iter().position(|&p| p == part).expect("every part is listed")
}

/// A reader that counts the bytes it gives, where the count can be read
/// while the reader is lent out.
struct Counted<'a, R> {
    inner: R,
    read: &'a Cell<u64>,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.set(self.read.get() + n as u64);
        Ok(n)
    }
}

fn owner_shared_root() -> Result<&'static Path> {
    let owner = Path::new(crate::storage::OWNER_STORAGE);
    ensure!(owner.canonicalize()? == owner && skyward_crypto::fscrypt::unlocked(owner)?,
        "decrypt owner internal storage first");
    ensure!(fs::metadata(owner)?.dev() == fs::metadata(DATA)?.dev(), "owner storage is outside data");
    Ok(owner)
}

fn stage_usb_restore(t: &RestoreTask, usb: &UsbMount, tell: &dyn Fn(Event), cancel: &AtomicBool) -> Result<tempfile::TempDir> {
    let [boot, system, data, efs, shared_files] = t.parts;
    ensure!(!(boot || system || efs) && (data || shared_files), "USB restore requires a file archive on lmi");
    usb.attached()?;
    let owner = owner_shared_root()?;
    // Owner shared storage is excluded from the archive and restore sweep.
    // The temporary directory is private and removed on return or failure.
    let scratch = tempfile::Builder::new().prefix(".skyward-restore-")
        .permissions(fs::Permissions::from_mode(0o700)).tempdir_in(owner).context("make a private restore staging directory")?;
    ensure!(fs::metadata(scratch.path())?.mode() & 0o077 == 0, "the staging directory is not private");
    let key = data.then(data_key).transpose()?;
    let mut last = None;
    skyward_backup::verify::stage_archives(&t.folder, scratch.path(), free_bytes(scratch.path())?, cancel, |manifest| {
        if manifest.device != t.device {
            return Err(Refusal::OtherPhone(manifest.device.clone()).into());
        }
        if let Some(key) = &key {
            if manifest.data_key.as_deref() != Some(key.as_str()) {
                return Err(Refusal::DataReset.into());
            }
            let item = manifest.items.iter().find(|item| item.part == Part::Data).context("the backup does not contain data")?;
            ensure!(item.name == t.data_scope.item_name(), "the backup's data scope does not match the restore");
        }
        Ok([(data, Part::Data), (shared_files, Part::SharedFiles)].into_iter()
            .filter_map(|(chosen, part)| chosen.then_some(part)).collect())
    }, |done, of| {
        if done == of || last.is_none_or(|at: Instant| at.elapsed() >= PROGRESS_EVERY) {
            tell(Event::Staging { done, of });
            last = Some(Instant::now());
        }
    })?;
    usb.attached()?;
    Ok(scratch)
}

/// Resolve a shared-storage output directory without taking a path out of
/// the owner's filesystem, then give it normal shared-storage attributes.
fn shared_folder(owner: &Path, child: &str) -> Result<PathBuf> {
    let path = owner.join(child);
    let there = path.ancestors().find(|path| fs::symlink_metadata(path).is_ok()).context("no shared storage destination")?;
    ensure!(there.canonicalize()? == there && fs::metadata(there)?.dev() == fs::metadata(owner)?.dev(),
        "the shared storage destination left the owner's data filesystem");
    skyward_files::make_folders(&path, Ownership::Shared)?;
    ensure!(fs::symlink_metadata(&path)?.is_dir(), "the shared storage destination is not a directory");
    Ok(path)
}

fn prepare_shared_restore(
    owner: &Path, folder: &Path, item: &Item, made: i64, cancel: &AtomicBool, telling: &mut Telling<'_>,
) -> Result<(tempfile::TempDir, PathBuf)> {
    const PREFIX: &str = ".skyward-restore-files-";
    let into = shared_folder(owner, skyward_backup::RESTORED_SHARED)?;
    let temp = tempfile::Builder::new().prefix(PREFIX).tempdir_in(&into)?;
    skyward_files::inherit_folder(temp.path(), Ownership::Shared)?;
    let suffix = temp.path().file_name().and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(PREFIX)).context("invalid restore work folder")?;
    let output = into.join(format!("Files-{made}-{suffix}"));
    ensure!(!output.try_exists()?, "the shared files destination already exists");
    let mut reader = Reader::open(folder, &item.stored);
    let mut done = 0;
    let mut input = ReadProgress { input: (&mut reader).take(item.stored.size), done: &mut done, cancel, telling };
    let extracted = (|| -> Result<()> {
        archive::unpack_shared_files(&mut input, temp.path(), cancel, &mut |_| {})
            .context("restore shared files into a temporary folder")?;
        io::copy(&mut input, &mut io::sink()).context("read the rest of the shared archive")?;
        Ok(())
    })();
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    extracted?;
    let mut extra = [0];
    ensure!(reader.read(&mut extra)? == 0, "the shared archive holds more than was stored");
    reader.check(&item.stored)?;
    File::open(temp.path())?.sync_all()?;
    Ok((temp, output))
}

fn restore(t: RestoreTask, tell: &dyn Fn(Event), cancel: &AtomicBool) -> Result<(Vec<PathBuf>, Option<PathBuf>)> {
    t.data_scope.validate_parts(t.parts)?;
    let staged = t.usb.as_ref().map(|usb| stage_usb_restore(&t, usb, tell, cancel)).transpose()?;
    let folder = staged.as_ref().map_or(t.folder.as_path(), tempfile::TempDir::path);
    let manifest = Manifest::read(folder)?;
    if manifest.device != t.device {
        return Err(Refusal::OtherPhone(manifest.device).into());
    }
    let slot = skyward_vab::current_slot()?;
    let data_root = t.data_scope.root();
    let mut items: Vec<&Item> = manifest.items.iter().filter(|i| t.parts[index(i.part)]).collect();
    ensure!(!items.is_empty(), "nothing chosen to restore is in the backup");
    let shared_files = items.iter().find(|item| item.part == Part::SharedFiles).copied();
    if shared_files.is_some() {
        ensure!(t.device == "lmi" && items.iter().filter(|item| item.part == Part::SharedFiles).count() == 1,
            "expected one shared files archive for lmi");
    }
    items.sort_by_key(|i| RESTORE_ORDER.iter().position(|&p| p == i.part));
    // All that can be seen before anything is written.
    ensure!(slot.is_some() || !items.iter().any(|item| item.part.uses_slot()), "boot and system restore require a slot");
    let system = items.iter().any(|i| i.part == Part::System);
    if system || items.iter().any(|i| i.part == Part::Boot) {
        no_update()?;
    }
    let sizes = if system { skyward_vab::slot_sizes(slot.context("system restore requires a slot")?)? } else { Vec::new() };
    for item in &items {
        item.verify_files()?;
        for name in &item.stored.files {
            let file = folder.join(name);
            ensure!(fs::symlink_metadata(&file)?.is_file(), "{} is not a backup file", file.display());
        }
        match item.part {
            Part::Boot | Part::Efs => {
                let device = target(item, slot)?;
                ensure!(device.exists(), "the phone has no partition {}", device.display());
                let size = skyward_block::size_bytes(&device)?;
                ensure!(size == item.stored.size, "{} holds {size} bytes, and its backup {}", device.display(), item.stored.size);
            }
            Part::System => {
                let size = sizes.iter().find(|(base, _)| *base == item.name).map(|(_, size)| *size);
                if size != Some(item.stored.size) {
                    return Err(Refusal::Resized(item.name.clone()).into());
                }
            }
            Part::Data => {
                ensure!(item.name == t.data_scope.item_name(), "the backup's data scope does not match the restore");
                if manifest.data_key.as_deref() != Some(data_key()?.as_str()) {
                    return Err(Refusal::DataReset.into());
                }
            }
            Part::SharedFiles => ensure!(item.name == SHARED_NAME, "unsupported shared files archive"),
        }
    }
    // A damaged backup is not written anywhere. USB verifies the complete
    // local copy, including raw volume lengths, before it can write.
    let skipped = Skipped::new(&t.skipped);
    let data_needed = if staged.is_some() {
        verify(folder, tell, cancel, Some((data_root, &skipped)))?
    } else {
        let mut data_needed = 0_u64;
        tell(Event::Plan(items.iter().map(|i| (i.name.clone(), i.stored.size)).collect()));
        for (k, item) in items.iter().enumerate() {
            let mut telling = Telling::new(tell, k, item.stored.size, true);
            let target = (item.part == Part::Data).then_some((data_root, &skipped));
            let bytes = check(folder, &item.stored, cancel, &mut telling, target).with_context(|| format!("check {}", item.name))?;
            data_needed = data_needed.checked_add(bytes).context("data restore size overflows")?;
        }
        data_needed
    };
    if staged.is_some() || shared_files.is_some() {
        let needed = data_needed.checked_add(shared_files.map_or(0, |item| item.stored.size)).context("restore size overflows")?;
        let free = free_bytes(data_root)?;
        if needed > free {
            return Err(Refusal::Room { needed, free }.into());
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    // Prepare and validate the new shared files directory before replacing any
    // app data. It is published only after the rest of the restore succeeds.
    let mut prepared = None;
    if let Some((k, &item)) = items.iter().enumerate().find(|(_, item)| item.part == Part::SharedFiles) {
        let mut telling = Telling::new(tell, k, item.stored.size, true);
        prepared = Some(prepare_shared_restore(owner_shared_root()?, folder, item, manifest.made, cancel, &mut telling)?);
        // Small files can take more allocated space than their archive.
        // Check the app-data reserve again after those blocks are allocated.
        let free = free_bytes(data_root)?;
        if data_needed > free {
            return Err(Refusal::Room { needed: data_needed, free }.into());
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    for item in items.iter().filter(|i| matches!(i.part, Part::Boot | Part::Efs)) {
        let device = target(item, slot)?;
        for point in mount::points_of(skyward_block::devnum(&device)?)? {
            mount::umount(&point).with_context(|| format!("{} is in use at {}", device.display(), point.display()))?;
        }
    }
    if system {
        (t.detach)().context("take the slot's partitions off before writing over them")?;
    }
    // The boot partitions are restored as the installed system's, whose
    // boot set goes back into them first, so that the second system's is
    // not left kept as being there.
    if items.iter().any(|i| i.part == Part::Boot) {
        skyward_systems::install::ensure_installed()?;
    }
    // Once anything is written, the restore goes on to its end.
    let go_on = AtomicBool::new(false);
    let mut unmade = Vec::new();
    let mut shared = None;
    for (k, item) in items.iter().enumerate() {
        match item.part {
            Part::Boot | Part::Efs => {
                let mut telling = Telling::new(tell, k, item.stored.size * 2, false);
                write_image(&target(item, slot)?, folder, &item.stored, &mut telling)?;
            }
            Part::System => {
                let slot = slot.context("system restore requires a slot")?;
                let mut telling = Telling::new(tell, k, item.stored.size * 2, false);
                let whole = skyward_vab::map_whole(slot, &item.name, &format!("skyward-restore-{}{}", item.name, slot.suffix()), false)?;
                let written = write_image(&whole.device, folder, &item.stored, &mut telling);
                skyward_vab::unmap_whole(&whole)?;
                written?;
            }
            Part::Data => {
                let mut telling = Telling::new(tell, k, item.stored.size, false);
                let read = Cell::new(0);
                let mut reader = Reader::open(folder, &item.stored);
                let from = Counted { inner: &mut reader, read: &read };
                let unpacked = archive::unpack(from, data_root, &skipped, &go_on, &mut |_| telling.done(read.get()))
                    .context("restore the data partition's files")?;
                io::copy(&mut reader, &mut io::sink()).context("read the rest of the backup")?;
                reader.check(&item.stored)?;
                unmade = unpacked.unmade.iter().map(|u| data_root.join(u)).collect();
            }
            Part::SharedFiles => {
                tell(Event::At { item: k, done: item.stored.size, of: item.stored.size, checking: false });
                let (temp, output) = prepared.take().context("shared files restore was not prepared")?;
                store::publish(temp.path(), &output)?;
                let _ = temp.keep();
                shared = Some(output.strip_prefix(crate::storage::OWNER_STORAGE)?.to_path_buf());
            }
        }
        skyward_sys::kmsg(&format!("restored {}", item.name))?;
    }
    unsafe { libc::sync() };
    Ok((unmade, shared))
}

/// Exercise the normal backup and restore path with an independently
/// installed app containing only disposable test data.
pub fn verify_lmi_fixture() -> Result<()> {
    const PARTS: [bool; 5] = [false, false, true, false, false];
    const FILES: [&str; 3] = ["files/skyward-fixture-state.txt", "shared_prefs/fixture.xml", "databases/fixture.db"];
    let scope = DataScope::LmiFixture;
    let root = scope.root();
    let data = mount::visible(Path::new(DATA))?.context("data is not mounted")?;
    ensure!(data.filesystem == "f2fs" && root.canonicalize()? == root, "unexpected fixture path or filesystem");
    ensure!(skyward_crypto::fscrypt::unlocked(root)?, "the test app data is still locked");
    let meta = fs::metadata(root)?;
    ensure!(meta.is_dir() && (10000..20000).contains(&meta.uid()), "the sample directory does not belong to an app");
    ensure!(fs::read(root.join(FILES[0]))? == b"skyward-lmi-fixture-v2\n", "the fixture marker differs");
    let snapshots = FILES.map(|relative| -> Result<_> {
        let path = root.join(relative);
        let file = fs::symlink_metadata(&path)?;
        ensure!(path.canonicalize()? == path && file.is_file() && file.uid() == meta.uid() && file.dev() == meta.dev(),
            "the fixture entry does not belong to the sample");
        ensure!(file.len() <= 1 << 20, "the fixture entry is too large");
        Ok((path.clone(), fs::read(path)?, file.mode(), file.uid(), file.gid()))
    }).into_iter().collect::<Result<Vec<_>>>()?;
    let skipped = Skipped::new(&[]);
    let cancel = AtomicBool::new(false);
    let (bytes, entries) = archive::measure(root, &skipped, &cancel)?;
    ensure!(bytes <= 4 << 20 && entries <= 100, "unexpected contents in the test app directory");
    ensure!(mount::visible(Path::new("/dev"))?.is_some_and(|m| m.filesystem == "tmpfs"), "the fixture archive must stay in RAM");
    let scratch = tempfile::tempdir_in("/dev").context("create the fixture archive directory")?;
    let folder = backup(BackupTask {
        data_scope: scope,
        parts: PARTS,
        into: scratch.path().to_path_buf(),
        stamp: "backup".into(),
        ownership: Ownership::Kept,
        compress: true,
        verify: true,
        remount: None,
        usb: None,
        made: 0,
        device: "lmi".into(),
        system: None,
        efs: Vec::new(),
        skipped: Vec::new(),
    }, &|_| {}, &cancel)?;
    skyward_sys::kmsg("lmi app fixture: compressed backup verified")?;

    for (path, _, _, _, _) in &snapshots {
        fs::remove_file(path)?;
        ensure!(!path.try_exists()?, "the sample entry is still present");
    }
    skyward_sys::kmsg("lmi app fixture: three original test files removed before restore")?;
    let (unmade, _) = restore(RestoreTask {
        data_scope: scope,
        folder,
        usb: None,
        parts: PARTS,
        device: "lmi".into(),
        skipped: Vec::new(),
        detach: Box::new(|| Ok(())),
    }, &|_| {}, &cancel)?;
    ensure!(unmade.is_empty(), "some fixture entries were skipped");
    for (path, bytes, mode, uid, gid) in snapshots {
        let back = fs::symlink_metadata(&path)?;
        ensure!(back.is_file() && back.mode() == mode && back.uid() == uid && back.gid() == gid,
            "the restored fixture entry lost its file type or permissions");
        ensure!(fs::read(&path)? == bytes, "a restored fixture entry differs");
    }
    scratch.close()?;
    skyward_sys::kmsg("lmi app fixture: private file, preferences and database restored on F2FS")
}

/// The partition a part of a backup goes back to.
fn target(item: &Item, slot: Option<Slot>) -> Result<PathBuf> {
    if item.part == Part::Efs {
        return Ok(partition(&item.name));
    }
    let slot = slot.context("boot restore requires a slot")?;
    Ok(partition(&format!("{}{}", item.name, slot.suffix())))
}

/// Write a part's image to the partition at `device`, then read it back
/// from the storage and see that it holds what was stored: twice the
/// part's bytes, told as they go.
fn write_image(device: &Path, folder: &Path, stored: &Stored, telling: &mut Telling) -> Result<()> {
    let mut reader = Reader::open(folder, stored);
    let mut out = OpenOptions::new().write(true).open(device).with_context(|| format!("open {}", device.display()))?;
    copy(&mut reader, &mut out, stored.size, &AtomicBool::new(false), telling).with_context(|| format!("write {}", device.display()))?;
    reader.check(stored)?;
    out.sync_all().with_context(|| format!("write {}", device.display()))?;
    // What is read back comes from the storage, not the pages just written.
    let rc = unsafe { libc::posix_fadvise(out.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc)).with_context(|| format!("drop the cached pages of {}", device.display()));
    }
    let back = File::open(device).with_context(|| format!("open {}", device.display()))?;
    let read = store::digest(back, stored.size, &mut |n| telling.done(stored.size + n)).with_context(|| format!("read {} back", device.display()))?;
    ensure!(read == stored.sha256, "{} reads back different from the backup", device.display());
    Ok(())
}
