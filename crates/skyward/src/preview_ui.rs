use crate::backup::{self, BackupTask, DataScope, RestoreTask, Task};
use crate::decrypt::{Decryptor, Ended, Job};
use crate::haptics::Haptics;
use crate::{filework, pty};
use skyward_crypto::data::Closed;
use crate::input::{monotonic_ms, Event, Inputs};
use crate::ui::Backlight;
use crate::wallpaper::{self, OWNER_STORAGE};
use anyhow::{ensure, Context, Result};
use skyward_config::{ramdisk, DeviceConfig};
use skyward_install::{block_preflight, sideload_test, Event as InstallEvent, Installer};
use skyward_ui::app::{BackupAt, Data, Effect, FileOp, FileTask, JobEnd, MountRow, Place, RenameRefused, Screen, Target};
use skyward_ui::surface::kms::KmsSurface;
use skyward_ui::wallpaper::Wallpaper;
use skyward_ui::{App, Canvas, Fonts, Lang, Surface};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

/// Run the regular page renderer and input handlers on an A-only device
/// without starting the recovery services or processing a pending BCB request.
pub fn run() -> Result<()> {
    let cfg = DeviceConfig::load(Path::new("/").join(ramdisk::CONFIG))?;
    exercise(&cfg, false)
}

pub fn inspect(cfg: &DeviceConfig) -> Result<()> {
    exercise(cfg, true).inspect_err(|error| {
        let _ = skyward_sys::kmsg(&format!("A-only recovery failed: {error:#}"));
    })
}

