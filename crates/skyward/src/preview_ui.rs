use crate::backup::{self, BackupTask, DataScope, RestoreTask, Task};
use crate::decrypt::{Decryptor, Ended, Job};
use crate::haptics::Haptics;
use skyward_crypto::data::Closed;
use crate::input::{monotonic_ms, Event, Inputs};
use crate::ui::Backlight;
use crate::wallpaper::{self, OWNER_STORAGE};
use anyhow::{ensure, Context, Result};
use skyward_config::{ramdisk, DeviceConfig};
use skyward_install::{block_preflight, sideload_test, Event as InstallEvent, Installer};
use skyward_sys::Power;
use skyward_ui::app::{BackupAt, Data, Effect, MountRow, Place, Screen, Target};
use skyward_ui::surface::kms::KmsSurface;
use skyward_ui::wallpaper::Wallpaper;
use skyward_ui::{App, Canvas, Fonts, Lang, Surface};
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
        handle(&mut app, &mut haptics, &mut installer, &mut data, effects, now)?;
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
        for event in inputs.read()? {
            let effects = match event {
                Event::Touch(touch, at) => app.touch(touch, &fonts, at),
                Event::Key(key, true) => app.key(key, monotonic_ms()),
                Event::Key(key, false) => app.key_up(key, monotonic_ms()),
            };
            handle(&mut app, &mut haptics, &mut installer, &mut data, effects, monotonic_ms())?;
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

    fn idle(&self) -> bool {
        self.backuper.is_none() && self.job.is_none() && self.wallpaper_import.is_none()
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

fn wallpaper_error(app: &App, error: &anyhow::Error) -> String {
    let name = app.state.lang.pick(["壁纸", "桌布", "Wallpaper"]);
    format!("{name}: {error:#}")
}

fn handle(
    app: &mut App,
    haptics: &mut Option<Haptics>,
    installer: &mut Option<Installer>,
    data: &mut DataWork,
    effects: Vec<Effect>,
    now: u64,
) -> Result<()> {
    for effect in effects {
        match effect {
            Effect::Reboot(Target::System) if data.idle() && installer.is_none() => {
                if data.enabled && !data.install_committed {
                    // Use the same orderly shutdown as the installer before init reboots.
                    if let Err(error) = release_for_block_install(true) {
                        hardware_error(app, "关闭数据分区", &error, now);
                    }
                }
                skyward_sys::power(Power::System)?;
            }
            Effect::Reboot(Target::System) => {
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
                if let Err(error) = (|| {
                    ensure!(installer.is_none(), "an installation is in progress");
                    data.start_backup(app, place, compress, verify)
                })() {
                    hardware_error(app, "备份", &error, now);
                }
            }
            Effect::Restore { place, name, parts: [false, false, true, false] } if data.data_backup => {
                if let Err(error) = (|| {
                    ensure!(installer.is_none(), "an installation is in progress");
                    data.start_restore(app, place, &name)
                })() {
                    hardware_error(app, "恢复", &error, now);
                }
            }
            Effect::FindBackups { storage, .. } if data.data_backup => {
                crate::ui::find_backups(app, storage, false, now);
            }
            Effect::DeleteBackup { place: Place::Storage, name } if data.data_backup && data.idle() && installer.is_none() => {
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
                let saved = data.wallpaper_storage(app, installer)
                    .and_then(|root| wallpaper::save_opacity(root, after, crate::ui::ownership(root)?));
                if let Err(error) = saved {
                    let message = wallpaper_error(app, &error);
                    app.wallpaper_opacity_not_saved(before, &message, now);
                }
            }
            Effect::SaveFrostedCards(on) if data.data_backup => {
                let saved = data.wallpaper_storage(app, installer)
                    .and_then(|root| wallpaper::save_frosted_cards(root, on, crate::ui::ownership(root)?));
                match saved {
                    Ok(()) => app.set_frosted_cards(on),
                    Err(error) => app.report(&wallpaper_error(app, &error), now),
                }
            }
            Effect::ResetWallpaper if data.data_backup => {
                match data.wallpaper_storage(app, installer).and_then(wallpaper::reset) {
                    Ok(()) => app.set_wallpaper(None),
                    Err(error) => {
                        let message = wallpaper_error(app, &error);
                        app.report(&message, now);
                    }
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
                    ensure!(data.idle() && installer.is_none(), "finish the current operation before sideload");
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
            Effect::Ticks | Effect::ReadSlots | Effect::ReadMounts => {}
            _ => {
                let message = app.state.lang.pick(["此操作在界面测试中停用", "此操作在介面測試中停用", "Unavailable in the UI preview"]);
                app.report(message, now);
            }
        }
    }
    Ok(())
}
