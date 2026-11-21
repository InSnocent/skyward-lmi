use anyhow::{ensure, Context, Result};
use skyward_block::{fstab, mount};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};
use std::path::Path;
use std::sync::Mutex;

const COMPAT: &str = "android.security.compat";

static SERVICES: Mutex<Option<crate::services::Managed>> = Mutex::new(None);

/// Start the installed system's own security services for lmi decryption.
pub fn prepare() -> Result<()> {
    let mut services = SERVICES.lock().expect("the lmi services lock is not poisoned");
    if services.is_none() {
        *services = Some(start()?);
    }
    Ok(())
}

fn stop_services() -> Result<()> {
    let mut services = SERVICES.lock().expect("the lmi services lock is not poisoned");
    if let Some(active) = services.as_mut() {
        active.stop()?;
        *services = None;
    }
    Ok(())
}

const KEYSTORE_PATH: &str = "/mnt/system/system/bin/keystore2";
const TEMP_STORE: &str = "/tmp/skyward-keystore";
const KEYSTORE_INTERFACE: &str = "android.system.keystore2.IKeystoreService";
const KEYSTORE_SERVICE: &str = "android.system.keystore2.IKeystoreService/default";
const MAINTENANCE: &str = "android.security.maintenance";
const MAINTENANCE_INTERFACE: &str = "android.security.maintenance.IKeystoreMaintenance";
const APP_DOMAIN: i32 = 0;
const GET_NUMBER_OF_ENTRIES: u32 = 8;
const CLEAR_NAMESPACE: u32 = 5;

pub(crate) fn clear_app_keys(
    store: &crate::data_wipe::KeystoreTarget<'_>,
    targets: &std::collections::BTreeSet<u32>,
    before_stopping: impl FnOnce(),
) -> Result<()> {
    use skyward_binder::{driver::Binder, parcel::Parcel, service, status};

    ensure!(!targets.is_empty(), "no app key namespaces were selected");
    check_keystore_sdk()?;
    let config = skyward_config::DeviceConfig::load("/skyward/device.toml")?;
    let mut keystore = config.services.last().cloned().context("missing lmi Keystore configuration")?;
    ensure!(config.device.codename == "lmi"
        && keystore.path == KEYSTORE_PATH && keystore.args == [TEMP_STORE]
        && keystore.user.as_deref() == Some("keystore")
        && keystore.ready_service.as_deref() == Some(KEYSTORE_SERVICE)
        && !keystore.oneshot,
        "unexpected lmi Keystore service configuration");
    keystore.args = vec![store.path().to_str().context("invalid Keystore path")?.to_owned()];
    store.check_path()?;

    let mut services = SERVICES.lock().expect("the lmi services lock is not poisoned");
    let active = services.as_mut().context("the lmi decryption services are not active")?;
    let old = active.last_pid()?;
    require_keystore_process(old, Path::new(TEMP_STORE))?;
    let binder = Binder::open()?;
    drop(checked_service(&binder, KEYSTORE_SERVICE, KEYSTORE_INTERFACE, old)?);
    drop(checked_service(&binder, MAINTENANCE, MAINTENANCE_INTERFACE, old)?);

    before_stopping();
    active.stop_last(old)?;
    let timeout = Instant::now() + Duration::from_secs(5);
    loop {
        if service::find(&binder, KEYSTORE_SERVICE)?.is_none()
            && service::find(&binder, MAINTENANCE)?.is_none()
        {
            break;
        }
        ensure!(Instant::now() < timeout, "previous Keystore registrations did not expire");
        std::thread::sleep(Duration::from_millis(100));
    }

    store.check_path()?;
    let mut native = crate::services::Managed::start(&[keystore])?;
    let pid = native.last_pid()?;
    let cleared = (|| -> Result<()> {
        require_keystore_process(pid, store.path())?;
        let main = checked_service(&binder, KEYSTORE_SERVICE, KEYSTORE_INTERFACE, pid)?;
        let maintenance = checked_service(&binder, MAINTENANCE, MAINTENANCE_INTERFACE, pid)?;

        // The main service opens SQLite lazily. Count the caller's own keys
        // to initialize it and check the actual open file before clearing.
        let mut count = Parcel::call(KEYSTORE_INTERFACE);
        count.i32(APP_DOMAIN);
        count.i64(-1);
        let reply = main.transact(GET_NUMBER_OF_ENTRIES, &count).context("initialize the inspected Keystore")?;
        let mut reply = reply.read();
        status::ok(&mut reply)?;
        ensure!(reply.i32()? >= 0, "the native Keystore returned an invalid entry count");
        store.check_service_database(pid)?;

        for &uid in targets {
            let mut request = Parcel::call(MAINTENANCE_INTERFACE);
            request.i32(APP_DOMAIN);
            request.i64(i64::from(uid));
            let reply = maintenance.transact(CLEAR_NAMESPACE, &request)
                .context("clear an Android app Keystore namespace")?;
            status::ok(&mut reply.read()).context("Android rejected app Keystore cleanup")?;
        }
        store.check_service_database(pid)
    })();
    let stopped = native.stop_last(pid).context("stop the native Keystore before reading the result");
    cleared?;
    stopped
}