fn exercise(cfg: &DeviceConfig, inspect: bool) -> Result<()> {
    let root = Path::new("/");
    ensure!(!cfg.device.ab && cfg.display.density > 0, "an A-only device with a display is required");
    let fonts = Fonts::load(root.join(ramdisk::FONT), 0, root.join(ramdisk::MONO))?;
    let mut surface = KmsSurface::open("/dev/dri/card0")?;
    let (width, height) = surface.size();
    let mut inputs = Inputs::open(width, height, cfg.touch.writes.clone())?;
    let data_readonly = inspect && root.join(ramdisk::DATA_READONLY).is_file();
    let app_restore_test = data_readonly && root.join(ramdisk::LMI_APP_RESTORE_TEST).is_file();
    let data_backup = data_readonly && root.join(ramdisk::LMI_DATA_BACKUP).is_file();
    let inspection = inspect.then(|| skyward_vab::inspect_unslotted(data_readonly));
    let parts: Vec<_> = inspection.as_ref().and_then(|r| r.as_ref().ok()).into_iter().flatten()
        .filter_map(|p| p.mounted.as_ref().ok()).collect();
    let mut info = if inspect {
        match skyward_info::collect_unslotted(cfg, &parts) {
            Ok(info) => info,
            Err(error) => {
                let _ = skyward_sys::kmsg(&format!("installed system properties: {error:#}"));
                skyward_info::collect(cfg)?
            }
        }
    } else {
        skyward_info::collect(cfg)?
    };

    let mut app = App::new(Screen::Home, width, height, cfg.display.density as f32 / 160.0);
    app.set_screen_corners(cfg.display.corner_radius as f32);
    app.take_settings(None, Some(Lang::Hans));
    // The A-only runtime starts MTP when the user enables it.
    app.state.file_transfer = false;
    if data_backup {
        app.use_lmi_backup();
        app.enable_wallpaper();
    }
    if let Some(inspection) = &inspection {
        app.state.inspect_mounts = true;
        let rows = inspection.as_ref().map(|partitions| partitions.iter().map(|p| {
            let point = match &p.mounted {
                Ok(mounted) => format!("{} · {} · ro", p.point.display(), mounted.filesystem),
                Err(error) => format!("{error:#}"),
            };
            MountRow { base: p.name.clone(), point, mounted: p.mounted.is_ok(), holders: Vec::new() }
        }).collect()).map_err(|error| format!("{error:#}"));
        app.set_mounts(rows);
        if !data_backup {
            app.report("分区检查 · 可从重启菜单返回系统", monotonic_ms());
        }
    } else {
        app.report("界面测试 · 可从重启菜单返回系统", monotonic_ms());
    }
    let mut canvas = Canvas::new(width, height);
    let mut last_screen = None;
    let sideload_test = inspect && Path::new(sideload_test::CERTIFICATES).is_file();
    app.state.sideload_test = sideload_test;
    let block_preflight = inspect && root.join(block_preflight::ENABLED).is_file();
    app.state.block_ota_preflight = block_preflight;
    let block_install = inspect && root.join(ramdisk::BLOCK_OTA_DIGEST).is_file();
    app.state.block_ota_install = block_install;
    app.state.check_signature = sideload_test || block_preflight || block_install;
    let mut installer = None;
    let mut data = DataWork {
        enabled: data_readonly,
        install_committed: false,
        app_restore_test,
        data_backup,
        system: info.os_version.clone(),
        backuper: None,
        transfer: None,
        files: None,
        shell: None,
        terminal_sessions: Vec::new(),
        terminal_check_at: 0,
        wallpaper_import: None,
        fixture_started: false,
        job: None,
        closed: Vec::new(),
    };
    let mut backlight = Backlight::open(&cfg.display)
        .inspect_err(|error| hardware_error(&mut app, "亮度", error, monotonic_ms()))
        .ok();
    let mut haptics = cfg.haptics.as_ref().and_then(|cfg| {
        Haptics::open(cfg)
            .inspect_err(|error| hardware_error(&mut app, "振动", error, monotonic_ms()))
            .ok()
    });

    loop {
        let now = monotonic_ms();
        let effects = app.tick(now);
        handle(&mut app, &mut haptics, &mut installer, &mut data, cfg, &info, &fonts, &canvas, effects, now)?;
        if app.take_dirty() {
            let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let clock = skyward_ui::fmt::clock(seconds, app.state.time_zone);
            app.compose(&mut canvas, &fonts, &info, &clock, now);
            surface.present(&canvas)?;
            if last_screen != Some(app.screen()) {
                surface.settle()?;
                last_screen = Some(app.screen());
                let _ = skyward_sys::kmsg(&format!("UI preview showing {:?}", app.screen()));
            }
        }
        if let Some(light) = &mut backlight {
            if let Err(error) = light.show(app.panel(now)) {
                hardware_error(&mut app, "亮度", &error, now);
                backlight = None;
            }
        }
        let max_wait = if data.wallpaper_import.is_some() { 50 } else { 1000 };
        let millis = app.deadline(now).map_or(max_wait, |due| due.saturating_sub(now).clamp(1, max_wait as u64) as i32);
        let fds = [inputs.fd()].into_iter()
            .chain(installer.as_ref().map(Installer::fd))
            .chain(data.job.as_ref().map(Decryptor::fd))
            .chain(data.backuper.as_ref().map(backup::Worker::fd))
            .chain(data.files.as_ref().map(filework::Worker::fd))
            .chain(data.shell.as_ref().into_iter().flat_map(pty::Shell::fds))
            .collect::<Vec<_>>();
        crate::ui::wait(&fds, millis)?;
        for event in installer.as_ref().map(Installer::events).unwrap_or_default() {
            let finished = matches!(event, InstallEvent::Finished(_));
            app.install(event, monotonic_ms());
            if finished {
                installer = None;
            }
        }
        if let Some(worker) = &data.backuper {
            for event in worker.events() {
                match event {
                    backup::Event::Plan(plan) => app.backup_plan(plan),
                    backup::Event::At { item, done, of, checking } => app.backup_at(BackupAt { item, done, of, checking }),
                    backup::Event::Ended(end) => {
                        app.backup_ended(crate::ui::backup_end(end), monotonic_ms());
                        data.backuper = None;
                        info.storage.free = backup::free_bytes(Path::new(skyward_crypto::data::MOUNT)).ok();
                    }
                }
            }
        }
        let mut files_finished = false;
        if let Some(worker) = &data.files {
            for event in worker.events() {
                match event {
                    filework::Event::Progress(progress) => {
                        app.file_progress((progress.bytes, progress.total_bytes), (progress.entries, progress.total_entries));
                    }
                    filework::Event::Ended(end) => {
                        let result = match end {
                            filework::End::Done => JobEnd::Done,
                            filework::End::Stopped => JobEnd::Stopped,
                            filework::End::Failed(why) => JobEnd::Failed(why),
                        };
                        app.file_job_ended(result, now);
                        files_finished = true;
                    }
                    filework::Event::Relabeling(_) => unreachable!("file work does not relabel"),
                }
            }
        }
        if files_finished {
            data.files = None;
            let folder = app.state.files.path.clone();
            crate::ui::list(&mut app, folder)?;
        }
        let mut ended_session = None;
        if let Some(shell) = &mut data.shell {
            let output = shell.read()?;
            if !output.is_empty() {
                app.term_output(&output);
            }
            if let Some(status) = shell.ended()? {
                let last = shell.read()?;
                if !last.is_empty() {
                    app.term_output(&last);
                }
                app.term_ended(pty::how(status));
                ended_session = Some(shell.session_id());
            }
        }
        if let Some(session) = ended_session {
            data.shell = None;
            if !data.terminal_sessions.contains(&session) {
                data.terminal_sessions.push(session);
            }
            data.terminal_check_at = 0;
        }
        if !data.terminal_sessions.is_empty() && now >= data.terminal_check_at {
            // On a read error keep the session busy: never assume its jobs stopped.
            data.terminal_sessions.retain(|&session| pty::session_has_tasks(session).unwrap_or(true));
            data.terminal_check_at = now.saturating_add(1000);
            if ended_session.is_some() && !data.terminal_sessions.is_empty() {
                app.report(app.state.lang.pick([
                    "终端后台任务尚未结束",
                    "終端背景工作尚未結束",
                    "Terminal background jobs are still running",
                ]), now);
            }
        }
        if let Some(job) = data.wallpaper_import.take_if(|job| job.is_finished()) {
            let result = job.join().unwrap_or_else(|_| Err(anyhow::anyhow!("the wallpaper import stopped")));
            let result = result.map_err(|error| wallpaper_error(&app, &error));
            app.wallpaper_imported(result, monotonic_ms());
        }
        if let Some(ended) = data.job.as_ref().and_then(Decryptor::ended) {
            let was_open = app.state.data.storage_open();
            data.job = None;
            match ended {
                Ended::Opened(Ok(opening)) => {
                    data.closed = opening.closed;
                    info.storage.free = backup::free_bytes(Path::new(skyward_crypto::data::MOUNT)).ok();
                    crate::ui::show_closed(&mut app, &data.closed)?;
                    data.start_fixture()?;
                }
                Ended::Unlocked(Ok(closed)) => {
                    data.closed = closed;
                    crate::ui::show_closed(&mut app, &data.closed)?;
                    data.start_fixture()?;
                    if !data.closed.iter().any(|user| user.user == 0) && root.join(ramdisk::DATA_SAMPLE).is_file() {
                        match verify_sample() {
                            Ok(()) => {
                                skyward_sys::kmsg("synthetic sample: read, archived and restored to RAM; contents match")?;
                                app.report("样本备份和恢复通过", monotonic_ms());
                            }
                            Err(error) => hardware_error(&mut app, "样本备份和恢复", &error, now),
                        }
                    }
                }
                Ended::Opened(Err(error)) => {
                    let _ = skyward_sys::kmsg(&format!("lmi /data: {error}"));
                    app.data_failed(error);
                }
                Ended::Unlocked(Err(error)) => {
                    let _ = skyward_sys::kmsg(&format!("lmi /data unlock: {error}"));
                    app.unlock_failed(error, now);
                }
                Ended::Fixture(result) => {
                    match result {
                        Ok(()) => app.report("测试应用备份与恢复通过", monotonic_ms()),
                        Err(error) => {
                            skyward_sys::kmsg(&format!("lmi app fixture failed: {error}"))?;
                            app.report(&error, now);
                        }
                    }
                    if let Err(error) = skyward_crypto::data::close() {
                        hardware_error(&mut app, "关闭数据分区", &error, now);
                    }
                }
            }
            if data.data_backup && !was_open && app.state.data.storage_open() {
                if let Err(error) = crate::ui::keep_held_screenshots() {
                    app.screenshots_not_kept(&format!("{error:#}"), now);
                }
                let root = Path::new(OWNER_STORAGE);
                match wallpaper::load_frosted_cards(root) {
                    Ok(on) => app.set_frosted_cards(on),
                    Err(error) => {
                        app.set_frosted_cards(false);
                        app.report(&wallpaper_error(&app, &error), now);
                    }
                }
                match wallpaper::load(root, (width, height)) {
                    Ok(saved) => app.set_wallpaper(saved),
                    Err(error) => {
                        let message = wallpaper_error(&app, &error);
                        app.wallpaper_unreadable(&message, now);
                    }
                }
            }
        }
        let offer_storage = app.state.data.storage_open() && !data.install_committed
            && data.idle() && installer.is_none() && app.state.editor.is_none();
        let transfer_status = match &mut data.transfer {
            Some(transfer) if !transfer.service.running() => Err(anyhow::anyhow!("the file transfer service ended")),
            Some(transfer) => transfer.offer(offer_storage, app.state.lang).and_then(|()| {
                if transfer.zone != app.state.time_zone {
                    transfer.service.set_zone(skyward_ui::fmt::rules(app.state.time_zone).clone())?;
                    transfer.zone = app.state.time_zone;
                }
                Ok(())
            }),
            None => Ok(()),
        };
        if let Err(error) = transfer_status {
            let transfer = data.transfer.take().expect("only an active transfer can fail");
            let why = transfer.stop(installer.is_none()).err().unwrap_or(error);
            app.file_transfer_failed(false, &skyward_ui::fmt::transfer_stopped(app.state.lang, &format!("{why:#}")), now);
        }
        for event in inputs.read()? {
            let effects = match event {
                Event::Touch(touch, at) => app.touch(touch, &fonts, at),
                Event::Key(key, true) => app.key(key, monotonic_ms()),
                Event::Key(key, false) => app.key_up(key, monotonic_ms()),
            };
            handle(&mut app, &mut haptics, &mut installer, &mut data, cfg, &info, &fonts, &canvas, effects, monotonic_ms())?;
        }
    }
}

