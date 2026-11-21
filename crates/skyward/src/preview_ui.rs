use crate::backup::{self, BackupTask, DataScope, RestoreTask, Task};
use crate::usb_mount::UsbMount;
use crate::decrypt::{Decryptor, Ended, Job};
use crate::haptics::Haptics;
use crate::readings::{Reading, Readings};
use crate::{filework, pty};
use skyward_crypto::data::Closed;
use crate::input::{monotonic_ms, Event, Inputs};
use crate::ui::Backlight;
use crate::storage::{open_owner_file, OWNER_STORAGE};
use crate::wallpaper;
use crate::{data_wipe, wipe};
use anyhow::{ensure, Context, Result};
use skyward_config::{ramdisk, DeviceConfig};
use skyward_install::{block_preflight, sideload_test, Event as InstallEvent, Installer};
use skyward_sys::bcb;
use skyward_ui::app::{BackupAt, Data, Effect, FileOp, FileTask, JobEnd, MountRow, Place, RenameRefused, Screen, Target, USB_DRIVE, Wipe};
use skyward_ui::settings::{Kept, Keeper};
use skyward_ui::surface::kms::KmsSurface;
use skyward_ui::wallpaper::{Wallpaper, WallpaperCrop};
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
    crate::ui::wake_panel_on_restart(&mut surface, width, height)?;
    let mut inputs = Inputs::open(width, height, cfg.touch.writes.clone())?;
    let data_readonly = inspect && root.join(ramdisk::DATA_READONLY).is_file();
    let app_restore_test = data_readonly && root.join(ramdisk::LMI_APP_RESTORE_TEST).is_file();
    let data_backup = data_readonly && root.join(ramdisk::LMI_DATA_BACKUP).is_file();
    let inspection = inspect.then(|| skyward_vab::inspect_unslotted(data_readonly));
    let parts: Vec<_> = inspection.as_ref().and_then(|r| r.as_ref().ok()).into_iter().flatten()
        .filter_map(|p| p.mounted.as_ref().ok()).collect();
    let mut unread_battery = skyward_sys::ToldOnce::default();
    let battery = skyward_info::battery_now();
    crate::ui::untaken(&mut unread_battery, "the battery", battery.as_ref().err())?;
    let mut info = skyward_info::collect_unslotted_with(cfg, &parts, battery.unwrap_or_default())?;

    let mut app = App::new(Screen::Home, width, height, cfg.display.density as f32 / 160.0);
    app.set_screen_corners(cfg.display.corner_radius as f32);
    let kept = if data_backup { crate::settings::read()? } else { Kept::Nothing };
    if let Some(settings) = kept.settings() {
        app.take_appearance(settings);
    }
    let named = info.locale.as_deref().and_then(|locale| Lang::for_locale(locale).ok());
    app.take_settings(None, named);
    let legacy_settings_pending = data_backup && matches!(&kept, Kept::Nothing);
    let settings_keeper = match &kept {
        Kept::Unreadable => Keeper::new(&kept),
        _ => Keeper::new(&Kept::Settings(app.settings().appearance())),
    };
    // The A-only runtime starts MTP when the user enables it.
    app.state.file_transfer = false;
    app.state.lmi_wipe = data_backup && cfg.device.codename == "lmi";
    app.state.lmi_data_keep_media =
        app.state.lmi_wipe && root.join(ramdisk::LMI_DATA_KEEP_MEDIA).is_file();
    if app.state.lmi_wipe {
        app.state.cache_partition = wipe::lmi_cache_plan().is_ok_and(|plan| plan.is_some());
        app.state.lmi_format_data = wipe::lmi_format_plan().is_ok();
    }
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
    let readings = Readings::battery_only()?;
    let sideload_test = inspect && Path::new(sideload_test::CERTIFICATES).is_file();
    app.state.sideload_test = sideload_test;
    let lmi_zip = inspect && cfg.device.codename == "lmi" && root.join(ramdisk::LMI_ZIP_INSTALL).is_file();
    app.state.lmi_zip_install = lmi_zip;
    let block_preflight = lmi_zip || (inspect && root.join(block_preflight::ENABLED).is_file());
    app.state.block_ota_preflight = block_preflight;
    let block_install = lmi_zip || (inspect && root.join(ramdisk::BLOCK_OTA_DIGEST).is_file());
    app.state.block_ota_install = block_install;
    app.state.check_signature = sideload_test || block_preflight || block_install;
    let mut installer = None;
    let mut data = DataWork {
        enabled: data_readonly,
        needs_reboot: false,
        app_restore_test,
        data_backup,
        system: info.os_version.clone(),
        backuper: None,
        eraser: None,
        usb_drive: None,
        transfer: None,
        files: None,
        shell: None,
        terminal_sessions: Vec::new(),
        terminal_check_at: 0,
        wallpaper_job: None,
        settings_keeper,
        legacy_settings_pending,
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

    let watch_usb = (data_backup || lmi_zip) && cfg.device.codename == "lmi";
    let mut plugged = None;
    let mut usb_seen = None;
    let mut next_usb = 0;
    let mut unread_usb = skyward_sys::ToldOnce::default();
    loop {
        let now = monotonic_ms();
        if watch_usb && now >= next_usb {
            next_usb = now.saturating_add(1000);
            let may_mount = data.idle() && !data.needs_reboot && installer.is_none();
            let probe = crate::ui::look_for_drive(
                &mut plugged, &mut usb_seen, may_mount.then_some(skyward_block::usb::mount_fat as crate::ui::DriveMount),
            );
            crate::ui::untaken(&mut unread_usb, "the USB drive", probe.as_ref().err())?;
            if let Ok(Some(drive)) = probe {
                if drive.is_none()
                    && app.state.backup_job.as_ref().is_some_and(|job| job.place == Place::UsbDrive)
                    && let Some(worker) = &data.backuper
                {
                    worker.stop();
                }
                data.usb_drive = plugged.as_ref().and_then(crate::ui::Plugged::readable_drive);
                let effects = app.set_usb_drive(drive, now);
                handle(&mut app, &mut haptics, &mut installer, &mut data, cfg, &info, &fonts, &canvas, effects, now)?;
            }
        }
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
        if let Some(light) = &mut backlight
            && let Err(error) = light.show(app.panel(now))
        {
            hardware_error(&mut app, "亮度", &error, now);
            backlight = None;
        }
        let max_wait = if data.wallpaper_job.is_some() { 50 } else { 1000 };
        let millis = app.deadline(now).map_or(max_wait, |due| due.saturating_sub(now).clamp(1, max_wait as u64) as i32);
        let fds = [inputs.fd(), readings.fd()].into_iter()
            .chain(installer.as_ref().map(Installer::fd))
            .chain(data.job.as_ref().map(Decryptor::fd))
            .chain(data.backuper.as_ref().map(backup::Worker::fd))
            .chain(data.eraser.as_ref().map(wipe::Eraser::fd))
            .chain(data.files.as_ref().map(filework::Worker::fd))
            .chain(data.shell.as_ref().into_iter().flat_map(pty::Shell::fds))
            .collect::<Vec<_>>();
        crate::ui::wait(&fds, millis)?;
        for reading in readings.taken() {
            let Reading::Battery(battery, _) = reading else { continue };
            crate::ui::untaken(&mut unread_battery, "the battery", battery.as_ref().err())?;
            if let Ok(battery) = battery
                && battery != info.battery
            {
                crate::ui::plug(&mut app, &info.battery, &battery, monotonic_ms())?;
                info.battery = battery;
                app.refresh();
            }
        }
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
                    backup::Event::Staging { done, of } => app.backup_staging(done, of),
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
        for event in data.eraser.as_ref().map(wipe::Eraser::events).unwrap_or_default() {
            match event {
                wipe::Event::At(step) => app.erase_at(crate::ui::part(step)),
                wipe::Event::Progress(done, total) => app.erase_progress(done, total),
                wipe::Event::NeedsReboot => {
                    data.needs_reboot = true;
                    app.data_failed(app.state.lang.pick([
                        "为完成本次 Data 清除，请重启系统",
                        "為完成本次 Data 清除，請重新啟動系統",
                        "Reboot to Android to finish wiping data",
                    ]).to_string());
                }
                wipe::Event::Opened(_) => unreachable!("advanced wipe opens its data root before running"),
                wipe::Event::Ended(outcome) => {
                    let summary = outcome.as_ref().map_or_else(wipe::said, |_| "done".into());
                    let _ = skyward_sys::kmsg(&format!("lmi advanced wipe: {summary}"));
                    app.erase_ended(outcome.map_err(crate::ui::erase_stop), monotonic_ms());
                    data.eraser = None;
                    info.storage.free = backup::free_bytes(Path::new(skyward_crypto::data::MOUNT)).ok();
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
        if let Some(job) = data.wallpaper_job.take_if(|job| job.is_finished()) {
            let now = monotonic_ms();
            match job {
                WallpaperJob::Prepare(job) => app.wallpaper_prepared(wallpaper_result(&app, job), now),
                WallpaperJob::Save(job) => app.wallpaper_applied(wallpaper_result(&app, job), now),
            }
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
                data.migrate_settings(&mut app, now);
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
        let offer_storage = app.state.data.storage_open() && !data.needs_reboot
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
        if installer.is_none() {
            data.keep_settings(&mut app, monotonic_ms(), false);
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
    ensure!(unsafe { info.assume_init() }.f_type == 0x0102_1994, "the recovery scratch filesystem is not tmpfs");
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

enum WallpaperJob {
    Prepare(JoinHandle<Result<WallpaperCrop>>),
    Save(JoinHandle<Result<Wallpaper>>),
}

impl WallpaperJob {
    fn is_finished(&self) -> bool {
        match self {
            Self::Prepare(job) => job.is_finished(),
            Self::Save(job) => job.is_finished(),
        }
    }
}

fn wallpaper_result<T>(app: &App, job: JoinHandle<Result<T>>) -> Result<T, String> {
    job.join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the wallpaper task stopped")))
        .map_err(|error| wallpaper_error(app, &error))
}

struct DataWork {
    enabled: bool,
    needs_reboot: bool,
    app_restore_test: bool,
    data_backup: bool,
    system: Option<String>,
    backuper: Option<backup::Worker>,
    eraser: Option<wipe::Eraser>,
    usb_drive: Option<(skyward_block::usb::Drive, i32)>,
    transfer: Option<crate::ui::Transfer>,
    files: Option<filework::Worker>,
    shell: Option<pty::Shell>,
    terminal_sessions: Vec<u32>,
    terminal_check_at: u64,
    wallpaper_job: Option<WallpaperJob>,
    settings_keeper: Keeper,
    legacy_settings_pending: bool,
    fixture_started: bool,
    job: Option<Decryptor>,
    closed: Vec<Closed>,
}

impl DataWork {
    fn keep_settings(&mut self, app: &mut App, now: u64, flush: bool) {
        if !self.data_backup || self.needs_reboot || !self.tasks_idle() {
            return;
        }
        let settings = app.settings().appearance();
        let due = if flush { self.settings_keeper.pending(settings) } else { self.settings_keeper.due(settings, now) };
        let Some(due) = due else { return };
        match crate::settings::write(&due) {
            Ok(()) => {
                self.settings_keeper.written(due);
                self.legacy_settings_pending = false;
            }
            Err(error) => {
                self.settings_keeper.failed();
                let name = app.state.lang.pick(["保存设置", "儲存設定", "Save settings"]);
                hardware_error(app, name, &error, now);
            }
        }
    }

    fn migrate_settings(&mut self, app: &mut App, now: u64) {
        if !std::mem::take(&mut self.legacy_settings_pending) {
            return;
        }
        // A newer choice made before unlocking takes priority over the old
        // shared-storage record.
        if self.settings_keeper.pending(app.settings().appearance()).is_none() {
            match wallpaper::load_settings(Path::new(OWNER_STORAGE)) {
                Ok(Some(settings)) => {
                    app.take_appearance(&settings);
                    self.settings_keeper.remade();
                }
                Ok(None) => {}
                Err(error) => {
                    let name = app.state.lang.pick(["读取旧设置", "讀取舊設定", "Read old settings"]);
                    hardware_error(app, name, &error, now);
                }
            }
        }
        self.keep_settings(app, now, true);
    }

    fn wallpaper_storage(&self, app: &App, installer: &Option<Installer>) -> Result<&'static Path> {
        ensure!(app.state.data.storage_open() && !self.needs_reboot, "decrypt internal storage first");
        ensure!(self.idle() && installer.is_none(), "finish the current operation first");
        Ok(Path::new(OWNER_STORAGE))
    }

    fn start_backup(&mut self, app: &mut App, parts: [bool; 5], place: Place, compress: bool, verify: bool) -> Result<()> {
        self.check_backup_scope(app, parts)?;
        let usb = self.backup_usb(app, place)?;
        let visible = crate::ui::backups_in(place);
        let there = visible.ancestors().find(|path| path.exists()).context("no backup destination")?;
        ensure!(there.canonicalize()? == there, "the backup directory contains a symbolic link");
        let ownership = if usb.is_some() { skyward_files::Ownership::Kept } else { crate::ui::ownership(there)? };
        let into = usb.as_ref().map_or(visible, UsbMount::backups);
        let verify = verify || usb.is_some();
        let made = crate::ui::unix_now()?;
        let task = BackupTask {
            data_scope: DataScope::Partition,
            parts,
            into,
            stamp: skyward_ui::fmt::stamp(made, app.state.time_zone),
            ownership,
            compress,
            verify,
            remount: None,
            usb,
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

    fn start_restore(&mut self, app: &mut App, parts: [bool; 5], place: Place, name: &str) -> Result<()> {
        self.check_backup_scope(app, parts)?;
        let usb = self.backup_usb(app, place)?;
        let folder = lmi_backup_folder(place, name)?;
        let folder = usb.as_ref().map_or(folder, |drive| drive.backups().join(name));
        let task = RestoreTask {
            data_scope: DataScope::Partition,
            folder,
            usb,
            parts,
            device: "lmi".into(),
            skipped: backup::data_skipped(&[]),
            detach: Box::new(|| Ok(())),
        };
        self.backuper = Some(backup::Worker::start(Task::Restore(task))?);
        app.backup_started(true, place, true);
        Ok(())
    }

    fn start_verification(&mut self, app: &mut App, place: Place, name: &str) -> Result<()> {
        ensure!(self.data_backup && !self.needs_reboot && self.idle(), "finish the current operation first");
        if place == Place::Storage {
            ensure!(app.state.data.storage_open(), "decrypt internal storage first");
        }
        let usb = self.backup_usb(app, place)?;
        let folder = lmi_backup_folder(place, name)?;
        let folder = usb.as_ref().map_or(folder, |drive| drive.backups().join(name));
        self.backuper = Some(backup::Worker::start(Task::Verify { folder, usb })?);
        app.verification_started(place);
        Ok(())
    }

    fn backup_usb(&self, app: &App, place: Place) -> Result<Option<UsbMount>> {
        if place == Place::Storage {
            return Ok(None);
        }
        self.open_usb(app).map(Some)
    }

    fn open_usb(&self, app: &App) -> Result<UsbMount> {
        ensure!(app.state.place_open(Place::UsbDrive), "connect a readable FAT32 or exFAT USB drive");
        let (drive, id) = self.usb_drive.clone().context("the USB drive is no longer mounted")?;
        UsbMount::open(drive, Path::new(USB_DRIVE), id)
    }

    fn check_backup_scope(&self, app: &App, parts: [bool; 5]) -> Result<()> {
        ensure!(self.data_backup && !self.needs_reboot && self.idle(), "finish the current operation first");
        let [boot, system, data, efs, shared] = parts;
        ensure!(!(boot || system || efs) && (data || shared), "choose data or shared files");
        ensure!(!data || (self.closed.is_empty() && app.state.data == Data::Open), "decrypt all users before processing data");
        ensure!(!shared || app.state.data.storage_open(), "decrypt owner internal storage first");
        Ok(())
    }

    fn start_advanced_wipe(&mut self, app: &mut App, selected: Vec<Wipe>, now: u64) -> Result<()> {
        ensure!(!self.needs_reboot && self.idle(), "finish the current operation before wiping");
        ensure!(!selected.is_empty(), "choose what to wipe");
        let mut choices = Vec::new();
        let mut cache = None;
        for item in selected {
            let choice = match item {
                Wipe::DataKeepMedia => {
                    ensure!(app.state.lmi_data_keep_media, "Data wipe is not enabled in this image");
                    (wipe::Step::DataKeepMedia, data_wipe::Group::DataKeepMedia)
                },
                Wipe::AppData => (wipe::Step::AppData, data_wipe::Group::AppData),
                Wipe::Media => (wipe::Step::Media, data_wipe::Group::Media),
                Wipe::AppCaches => (wipe::Step::AppCaches, data_wipe::Group::AppCaches),
                Wipe::SystemCaches => (wipe::Step::SystemCaches, data_wipe::Group::SystemCaches),
                Wipe::Art => (wipe::Step::Art, data_wipe::Group::Art),
                Wipe::Cache if app.state.cache_partition => {
                    cache = Some(wipe::lmi_cache_plan().map_err(|stop| anyhow::anyhow!(wipe::said(&stop)))?
                        .context("the cache partition disappeared")?);
                    continue;
                }
                _ => anyhow::bail!("the selected item cannot be wiped on lmi"),
            };
            choices.push(choice);
        }
        if !choices.is_empty() {
            ensure!(self.closed.is_empty() && app.state.data == Data::Open, "decrypt all users before wiping items in data");
        }
        if !crate::ui::withhold(&mut self.transfer, app, now)? {
            return Ok(());
        }
        let root = (!choices.is_empty()).then(data_wipe::DataRoot::open_lmi_data).transpose()?;
        let parts = choices.iter().map(|&(step, _)| step)
            .chain(cache.as_ref().map(|_| wipe::Step::Cache)).map(crate::ui::part).collect();
        self.eraser = Some(wipe::Eraser::start(move |tell| {
            if let Some(root) = root {
                wipe::clear_lmi_groups(root, choices, tell)?;
            }
            if let Some(cache) = cache {
                cache.carry_out(tell)?;
            }
            Ok(())
        })?);
        app.state.wipe.fill(false);
        app.erase_started(false, false, parts);
        Ok(())
    }

    fn start_format_data(&mut self, app: &mut App, now: u64) -> Result<()> {
        ensure!(self.idle() && (!self.needs_reboot || app.state.erase.reset), "finish the current operation before formatting");
        let plan = wipe::lmi_format_plan().map_err(|stop| anyhow::anyhow!(wipe::said(&stop)))?;
        if !crate::ui::withhold(&mut self.transfer, app, now)? {
            return Ok(());
        }
        let parts = plan.steps().into_iter().map(crate::ui::part).collect();
        let eraser = wipe::Eraser::start(move |tell| {
            crate::legacy_data::finish().map_err(|error| wipe::Stop::Closed(format!("{error:#}")))?;
            plan.carry_out(true, tell)
        })?;
        self.needs_reboot = true;
        self.closed.clear();
        self.eraser = Some(eraser);
        app.data_failed(app.state.lang.pick([
            "格式化后需重启系统以重建 data",
            "格式化後需重啟系統以重建 data",
            "Reboot to Android after formatting to recreate data",
        ]).to_string());
        app.state.wipe.fill(false);
        app.erase_started(true, false, parts);
        Ok(())
    }

    fn tasks_idle(&self) -> bool {
        self.backuper.is_none() && self.eraser.is_none() && self.files.is_none() && self.job.is_none() && self.wallpaper_job.is_none()
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
    crate::ui::checked_backup_folder(place, name)
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
                // The A-only launcher leaves misc alone on entry. Honor an
                // explicit system reboot in the full lmi image by retiring a
                // pending recovery request. Inspection images only read misc.
                if target == Target::System && app.state.lmi_zip_install {
                    match bcb::clear_recovery_request(&bcb::misc()) {
                        Ok(true) => {
                            let _ = skyward_sys::kmsg("lmi: cleared a pending recovery request before rebooting to Android");
                        }
                        Ok(false) => {}
                        Err(error) => {
                            let name = app.state.lang.pick([
                                "无法清除 Recovery 启动请求",
                                "無法清除 Recovery 啟動請求",
                                "Could not clear the recovery boot request",
                            ]);
                            hardware_error(app, name, &error, now);
                            continue;
                        }
                    }
                }
                data.keep_settings(app, now, true);
                if data.enabled {
                    let closed = if data.needs_reboot {
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
                && !data.needs_reboot && installer.is_none() => {
                data.keep_settings(app, now, true);
                data.job = Some(Decryptor::start(Job::OpenLmi { readonly: !(data.app_restore_test || data.data_backup) })?);
            }
            Effect::OpenData if data.enabled => {
                app.data_failed("请先结束当前操作，或重启 Recovery 后解密".into());
            }
            Effect::Unlock { user, code } if data.enabled && data.idle() && installer.is_none() && !data.needs_reboot => {
                data.keep_settings(app, now, true);
                data.job = Some(Decryptor::start(Job::Unlock { closed: data.closed.clone(), user, credential: code.into_bytes() })?);
            }
            Effect::Unlock { .. } if data.enabled => {
                app.unlock_failed("请先结束当前操作，或重启 Recovery 后解密".into(), now);
            }
            Effect::Wipe(selected) if app.state.lmi_wipe => {
                let title = app.state.lang.pick(["高级清除", "進階清除", "Advanced wipe"]);
                let result = (|| {
                    ensure!(installer.is_none(), "finish the installation before wiping");
                    data.start_advanced_wipe(app, selected, now)
                })();
                if let Err(error) = result {
                    hardware_error(app, title, &error, now);
                }
            }
            Effect::FactoryReset if app.state.lmi_format_data && app.screen() == Screen::FormatData => {
                let result = (|| {
                    ensure!(installer.is_none(), "finish the installation before formatting");
                    data.start_format_data(app, now)
                })();
                if let Err(error) = result {
                    let title = app.state.lang.pick(["格式化 data", "格式化 data", "Format data"]);
                    hardware_error(app, title, &error, now);
                }
            }
            Effect::Dismiss if app.state.lmi_wipe && app.state.erase.outcome.is_some() => {}
            Effect::Backup { parts, place, compress, verify } if data.data_backup => {
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
                    data.start_backup(app, parts, place, compress, verify)
                })() {
                    hardware_error(app, "备份", &error, now);
                }
            }
            Effect::Restore { place, name, parts } if data.data_backup => {
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
                    data.start_restore(app, parts, place, &name)
                })() {
                    hardware_error(app, "恢复", &error, now);
                }
            }
            Effect::VerifyBackup { place, name } if data.data_backup => {
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
                    data.start_verification(app, place, &name)
                })() {
                    let title = app.state.lang.pick(["校验备份", "驗證備份", "Verify backup"]);
                    hardware_error(app, title, &error, now);
                }
            }
            Effect::FindBackups { storage, usb } if data.data_backup => {
                let usb = usb && app.state.place_open(Place::UsbDrive);
                crate::ui::find_backups(app, storage, usb, now);
            }
            Effect::DeleteBackup { place: Place::Storage, name } if data.data_backup && data.idle() && installer.is_none() => {
                if !crate::ui::withhold(&mut data.transfer, app, now)? {
                    continue;
                }
                let result = lmi_backup_folder(Place::Storage, &name)
                    .and_then(|folder| crate::ui::delete_backup(Place::Storage, &folder))
                    .map_err(|error| format!("{error:#}"));
                if !app.backup_deleted(result, now).is_empty() {
                    let usb = app.state.place_open(Place::UsbDrive);
                    crate::ui::find_backups(app, true, usb, now);
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
            Effect::PrepareWallpaper(path) if data.data_backup => {
                let started = (|| -> Result<_> {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    let screen = app.theme().m;
                    Ok(thread::Builder::new().name("wallpaper".into()).spawn(move || {
                        wallpaper::prepare(root, &path, (screen.w, screen.h))
                    })?)
                })();
                match started {
                    Ok(job) => data.wallpaper_job = Some(WallpaperJob::Prepare(job)),
                    Err(error) => app.wallpaper_prepared(Err(wallpaper_error(app, &error)), now),
                }
            }
            Effect::SaveWallpaper if data.data_backup => {
                let started = (|| -> Result<_> {
                    let root = data.wallpaper_storage(app, installer)?;
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    let ownership = crate::ui::ownership(root)?;
                    let opacity = app.wallpaper_opacity();
                    let crop = app.wallpaper_crop().cloned().context("select a picture first")?;
                    Ok(thread::Builder::new().name("wallpaper".into()).spawn(move || {
                        wallpaper::apply(root, &crop, opacity, ownership)
                    })?)
                })();
                match started {
                    Ok(job) => data.wallpaper_job = Some(WallpaperJob::Save(job)),
                    Err(error) => app.wallpaper_applied(Err(wallpaper_error(app, &error)), now),
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
                            if installer.is_none()
                                && let Err(error) = skyward_sys::usb::switch(skyward_sys::usb::resting(), crate::ui::USB_TIMEOUT)
                            {
                                let _ = transfer.stop(true);
                                return Err(error);
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
            Effect::ReadPlace(place, folder) => crate::ui::list_place(app, place, folder)?,
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
                data.keep_settings(app, now, true);
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
                if let Some(shell) = &mut data.shell
                    && let Err(error) = shell.write(&bytes)
                {
                    hardware_error(app, "终端", &error, now);
                }
            }
            Effect::Screenshot { storage } => {
                let saved = crate::ui::save_screenshot(canvas, storage && data.eraser.is_none(), app.state.time_zone)
                    .map_err(|error| format!("{error:#}"));
                app.screenshot_saved(saved, now);
            }
            Effect::ExportLogs { recovery, kernel, storage } => {
                let into = if storage && data.eraser.is_none() {
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
                if let Some(motor) = haptics
                    && let Err(error) = motor.click(app.state.vibration)
                {
                    hardware_error(app, "振动", &error, now);
                    *haptics = None;
                }
            }
            Effect::Sideload | Effect::SideloadPreflight
                if app.state.sideload_test || app.state.block_ota_preflight || app.state.block_ota_install =>
            {
                let result = (|| {
                    ensure!(data.shell.is_none() && data.terminal_sessions.is_empty(), "finish the terminal and its background jobs before installing");
                    ensure!(data.tasks_idle() && installer.is_none(), "finish the current operation before sideload");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    let preflight = effect == Effect::SideloadPreflight;
                    if app.state.lmi_zip_install {
                        let use_services = data.enabled;
                        Installer::start_lmi_zip(preflight, Box::new(move || release_for_block_install(use_services)))
                    } else if preflight {
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
            Effect::InstallPath(_) | Effect::InstallFile(_, _) => {
                let full = match effect {
                    Effect::InstallFile(place, path) => crate::ui::in_place(place, &path),
                    Effect::InstallPath(path) => std::path::PathBuf::from(path),
                    _ => unreachable!("only local installation reaches this branch"),
                };
                let started = (|| -> Result<Installer> {
                    ensure!(app.state.block_ota_install, "local installation is unavailable");
                    ensure!(data.shell.is_none() && data.terminal_sessions.is_empty(), "finish the terminal and its background jobs before installing");
                    ensure!(data.tasks_idle() && !data.needs_reboot && installer.is_none(), "finish the current operation first");
                    let is_image = full.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("img"));
                    let is_zip = full.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
                    if let Ok(relative) = full.strip_prefix(USB_DRIVE) {
                        ensure!(app.state.lmi_zip_install && is_zip, "choose a ZIP on the USB drive");
                        let usb = data.open_usb(app)?;
                        let file = usb.open_zip(relative)?;
                        ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer is still active");
                        let use_services = data.enabled;
                        return Installer::open_lmi_zip(file, Box::new(move || {
                            usb.attached()?;
                            release_for_block_install(use_services)?;
                            usb.attached()
                        }));
                    }
                    ensure!(is_image || is_zip, "choose an IMG or an install ZIP");
                    ensure!(app.state.data.storage_open(), "decrypt internal storage first");
                    ensure!(crate::ui::withhold(&mut data.transfer, app, now)?, "file transfer still holds internal storage");
                    ensure!(skyward_block::mount::is_mount_point(Path::new("/data"))?, "decrypt internal storage first");
                    let data_device = std::fs::metadata("/data")?.dev();
                    if is_image || app.state.lmi_zip_install {
                        let file = open_owner_file(Path::new(OWNER_STORAGE), Path::new(skyward_crypto::data::SDCARD), &full)?;
                        ensure!(file.metadata()?.dev() == data_device, "the file is not on the data partition");
                        if is_image {
                            Installer::open_lmi_image(file)
                        } else {
                            Installer::open_lmi_zip(file, Box::new(release_for_local_block_install))
                        }
                    } else {
                        ensure!(std::fs::metadata(&full)?.dev() == data_device, "choose the supported ROM ZIP from internal storage");
                        Installer::open_block_install(full, Box::new(release_for_local_block_install))
                    }
                })();
                match started {
                    Ok(started) => *installer = Some(started),
                    Err(error) => {
                        let failure = skyward_install::Failure::Error(format!("{error:#}"));
                        app.install(InstallEvent::Finished(skyward_install::Outcome::Failed(failure)), now);
                    }
                }
            }
            Effect::Flash { partition, pick: skyward_ui::app::Pick::Single } => {
                if let Some(active) = installer.as_ref() {
                    data.keep_settings(app, now, true);
                    active.flash(partition, Vec::new());
                } else {
                    app.install(InstallEvent::Finished(skyward_install::Outcome::Failed(
                        skyward_install::Failure::Error("no prepared local image is available".into()),
                    )), now);
                }
            }
            Effect::Install | Effect::InstallUnverifiedZip if app.state.sideload_test || app.state.block_ota_install => {
                if app.state.sideload_preflight || (effect == Effect::InstallUnverifiedZip && !app.state.lmi_zip_install) {
                    continue;
                }
                if let Some(active) = installer.as_ref() {
                    // A swipe can arrive between USB probes. Check the drive before
                    // marking data and logical partitions as unavailable.
                    if app.state.source.uses_usb_drive()
                        && let Err(error) = data.open_usb(app)
                    {
                        active.cancel()?;
                        skyward_sys::kmsg(&format!("install: USB ZIP confirmation cancelled: {error:#}"))?;
                        app.report(app.state.lang.pick(skyward_ui::app::INSTALL_USB_DISCONNECTED), now);
                        continue;
                    }
                    if app.state.block_ota_install {
                        data.keep_settings(app, now, true);
                        data.needs_reboot = true;
                        data.closed.clear();
                        app.data_failed("请重启 Recovery 后再解密".into());
                        app.set_mounts(Err("安装开始后请重启 Recovery 以重新检查挂载".into()));
                    }
                    if effect == Effect::InstallUnverifiedZip {
                        active.install_unverified_zip();
                    } else {
                        active.install();
                    }
                }
            }
            Effect::Cancel => {
                if let Some(active) = installer.as_ref() {
                    active.cancel()?;
                }
            }
            Effect::Ticks => {
                if let Some(motor) = haptics
                    && let Err(error) = motor.ticks()
                {
                    hardware_error(app, "振动", &error, now);
                    *haptics = None;
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