pub(crate) fn check_keystore_sdk() -> Result<()> {
    let props = fs::read_to_string("/mnt/system/system/build.prop")
        .context("read the installed system SDK for Keystore")?;
    let mut found = false;
    for (name, value) in props.lines().filter_map(|line| line.split_once('=')) {
        if matches!(name.trim(), "ro.system.build.version.sdk" | "ro.build.version.sdk") {
            ensure!(value.trim() == "36", "app Keystore cleanup requires an Android 16 system");
            found = true;
        }
    }
    ensure!(found, "the installed system did not declare its Android SDK");
    Ok(())
}

fn require_keystore_process(pid: u32, directory: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let process = procfs::process::Process::new(pid.try_into()?)?;
    ensure!(process.exe()? == Path::new(KEYSTORE_PATH)
        && process.status()?.euid == crate::data_wipe::KeystoreTarget::UID,
        "unexpected native Keystore process");
    let command = fs::read(format!("/proc/{pid}/cmdline"))?;
    let command = command.strip_suffix(&[0]).context("invalid Keystore process arguments")?;
    let args: Vec<_> = command.split(|&byte| byte == 0).collect();
    ensure!(args == [KEYSTORE_PATH.as_bytes(), directory.as_os_str().as_bytes()],
        "native Keystore is using a different directory");
    Ok(())
}

fn checked_service<'a>(
    binder: &'a skyward_binder::driver::Binder,
    name: &str,
    interface: &str,
    pid: u32,
) -> Result<skyward_binder::driver::Handle<'a>> {
    use skyward_binder::service;
    // The main service can register before the maintenance endpoint.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(handle) = service::find(binder, name)? {
            ensure!(service::pid(&handle)? == pid && service::interface(&handle)? == interface,
                "unexpected Android Keystore Binder service");
            return Ok(handle);
        }
        ensure!(Instant::now() < deadline, "Android Keystore Binder service did not register");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn unmount_partition(name: &str, point: &str) -> Result<()> {
    let path = Path::new(point);
    if let Some(mounted) = mount::visible(path)? {
        let partition = Path::new(skyward_block::BY_NAME).join(name);
        let device = skyward_block::devnum(&partition)?;
        ensure!(mounted.device == device, "unexpected filesystem mounted at {point}");
        mount::umount(path)?;
    }
    Ok(())
}

/// Stop the installed services and firmware mounts while leaving data open
/// for an OTA package being read from internal storage.
pub fn stop_for_local_install() -> Result<()> {
    stop_services()?;
    unmount_partition("modem", crate::FIRMWARE_SHOWN)?;
    unmount_partition("modem", "/vendor/firmware_mnt")
}

/// Close data and unmount the dependencies of the executable system mounts.
pub fn finish() -> Result<()> {
    stop_services()?;
    skyward_crypto::data::close()?;
    unmount_partition("modem", crate::FIRMWARE_SHOWN)?;
    unmount_partition("modem", "/vendor/firmware_mnt")?;
    unmount_partition("metadata", "/metadata")
}