/// Archive one synthetic file from the unlocked shared storage and compare
/// the restored contents in a fresh directory on the recovery's tmpfs.
fn verify_sample() -> Result<()> {
    use anyhow::Context;
    use skyward_backup::archive::{pack, unpack, Skipped};
    use std::fs;
    use std::sync::atomic::AtomicBool;

    const NAME: &str = "sample.txt";
    let expected = fs::read(Path::new("/").join(ramdisk::DATA_SAMPLE))?;
    let source = Path::new("/sdcard/Download/Skyward-Recovery-Test-64dbec");
    let canonical = fs::canonicalize(source)?;
    ensure!(canonical == Path::new("/data/media/0/Download/Skyward-Recovery-Test-64dbec")
        || canonical == source, "the sample path contains a link");
    let mut files = fs::read_dir(source).context("open the sample folder after unlock")?;
    let file = files.next().transpose()?.context("the sample is absent")?;
    ensure!(file.file_name() == NAME && file.file_type()?.is_file() && files.next().is_none(), "unexpected entries in the sample folder");
    ensure!(file.metadata()?.len() == expected.len() as u64, "the sample length changed");
    ensure!(fs::read(file.path())? == expected, "the unlocked sample contents differ");

    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(c"/dev".as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("check the recovery scratch filesystem");
    }
    ensure!(unsafe { info.assume_init() }.f_type as u64 == 0x0102_1994, "the recovery scratch filesystem is not tmpfs");
    let destination = tempfile::tempdir_in("/dev").context("create a scratch directory on tmpfs")?;
    let cancel = AtomicBool::new(false);
    let skipped = Skipped::new(&[]);
    let mut archive = Vec::new();
    pack(source, &skipped, &mut archive, &cancel, &mut |_| {}).context("archive the sample")?;
    let restored = unpack(archive.as_slice(), destination.path(), &skipped, &cancel, &mut |_| {})
        .context("restore the sample to RAM")?;
    ensure!(restored.unmade.is_empty(), "sample entries were skipped");
    ensure!(fs::read(destination.path().join(NAME))? == expected, "the restored sample contents differ");
    destination.close()?;
    Ok(())
}

