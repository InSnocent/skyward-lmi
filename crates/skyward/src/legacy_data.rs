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

    let key_db = Path::new("/tmp/skyward-keystore");
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