fn start() -> Result<crate::services::Managed> {
    let cfg = skyward_config::DeviceConfig::load("/skyward/device.toml")?;
    ensure!(cfg.device.codename == "lmi", "legacy decryption is configured for lmi");
    // The same ROM's Recovery already carries the correct OS and vendor patch levels.
    for point in ["/mnt/system", "/mnt/system_ext", "/vendor", "/odm"] {
        ensure!(mount::is_mount_point(Path::new(point))?, "{point} is not mounted");
    }
    let metadata = fstab::partition("metadata")?.context("no metadata partition in fstab")?;
    let device = fstab::device(&metadata)?;
    if !mount::is_mount_point(Path::new("/metadata"))? {
        mount::mount_ro_no_recovery(Path::new(&device), Path::new("/metadata"))?;
    }
    crate::mount_firmware("modem")?;
    skyward_sys::ueventd::apply()?;

    let key_db = Path::new(TEMP_STORE);
    fs::create_dir_all(key_db)?;
    fs::set_permissions(key_db, fs::Permissions::from_mode(0o700))?;
    std::os::unix::fs::chown(key_db, Some(1017), Some(1017))?;

    let hardware: Vec<_> = cfg.services.iter().filter_map(|s| s.ready_hardware_service.clone()).collect();
    crate::secure::prepare_legacy_hardware_directory(&hardware)?;
    let fragments = Path::new("/skyward/framework-manifest-fragments");
    fs::create_dir_all(fragments)?;
    fs::set_permissions(fragments, fs::Permissions::from_mode(0o755))?;
    // Recovery's servicemanager uses the recovery Binder wire header.
    // Use that matching libbinder without replacing the ROM's other libraries.
    let binder_libraries = Path::new("/skyward/recovery-binder");
    fs::create_dir_all(binder_libraries)?;
    fs::set_permissions(binder_libraries, fs::Permissions::from_mode(0o755))?;
    let binder_library = binder_libraries.join("libbinder.so");
    if !binder_library.exists() {
        std::os::unix::fs::symlink("/system/lib64/libbinder.so", binder_library)?;
    }
    let diagnostics = startup_logs()?;
    let started = crate::services::Managed::start(&cfg.services);
    if let Some((worker, stop)) = diagnostics {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = worker.join();
        fs::remove_file("/dev/socket/logdw")?;
    }
    let started = started.with_context(|| {
        let state = skyward_sys::prop("hwservicemanager.disabled");
        let running = crate::services::running("/mnt/system_ext/bin/hwservicemanager").is_some();
        format!("hardware directory running={running}, disabled={state:?}")
    })?;
    ensure!(crate::services::registered(COMPAT), "the Keystore compatibility service did not register");
    let binder = skyward_binder::driver::Binder::open()?;
    skyward_crypto::keymint::KeyMint::open(&binder)?.hardware()?;
    Ok(started)
}

/// In recovery without logd, keep just the hardware directory's startup
/// messages. This socket is removed before the credential UI appears.
fn startup_logs() -> Result<Option<(std::thread::JoinHandle<()>, std::sync::Arc<std::sync::atomic::AtomicBool>)>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let path = Path::new("/dev/socket/logdw");
    if path.exists() {
        return Ok(None);
    }
    let socket = UnixDatagram::bind(path).context("open the temporary hardware service log socket")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o222))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(110);
        let mut buf = [0u8; 4096];
        let mut lines = 0;
        while !finished.load(Ordering::Relaxed) && Instant::now() < deadline && lines < 60 {
            let Ok(n) = socket.recv(&mut buf) else { continue };
            // liblog's datagram starts with log id, thread id and timestamp.
            let Some(body) = buf.get(12..n) else { continue };
            let mut fields = body.split(|&byte| byte == 0);
            let Some(tag) = fields.next() else { continue };
            if ![b"hwservicemanager".as_slice(), b"VintfObject", b"libvintf", b"HidlServiceManagement", b"libhwbinder", b"keystore2", b"linker"].contains(&tag) {
                continue;
            }
            if let Some(msg) = fields.next() {
                if tag == b"keystore2" && ![b"panicked".as_slice(), b"Failed to register", b"compat service", b"compatibility service"].iter()
                    .any(|needle| msg.windows(needle.len()).any(|part| part == *needle)) {
                    continue;
                }
                let msg = String::from_utf8_lossy(msg);
                let tag = String::from_utf8_lossy(tag);
                let _ = skyward_sys::kmsg(&format!("legacy service {tag}: {}", msg.trim()));
                lines += 1;
            }
        }
    });
    Ok(Some((worker, stop)))
}