fn hardware_error(app: &mut App, name: &str, error: &anyhow::Error, now: u64) {
    let message = format!("{name}：{error:#}");
    let _ = skyward_sys::kmsg(&message);
    app.report(&message, now);
}

struct DataWork {
    enabled: bool,
    install_committed: bool,
    app_restore_test: bool,
    data_backup: bool,
    system: Option<String>,
    backuper: Option<backup::Worker>,
    transfer: Option<crate::ui::Transfer>,
    files: Option<filework::Worker>,
    shell: Option<pty::Shell>,
    terminal_sessions: Vec<u32>,
    terminal_check_at: u64,
    wallpaper_import: Option<JoinHandle<Result<Wallpaper>>>,
    fixture_started: bool,
    job: Option<Decryptor>,
    closed: Vec<Closed>,
}

impl DataWork {
    fn wallpaper_storage(&self, app: &App, installer: &Option<Installer>) -> Result<&'static Path> {
        ensure!(app.state.data.storage_open() && !self.install_committed, "decrypt internal storage first");
        ensure!(self.idle() && installer.is_none(), "finish the current operation first");
        Ok(Path::new(OWNER_STORAGE))
    }

    fn start_backup(&mut self, app: &mut App, place: Place, compress: bool, verify: bool) -> Result<()> {
        ensure!(self.can_backup(app) && place == Place::Storage, "decrypt all users before backing up");
        let into = crate::ui::backups_in(place);
        let there = into.ancestors().find(|path| path.exists()).context("no backup destination")?;
        ensure!(there.canonicalize()? == there, "the backup directory contains a symbolic link");
        let made = crate::ui::unix_now()?;
        let task = BackupTask {
            data_scope: DataScope::Partition,
            parts: [false, false, true, false],
            into: into.clone(),
            stamp: skyward_ui::fmt::stamp(made, app.state.time_zone),
            ownership: crate::ui::ownership(there)?,
            compress,
            verify,
            remount: None,
            made,
            device: "lmi".into(),
            system: self.system.clone(),
            efs: Vec::new(),
            skipped: backup::data_skipped(&[]),
        };
        self.backuper = Some(backup::Worker::start(Task::Backup(task))?);
        app.backup_started(false, place, verify);
        Ok(())
    }

    fn start_restore(&mut self, app: &mut App, place: Place, name: &str) -> Result<()> {
        ensure!(self.can_backup(app) && place == Place::Storage, "decrypt all users before restoring");
        let task = RestoreTask {
            data_scope: DataScope::Partition,
            folder: lmi_backup_folder(place, name)?,
            parts: [false, false, true, false],
            device: "lmi".into(),
            skipped: backup::data_skipped(&[]),
            detach: Box::new(|| Ok(())),
        };
        self.backuper = Some(backup::Worker::start(Task::Restore(task))?);
        app.backup_started(true, place, true);
        Ok(())
    }

    fn can_backup(&self, app: &App) -> bool {
        self.data_backup && !self.install_committed && self.idle()
            && self.closed.is_empty() && app.state.data == Data::Open
    }

    fn tasks_idle(&self) -> bool {
        self.backuper.is_none() && self.files.is_none() && self.job.is_none() && self.wallpaper_import.is_none()
    }

    fn idle(&self) -> bool {
        self.tasks_idle() && self.shell.is_none() && self.terminal_sessions.is_empty()
    }

    fn start_fixture(&mut self) -> Result<()> {
        if self.app_restore_test && !self.fixture_started && !self.closed.iter().any(|user| user.user == 0) {
            self.job = Some(Decryptor::start(Job::VerifyLmiAppRestore)?);
            self.fixture_started = true;
        }
        Ok(())
    }
}

fn lmi_backup_folder(place: Place, name: &str) -> Result<std::path::PathBuf> {
    ensure!(!name.is_empty() && !name.starts_with('.') && !name.contains('/'), "invalid backup folder name");
    let parent = crate::ui::backups_in(place);
    ensure!(parent.canonicalize()? == parent, "the backup directory contains a symbolic link");
    let folder = parent.join(name);
    ensure!(std::fs::symlink_metadata(&folder)?.is_dir(), "the selected backup is not a directory");
    Ok(folder)
}

/// The BLOCK updater needs exclusive access to the logical partitions.
fn release_for_block_install(for_services: bool) -> Result<()> {
    if for_services {
        crate::legacy_data::finish().context("close lmi decryption before installation")?;
    }
    skyward_vab::release_inspected(for_services).context("release the lmi logical partitions")?;
    skyward_sys::kmsg("lmi: closed data and released logical partitions")
}

/// Release system partitions while the locally stored OTA stays readable.
fn release_for_local_block_install() -> Result<()> {
    crate::legacy_data::stop_for_local_install().context("stop lmi decryption services before installation")?;
    skyward_vab::release_inspected(true).context("release the lmi logical partitions")?;
    skyward_sys::kmsg("lmi: released logical partitions and kept the local OTA readable")
}

fn wallpaper_error(app: &App, error: &anyhow::Error) -> String {
    let name = app.state.lang.pick(["壁纸", "桌布", "Wallpaper"]);
    format!("{name}: {error:#}")
}

#[allow(clippy::too_many_arguments)]
fn handle(
    app: &mut App,
    haptics: &mut Option<Haptics>,
    installer: &mut Option<Installer>,
    data: &mut DataWork,
    cfg: &DeviceConfig,
    info: &skyward_info::DeviceInfo,
    fonts: &Fonts,
    canvas: &Canvas,
    effects: Vec<Effect>,
    now: u64,
) -> Result<()> {
    for effect in effects {
        match effect {
            Effect::Reboot(target) if target != Target::Fastboot && data.tasks_idle() && installer.is_none() => {
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                if data.enabled {
                    let closed = if data.install_committed {
                        crate::legacy_data::finish()
                    } else {
                        release_for_block_install(true)
                    };
                    if let Err(error) = closed {
                        hardware_error(app, "关闭数据分区", &error, now);
                    }
                }
                crate::ui::power(target)?;
            }
            Effect::Reboot(target) if target != Target::Fastboot => {
                app.report("当前操作完成后可重启", now);
            }
            Effect::OpenData if data.enabled && data.idle() && !data.fixture_started
                && !data.install_committed && installer.is_none() => {
                data.job = Some(Decryptor::start(Job::OpenLmi { readonly: !(data.app_restore_test || data.data_backup) })?);
            }
            Effect::OpenData if data.enabled => {
                app.data_failed("请先结束当前操作，或重启 Recovery 后解密".into());
            }
            Effect::Unlock { user, code } if data.enabled && data.idle() && installer.is_none() && !data.install_committed => {
                data.job = Some(Decryptor::start(Job::Unlock { closed: data.closed.clone(), user, credential: code.into_bytes() })?);
            }
            Effect::Unlock { .. } if data.enabled => {
                app.unlock_failed("请先结束当前操作，或重启 Recovery 后解密".into(), now);
            }
            Effect::Backup { parts: [false, false, true, false], place, compress, verify } if data.data_backup => {
                if data.shell.is_some() || !data.terminal_sessions.is_empty() {
                    app.report(app.state.lang.pick([
                        "请先结束终端及其后台任务",
                        "請先結束終端及其背景工作",
                        "Finish the terminal and its background jobs first",
                    ]), now);
                    continue;
                }
                if let Err(error) = (|| {
                    ensure!(installer.is_none(), "an installation is in progress");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    data.start_backup(app, place, compress, verify)
                })() {
                    hardware_error(app, "备份", &error, now);
                }
            }
            Effect::Restore { place, name, parts: [false, false, true, false] } if data.data_backup => {
                if data.shell.is_some() || !data.terminal_sessions.is_empty() {
                    app.report(app.state.lang.pick([
                        "请先结束终端及其后台任务",
                        "請先結束終端及其背景工作",
                        "Finish the terminal and its background jobs first",
                    ]), now);
                    continue;
                }
                if let Err(error) = (|| {
                    ensure!(installer.is_none(), "an installation is in progress");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    data.start_restore(app, place, &name)
                })() {
                    hardware_error(app, "恢复", &error, now);
                }
            }
            Effect::FindBackups { storage, .. } if data.data_backup => {
                crate::ui::find_backups(app, storage, false, now);
            }
            Effect::DeleteBackup { place: Place::Storage, name } if data.data_backup && data.idle() && installer.is_none() => {
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let result = lmi_backup_folder(Place::Storage, &name)
                    .and_then(|folder| crate::ui::delete_backup(Place::Storage, &folder))
                    .map_err(|error| format!("{error:#}"));
                if !app.backup_deleted(result, now).is_empty() {
                    crate::ui::find_backups(app, true, false, now);
                }
            }
            Effect::StopBackup if data.data_backup => {
                if let Some(worker) = &data.backuper {
                    worker.stop();
                }
            }
            Effect::ReadWallpaperFolder(folder) if data.data_backup => {
                let listed = data.wallpaper_storage(app, installer)
                    .and_then(|root| wallpaper::list(root, &folder))
                    .map_err(|error| format!("{error:#}"));
                app.set_wallpaper_folder(folder, listed, crate::ui::unix_now()?);
            }
            Effect::ImportWallpaper(path) if data.data_backup => {
                let started = (|| -> Result<_> {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    let ownership = crate::ui::ownership(root)?;
                    let screen = app.theme().m;
                    let opacity = app.wallpaper_opacity();
                    Ok(thread::Builder::new().name("wallpaper".into()).spawn(move || {
                        wallpaper::import(root, &path, opacity, (screen.w, screen.h), ownership)
                    })?)
                })();
                match started {
                    Ok(job) => data.wallpaper_import = Some(job),
                    Err(error) => {
                        let message = wallpaper_error(app, &error);
                        app.wallpaper_imported(Err(message), now);
                    }
                }
            }
            Effect::SaveWallpaperOpacity { before, after } if data.data_backup => {
                let saved = (|| {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    wallpaper::save_opacity(root, after, crate::ui::ownership(root)?)
                })();
                if let Err(error) = saved {
                    let message = wallpaper_error(app, &error);
                    app.wallpaper_opacity_not_saved(before, &message, now);
                }
            }
            Effect::SaveFrostedCards(on) if data.data_backup => {
                let saved = (|| {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    wallpaper::save_frosted_cards(root, on, crate::ui::ownership(root)?)
                })();
                match saved {
                    Ok(()) => app.set_frosted_cards(on),
                    Err(error) => app.report(&wallpaper_error(app, &error), now),
                }
            }
            Effect::ResetWallpaper if data.data_backup => {
                let reset = (|| {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    wallpaper::reset(root)
                })();
                match reset {
                    Ok(()) => app.set_wallpaper(None),
                    Err(error) => {
                        let message = wallpaper_error(app, &error);
                        app.report(&message, now);
                    }
                }
            }
            Effect::FileTransfer(on) => {
                match (on, data.transfer.take()) {
                    (true, None) => {
                        let started = (|| -> Result<_> {
                            let transfer = crate::ui::Transfer::start(cfg, info, app.state.time_zone)?;
                            if installer.is_none() {
                                if let Err(error) = skyward_sys::usb::switch(skyward_sys::usb::resting(), crate::ui::USB_TIMEOUT) {
                                    let _ = transfer.stop(true);
                                    return Err(error);
                                }
                            }
                            Ok(transfer)
                        })();
                        match started {
                            Ok(transfer) => data.transfer = Some(transfer),
                            Err(error) => app.file_transfer_failed(false, &skyward_ui::fmt::transfer_failed(app.state.lang, &format!("{error:#}")), now),
                        }
                    }
                    (false, Some(transfer)) => {
                        if let Err(error) = transfer.stop(installer.is_none()) {
                            app.report(&skyward_ui::fmt::transfer_left_on(app.state.lang, &format!("{error:#}")), now);
                        }
                    }
                    (_, transfer) => data.transfer = transfer,
                }
            }
            Effect::ReadFilesystems => {
                app.set_filesystems(crate::ui::read_filesystems().map_err(|error| format!("{error:#}")));
            }
            Effect::ReadDir(folder) => crate::ui::list(app, folder)?,
            Effect::ReadPlace(Place::Storage, folder) => crate::ui::list_place(app, Place::Storage, folder)?,
            Effect::StartShell(folder) => {
                if data.shell.is_some() {
                    app.report("终端已经启动", now);
                    continue;
                }
                if !data.tasks_idle() || installer.is_some() {
                    app.report(app.state.lang.pick(["请先结束当前操作", "請先結束目前操作", "Finish the current operation first"]), now);
                    continue;
                }
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                if data.transfer.is_some() {
                    app.report(app.state.lang.pick([
                        "终端运行期间，电脑文件传输将暂停",
                        "終端執行期間，電腦檔案傳輸將暫停",
                        "Computer file transfer pauses while the terminal runs",
                    ]), now);
                }
                let (rows, cols) = app.term_size(fonts);
                match pty::Shell::start(rows, cols, &cfg.device.codename, Path::new(&folder)) {
                    Ok(shell) => {
                        data.shell = Some(shell);
                        app.term_started(rows, cols);
                    }
                    Err(error) => hardware_error(app, "终端", &error, now),
                }
            }
            Effect::ShellTo(folder) => {
                if let Some(shell) = &mut data.shell {
                    match shell.waiting() {
                        Ok(true) => {
                            if let Err(error) = shell.change_to(&folder) {
                                hardware_error(app, "终端", &error, now);
                            }
                        }
                        Ok(false) => app.report(app.state.lang.pick(skyward_ui::fmt::TERMINAL_BUSY), now),
                        Err(error) => hardware_error(app, "终端", &error, now),
                    }
                }
            }
            Effect::TermInput(bytes) => {
                if let Some(shell) = &mut data.shell {
                    if let Err(error) = shell.write(&bytes) {
                        hardware_error(app, "终端", &error, now);
                    }
                }
            }
            Effect::Screenshot { storage } => {
                let saved = crate::ui::save_screenshot(canvas, storage, app.state.time_zone)
                    .map_err(|error| format!("{error:#}"));
                app.screenshot_saved(saved, now);
            }
            Effect::ExportLogs { recovery, kernel, storage } => {
                let into = if storage {
                    Path::new(skyward_crypto::data::SDCARD).join(crate::ui::STORAGE_LOGS)
                } else {
                    std::path::PathBuf::from(crate::ui::TMP_LOGS)
                };
                match crate::ui::export_logs(recovery, kernel, &into, app.state.time_zone) {
                    Ok(folder) => app.report(&skyward_ui::fmt::exported(app.state.lang, &folder.display().to_string()), now),
                    Err(error) => app.report(&skyward_ui::fmt::export_failed(app.state.lang, &format!("{error:#}")), now),
                }
            }
            Effect::Paste { op, path, into, clash } => {
                if !data.idle() || installer.is_some() {
                    app.report(if data.shell.is_some() || !data.terminal_sessions.is_empty() { "请先结束终端及其后台任务" } else { "请先结束当前操作" }, now);
                    continue;
                }
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let clash = match clash {
                    Some(choice) => crate::ui::clash_rule(choice),
                    None => match skyward_files::clashes(&[std::path::PathBuf::from(&path)], Path::new(&into)) {
                        Ok(taken) if taken.is_empty() => skyward_files::Clash::Refuse,
                        Ok(taken) => {
                            let names = taken.iter().map(|item| item.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned())).collect();
                            app.file_clash(op, path, into, names);
                            continue;
                        }
                        Err(error) => {
                            hardware_error(app, "文件", &error, now);
                            continue;
                        }
                    },
                };
                let ownership = match crate::ui::ownership(Path::new(&into)) {
                    Ok(ownership) => ownership,
                    Err(error) => {
                        hardware_error(app, "文件", &error, now);
                        continue;
                    }
                };
                let (from, destination) = (std::path::PathBuf::from(&path), std::path::PathBuf::from(&into));
                let (task, shown) = match op {
                    FileOp::Copy => (filework::Task::Copy { from, into: destination, clash, ownership }, FileTask::Copy { path, into }),
                    FileOp::Move => (filework::Task::Move { from, into: destination, clash, ownership }, FileTask::Move { path, into }),
                };
                match filework::Worker::start(task) {
                    Ok(worker) => {
                        data.files = Some(worker);
                        app.file_job_started(shown);
                    }
                    Err(error) => hardware_error(app, "文件", &error, now),
                }
            }
            Effect::Delete(path) => {
                if !data.idle() || installer.is_some() {
                    app.report(if data.shell.is_some() || !data.terminal_sessions.is_empty() { "请先结束终端及其后台任务" } else { "请先结束当前操作" }, now);
                    continue;
                }
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let task = filework::Task::Delete { path: std::path::PathBuf::from(&path) };
                match filework::Worker::start(task) {
                    Ok(worker) => {
                        data.files = Some(worker);
                        app.file_job_started(FileTask::Delete { path });
                    }
                    Err(error) => hardware_error(app, "文件", &error, now),
                }
            }
            Effect::StopJob => {
                if let Some(worker) = &data.files {
                    worker.stop();
                }
            }
            Effect::SetMode { path, mode } => {
                if data.idle() && installer.is_none() && !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let result = if data.idle() && installer.is_none() {
                    skyward_files::set_mode(Path::new(&path), mode).map_err(|error| format!("{error:#}"))
                } else {
                    Err("请先结束当前操作".into())
                };
                app.mode_set(mode, result, now);
            }
            Effect::OpenText(path) => {
                if !data.idle() || installer.is_some() {
                    app.report(if data.shell.is_some() || !data.terminal_sessions.is_empty() { "请先结束终端及其后台任务" } else { "请先结束当前操作" }, now);
                    continue;
                }
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let read = skyward_files::read_up_to(Path::new(&path), skyward_ui::editor::MAX_BYTES)
                    .map_err(|error| format!("{error:#}"));
                app.text_opened(fonts, path, read, now);
            }
            Effect::SaveText { path, text } => {
                if data.idle() && installer.is_none() && !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let result = if data.idle() && installer.is_none() {
                    skyward_files::replace(Path::new(&path), text.as_bytes()).map_err(|error| format!("{error:#}"))
                } else {
                    Err("请先结束当前操作".into())
                };
                let saved = result.is_ok();
                app.text_saved(result, now);
                if saved {
                    crate::ui::list(app, app.state.files.path.clone())?;
                }
            }
            Effect::Rename { path, name } => {
                if data.idle() && installer.is_none() && !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let result = if data.idle() && installer.is_none() {
                    skyward_files::rename(Path::new(&path), &name)
                        .map(|_| ())
                        .map_err(|error| if error.is::<skyward_files::Taken>() {
                            RenameRefused::Taken
                        } else {
                            RenameRefused::Failed(format!("{error:#}"))
                        })
                } else {
                    Err(RenameRefused::Failed("请先结束当前操作".into()))
                };
                let renamed = result.is_ok();
                app.renamed(result, now);
                if renamed {
                    crate::ui::list(app, app.state.files.path.clone())?;
                }
            }
            Effect::Haptic => {
                if let Some(motor) = haptics {
                    if let Err(error) = motor.click(app.state.vibration) {
                        hardware_error(app, "振动", &error, now);
                        *haptics = None;
                    }
                }
            }
            Effect::Sideload | Effect::SideloadPreflight
                if app.state.sideload_test || app.state.block_ota_preflight || app.state.block_ota_install =>
            {
                let result = (|| {
                    ensure!(data.shell.is_none() && data.terminal_sessions.is_empty(), "finish the terminal and its background jobs before installing");
                    ensure!(data.tasks_idle() && installer.is_none(), "finish the current operation before sideload");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    if effect == Effect::SideloadPreflight {
                        ensure!(app.state.block_ota_preflight, "the preflight entry is unavailable");
                        Installer::start_block_preflight()
                    } else if app.state.block_ota_install {
                        let use_services = data.enabled;
                        Installer::start_block_install(Box::new(move || release_for_block_install(use_services)))
                    } else {
                        Installer::start_test()
                    }
                })();
                match result {
                    Ok(started) => *installer = Some(started),
                    Err(error) => {
                        let failure = skyward_install::Failure::Error(format!("{error:#}"));
                        app.install(InstallEvent::Finished(skyward_install::Outcome::Failed(failure)), now);
                    }
                }
            }
            Effect::InstallPath(_) | Effect::InstallFile(Place::Storage, _) => {
                let full = match effect {
                    Effect::InstallFile(_, path) => crate::ui::in_place(Place::Storage, &path),
                    Effect::InstallPath(path) => std::path::PathBuf::from(path),
                    _ => unreachable!("only local installation reaches this branch"),
                };
                let started = (|| -> Result<Installer> {
                    ensure!(app.state.block_ota_install, "local installation is unavailable");
                    ensure!(data.shell.is_none() && data.terminal_sessions.is_empty(), "finish the terminal and its background jobs before installing");
                    ensure!(data.tasks_idle() && !data.install_committed && installer.is_none(), "finish the current operation first");
                    ensure!(full.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zip")), "choose the supported full ROM ZIP");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    ensure!(skyward_block::mount::is_mount_point(Path::new("/data"))?, "decrypt internal storage first");
                    ensure!(std::fs::metadata(&full)?.dev() == std::fs::metadata("/data")?.dev(),
                        "choose the supported ROM ZIP from internal storage");
                    Installer::open_block_install(full, Box::new(release_for_local_block_install))
                })();
                match started {
                    Ok(started) => *installer = Some(started),
                    Err(error) => {
                        let failure = skyward_install::Failure::Error(format!("{error:#}"));
                        app.install(InstallEvent::Finished(skyward_install::Outcome::Failed(failure)), now);
                    }
                }
            }
            Effect::Install if app.state.sideload_test || app.state.block_ota_install => {
                if let Some(active) = installer.as_ref() {
                    if app.state.block_ota_install && !app.state.sideload_preflight {
                        data.install_committed = true;
                        data.closed.clear();
                        app.data_failed("请重启 Recovery 后再解密".into());
                        app.set_mounts(Err("安装开始后请重启 Recovery 以重新检查挂载".into()));
                    }
                    active.install();
                }
            }
            Effect::Cancel => {
                if let Some(active) = installer.as_ref() {
                    active.cancel()?;
                }
            }
            Effect::Ticks => {
                if let Some(motor) = haptics {
                    if let Err(error) = motor.ticks() {
                        hardware_error(app, "振动", &error, now);
                        *haptics = None;
                    }
                }
            }
            Effect::ReadSlots | Effect::ReadMounts => {}
            _ => {
                let message = app.state.lang.pick(["此操作在界面测试中停用", "此操作在介面測試中停用", "Unavailable in the UI preview"]);
                app.report(message, now);
            }
        }
    }
    Ok(())
}
