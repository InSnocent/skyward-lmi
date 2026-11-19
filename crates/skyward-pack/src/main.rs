use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use skyward_config::ramdisk as paths;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

mod avb;
mod described;

use skyward_bootimg::{BootImage, LegacyBootImage, Ramdisk};

/// Builds Skyward recovery images from a device's own stock images.
#[derive(Parser)]
#[command(name = "skyward-pack")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print an image's header and the files in its ramdisk.
    List { image: PathBuf },
    /// Write one ramdisk file to stdout.
    Cat { image: PathBuf, path: String },
    /// Unpack and pack an image's ramdisk unchanged, to check that a rebuilt
    /// archive keeps every entry.
    Repack {
        image: PathBuf,
        out: PathBuf,
        /// Add a new unsigned recovery AVB footer at the original partition size.
        #[arg(long)]
        recovery_avb: bool,
    },
    /// Make a temporary v2/gzip recovery image that tests display and touch.
    /// Keeps the base partition size and adds a fresh unsigned AVB footer.
    DisplayProbe {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Make a temporary image for testing the full UI on an A-only device.
    PreviewUi {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        assets: PreviewAssets,
    },
    /// Join a kernel and another image's ramdisk into one boot image.
    Boot {
        /// Image holding the kernel (the device's boot partition).
        #[arg(long)]
        kernel: PathBuf,
        /// Image holding the ramdisk.
        #[arg(long)]
        ramdisk: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a Skyward recovery image on top of the device's stock recovery:
    /// its init, adbd, SELinux policy and libraries stay, the recovery
    /// service becomes Skyward. The image fills the whole partition and ends
    /// in an unsigned AVB footer of its own.
    Recovery {
        /// The device's stock recovery image: the whole partition, as the
        /// phone or a full update package holds it, ending in its AVB footer.
        #[arg(long)]
        stock: PathBuf,
        /// Skyward built for the device (aarch64-linux-android).
        #[arg(long)]
        binary: PathBuf,
        /// The first program of a system installed beside the installed
        /// one, skyward-system-init built for the device and linked
        /// statically.
        #[arg(long)]
        system_init: PathBuf,
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// The UI typeface.
        #[arg(long)]
        font: PathBuf,
        /// The terminal's monospaced typeface.
        #[arg(long)]
        mono: PathBuf,
        /// The monospaced typeface's licence, which goes with it.
        #[arg(long)]
        mono_license: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the stock build a device's recovery is made on, as lines of
    /// name=value: its version, where its full update package is, and the
    /// SHA-256 of the recovery partition in that package. Given somewhere
    /// to write it, also take that partition out of the package on its
    /// server, reading only what the partition takes, and write it there
    /// once its SHA-256 is that one.
    Stock {
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the stock recovery image.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Take what a device's own partitions say of the programs they carry
    /// out of its stock build's full update package on its server: the
    /// init scripts, the service descriptions, the accounts and the build
    /// properties of each partition that carries such programs, written
    /// under `out` as the partition and then the path in it. Only the
    /// parts of the package that hold these files are downloaded, each
    /// checked against the package's own sum of it.
    Described {
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// A partition to take them out of; vendor, odm and system_ext
        /// when none is named.
        #[arg(long = "partition")]
        partitions: Vec<String>,
        /// The folder to write the files under, which is not there yet.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(clap::Args)]
struct PreviewAssets {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    font: PathBuf,
    #[arg(long)]
    mono: PathBuf,
    #[arg(long)]
    mono_license: PathBuf,
    /// Start the A-only recovery service in read-only inspection mode.
    #[arg(long)]
    read_only: bool,
    /// Public certificate archive used only by the log-only sideload test.
    #[arg(long, requires = "read_only")]
    sideload_test_certs: Option<PathBuf>,
    /// Add the signed BLOCK OTA inspection entry to a read-only image.
    #[arg(long, requires = "read_only", conflicts_with = "sideload_test_certs")]
    block_ota_preflight: bool,
    /// Allow installation of this exact signed A-only OTA by SHA-256.
    #[arg(long, requires = "read_only", conflicts_with = "sideload_test_certs")]
    block_ota_install: Option<String>,
    /// Enable A-only data decryption with the matching ROM libraries.
    #[arg(long, requires = "read_only", conflicts_with = "sideload_test_certs")]
    data_readonly: bool,
    /// Directory containing the Runtime and ICU APEX files extracted from the lmi ROM.
    #[arg(long, requires = "data_readonly")]
    lmi_apex_files: Option<PathBuf>,
    /// Expected contents of the synthetic lmi backup sample.
    #[arg(long, requires = "data_readonly", conflicts_with = "block_ota_install")]
    lmi_data_sample: Option<PathBuf>,
    /// Exercise backup and restore of the local lmi test app.
    #[arg(long, requires = "data_readonly", conflicts_with_all = ["lmi_data_sample", "block_ota_install"])]
    lmi_app_restore_test: bool,
    /// Enable the lmi Data backup and restore pages with the matching ROM libraries.
    #[arg(long, requires = "data_readonly", conflicts_with_all = ["lmi_data_sample", "lmi_app_restore_test"])]
    lmi_data_backup: bool,
    /// Directory with the AW8697 RAM and RTP firmware extracted from the ROM.
    #[arg(long)]
    aw8697_firmware: Option<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::List { image } => list(&image),
        Cmd::Cat { image, path } => {
            let rd = Ramdisk::decode(PackImage::read(&image)?.ramdisk())?;
            let e = rd.find(&path).with_context(|| format!("{path} is not in the ramdisk"))?;
            std::io::stdout().write_all(&e.data)?;
            Ok(())
        }
        Cmd::Repack { image, out, recovery_avb } => {
            let size = recovery_avb.then(|| partition_size(&image)).transpose()?;
            let mut img = PackImage::read(&image)?;
            *img.ramdisk_mut() = Ramdisk::repack(img.ramdisk())?;
            let mut bytes = img.to_bytes()?;
            if let Some(size) = size {
                bytes = avb::footed(&bytes, RECOVERY, size, &Sha256::digest(&bytes))?;
            }
            write(&out, img.kernel_len(), img.ramdisk().len(), &bytes)
        }
        Cmd::DisplayProbe { base, binary, out } => build_probe(&base, &binary, &out, None),
        Cmd::PreviewUi { base, binary, out, assets } => build_probe(&base, &binary, &out, Some(&assets)),
        Cmd::Boot { kernel, ramdisk, out } => {
            let mut img = BootImage::read(&ramdisk)?;
            img.kernel = BootImage::read(&kernel)?.kernel;
            write(&out, img.kernel.len(), img.ramdisk.len(), &img.to_bytes()?)
        }
        Cmd::Recovery { stock, binary, system_init, config, font, mono, mono_license, out } => {
            let size = partition_size(&stock)?;
            let mut img = BootImage::read(&stock)?;
            let mut rd = Ramdisk::decode(&img.ramdisk)?;
            // A stock recovery ramdisk has no third-party tree; refuse to build
            // on top of another custom recovery.
            if let Some(e) = rd.entries.iter().find(|e| e.name == "FFiles" || e.name.starts_with("twres")) {
                anyhow::bail!("{} holds {}, which is not a stock recovery", stock.display(), e.name);
            }
            let read = |p: &PathBuf| std::fs::read(p).with_context(|| format!("read {}", p.display()));
            skyward_config::DeviceConfig::load(&config)?;
            rd.replace(paths::RECOVERY, read(&binary)?)?;
            // Install scripts look for the commands a stock recovery lacks
            // among its programs, and its unzip there too, a link to ziptool
            // until it points here; the recovery binary is each of them.
            let (_, recovery) = paths::RECOVERY.rsplit_once('/').expect("the recovery binary is in a folder");
            for command in paths::COMMANDS {
                rd.link(command, recovery)?;
            }
            rd.add(paths::SYSTEM_INIT, 0o755, read(&system_init)?)?;
            rd.add(paths::CONFIG, 0o644, read(&config)?)?;
            rd.add(paths::FONT, 0o644, read(&font)?)?;
            rd.add(paths::MONO, 0o644, read(&mono)?)?;
            rd.add(paths::MONO_LICENSE, 0o644, read(&mono_license)?)?;
            // A stock ramdisk may run logd without any scheduling profiles,
            // and logd stops as soon as it cannot find the one it asks for.
            if rd.find(paths::TASK_PROFILES).is_none() {
                rd.add(paths::TASK_PROFILES, 0o644, TASK_PROFILES.to_vec())?;
            }
            rd.add(paths::MTP_RC, 0o644, MTP_RC.to_vec())?;
            // On an unlocked device adbd keeps root when ro.secure is 0 and
            // lets any computer in without authorization when ro.adb.secure is
            // 0; init lets a later line of a property file override an earlier
            // one. update_engine refuses a package built before the recovery
            // itself unless both of the last two are set; the recovery checks
            // packages against the installed system instead.
            let stock_props = &rd
                .find(paths::PROP_DEFAULT)
                .with_context(|| format!("{} is not in the ramdisk", paths::PROP_DEFAULT))?
                .data;
            let mut props = without(stock_props, &BUILD_VERSION);
            props.extend_from_slice(b"\nro.secure=0\nro.adb.secure=0\nro.debuggable=1\nro.ota.allow_downgrade=true\n");
            rd.replace(paths::PROP_DEFAULT, props)?;
            img.ramdisk = rd.encode()?;
            let bytes = img.to_bytes()?;
            write(&out, img.kernel.len(), img.ramdisk.len(), &avb::footed(&bytes, RECOVERY, size, &Sha256::digest(&bytes))?)
        }
        Cmd::Stock { config, out } => {
            let stock = skyward_config::DeviceConfig::load(&config)?.stock.with_context(|| format!("{} names no stock build", config.display()))?;
            println!("version={}\npackage={}\nrecovery={}", stock.version, stock.package, stock.recovery);
            match out {
                Some(out) => take_stock_recovery(&stock, &out),
                None => Ok(()),
            }
        }
        Cmd::Described { config, partitions, out } => {
            let stock = skyward_config::DeviceConfig::load(&config)?.stock.with_context(|| format!("{} names no stock build", config.display()))?;
            let partitions = if partitions.is_empty() { skyward_programs::phone::PARTITIONS.map(str::to_string).to_vec() } else { partitions };
            described::take(&stock.package, &partitions, &out)
        }
    }
}

/// Keep the base's init and policy. The explicit init arguments let the
/// binary enter the probe without running any of the regular recovery tasks.
fn build_probe(base: &Path, binary: &Path, out: &Path, assets: Option<&PreviewAssets>) -> Result<()> {
    let size = partition_size(base)?;
    const INIT: &str = "system/etc/init/hw/init.rc";
    let image = std::fs::read(base).with_context(|| format!("read {}", base.display()))?;
    let mut img = LegacyBootImage::parse(&image)?;
    ensure!(img.ramdisk.starts_with(&[0x1f, 0x8b]), "the probe needs a gzip ramdisk");
    let mut ramdisk = Ramdisk::decode(&img.ramdisk)?;
    ensure!(
        ramdisk.entries.iter().all(|e| e.is_dir() || e.nlink == 1),
        "the probe needs a ramdisk without hard-linked entries"
    );
    let recovery = std::fs::read(binary).with_context(|| format!("read {}", binary.display()))?;
    let command = if let Some(assets) = assets {
        let cfg = skyward_config::DeviceConfig::load(&assets.config)?;
        ensure!(!cfg.device.ab && cfg.display.density > 0, "an A-only device with a display is required");
        for (entry, path) in [
            (paths::CONFIG, &assets.config),
            (paths::FONT, &assets.font),
            (paths::MONO, &assets.mono),
            (paths::MONO_LICENSE, &assets.mono_license),
        ] {
            let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            ramdisk.add(entry, 0o644, data)?;
        }
        if cfg.device.codename == "lmi" {
            // The lmi base does not set sys.usb.config_disable. Its normal
            // adb rule starts adbd whenever the requested function needs it.
            let rules = std::str::from_utf8(MTP_RC)?
                .replace(" && property:sys.usb.config_disable=0", "");
            ramdisk.add(paths::MTP_RC, 0o644, rules.into_bytes())?;
        }
        if cfg.haptics.as_ref().is_some_and(|h| h.backend == skyward_config::HapticsBackend::Aw8697) {
            let firmware = assets.aw8697_firmware.as_deref().context("AW8697 requires --aw8697-firmware")?;
            for name in ["aw8697_haptic.bin", "aw8697_rtp_1.bin"] {
                let path = firmware.join(name);
                let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                ensure!(!data.is_empty(), "{} is empty", path.display());
                ramdisk.add(&format!("vendor/firmware/{name}"), 0o644, data)?;
            }
        }
        if let Some(certs) = &assets.sideload_test_certs {
            let data = std::fs::read(certs).with_context(|| format!("read {}", certs.display()))?;
            ensure!(!data.is_empty(), "the sideload test certificate archive is empty");
            ramdisk.add("skyward/sideload-test-otacerts.zip", 0o644, data)?;
        }
        if assets.block_ota_preflight {
            ensure!(ramdisk.find("system/etc/security/otacerts.zip").is_some(), "the base recovery has no OTA certificates");
            ramdisk.add("skyward/block-ota-preflight", 0o644, Vec::new())?;
        }
        if let Some(sha256) = &assets.block_ota_install {
            ensure!(ramdisk.find("system/etc/security/otacerts.zip").is_some(), "the base recovery has no OTA certificates");
            ramdisk.add(paths::BLOCK_OTA_DIGEST, 0o644, parse_sha256(sha256)?.to_vec())?;
        }
        if assets.data_readonly {
            ensure!(cfg.device.codename == "lmi", "the legacy decryption test is configured for lmi");
            let apex = assets.lmi_apex_files.as_deref().context("the lmi decryption test requires --lmi-apex-files")?;
            add_lmi_apex(&mut ramdisk, apex)?;
            // The Recovery service manager caches its manifest at boot.
            ramdisk.add(
                "system/etc/vintf/manifest/android.system.keystore2-service.xml",
                0o644,
                LMI_KEYSTORE_MANIFEST.to_vec(),
            )?;
            ramdisk.add(paths::DATA_READONLY, 0o644, Vec::new())?;
            if assets.lmi_app_restore_test {
                ramdisk.add(paths::LMI_APP_RESTORE_TEST, 0o644, Vec::new())?;
            }
            if assets.lmi_data_backup {
                ramdisk.add(paths::LMI_DATA_BACKUP, 0o644, Vec::new())?;
            }
            if let Some(sample) = &assets.lmi_data_sample {
                let expected = std::fs::read(sample)?;
                ensure!((1..=4096).contains(&expected.len()), "the sample must be between 1 and 4096 bytes");
                ramdisk.add(paths::DATA_SAMPLE, 0o644, expected)?;
            }
        }
        if assets.read_only { "" } else { "preview-ui" }
    } else {
        "display-probe --seconds 60"
    };
    let init = ramdisk.find(INIT).context("missing recovery init configuration")?;
    let patched = probe_init(std::str::from_utf8(&init.data).context("read recovery init configuration as UTF-8")?, command)?;
    ramdisk.replace(INIT, patched.into_bytes())?;
    ramdisk.replace(paths::RECOVERY, recovery)?;
    img.ramdisk = ramdisk.encode_gzip()?;
    let payload = img.to_bytes()?;
    let bytes = avb::footed(&payload, RECOVERY, size, &Sha256::digest(&payload))?;
    write(out, img.kernel.len(), img.ramdisk.len(), &bytes)
}

// Infinity-X Android 16's Keystore2 AIDL service declaration.
const LMI_KEYSTORE_MANIFEST: &[u8] = br#"<manifest version="9.0" type="framework">
    <hal format="aidl">
        <name>android.system.keystore2</name>
        <version>5</version>
        <fqname>IKeystoreService/default</fqname>
    </hal>
</manifest>
"#;

/// Pack the needed 64-bit files from the matching ROM's verified APEX inputs.
fn add_lmi_apex(ramdisk: &mut Ramdisk, source: &Path) -> Result<()> {
    for (file, mode) in [
        ("com.android.runtime/bin/linker64", 0o755),
        ("com.android.runtime/lib64/bionic/libc.so", 0o644),
        ("com.android.runtime/lib64/bionic/libdl.so", 0o644),
        ("com.android.runtime/lib64/bionic/libdl_android.so", 0o644),
        ("com.android.runtime/lib64/bionic/libm.so", 0o644),
        ("com.android.i18n/lib64/libandroidicu.so", 0o644),
        ("com.android.i18n/lib64/libicuuc.so", 0o644),
        ("com.android.i18n/lib64/libicui18n.so", 0o644),
        ("com.android.i18n/etc/icu/icudt76l.dat", 0o644),
    ] {
        let path = source.join(file);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        ensure!(!bytes.is_empty(), "{} is empty", path.display());
        ramdisk.add(&format!("skyward/apex/{file}"), mode, bytes)?;
    }
    Ok(())
}

fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    ensure!(value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()), "expected 64 hexadecimal SHA-256 digits");
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16)?;
    }
    Ok(digest)
}

fn probe_init(init: &str, command: &str) -> Result<String> {
    const SERVICE: &str = "service recovery /system/bin/recovery";
    ensure!(init.lines().filter(|line| *line == SERVICE).count() == 1, "expected one recovery service in init");
    let invocation = if command.is_empty() { SERVICE.to_owned() } else { format!("{SERVICE} {command}") };
    let probe = format!("{invocation}\n    oneshot");
    Ok(init.replacen(SERVICE, &probe, 1))
}

/// The partition a recovery is made on, as a full update package names it.
const RECOVERY: &str = "recovery";

/// Take the recovery partition out of the stock build's full update package
/// on its server, reading only what that partition takes of it, and write it
/// to `out` once its SHA-256 is the one the configuration names.
fn take_stock_recovery(stock: &skyward_config::Stock, out: &Path) -> Result<()> {
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("make {}", parent.display()))?;
    // Taken out beside where it goes, so it is moved there whole.
    let work = tempfile::Builder::new().prefix(".stock").tempdir_in(parent).with_context(|| format!("make a folder in {}", parent.display()))?;
    let payload = unpayload::Payload::open_url(&stock.package).with_context(|| format!("open {}", stock.package))?;
    let options = unpayload::ExtractOptions { output_dir: work.path().to_path_buf(), partitions: vec![RECOVERY.into()], ..Default::default() };
    let report = |event: unpayload::Event<'_>| match event {
        unpayload::Event::Started { partitions } => {
            for (name, bytes) in partitions {
                println!("taking {name} out of the package: {bytes} bytes");
            }
        }
        unpayload::Event::Extracted(image) => println!("took {} out: {} bytes", image.partition, image.size),
        unpayload::Event::Progress { .. } => {}
    };
    let taken = payload.extract(&options, &report).with_context(|| format!("take {RECOVERY} out of {}", stock.package))?;
    let image = taken.iter().find(|i| i.partition == RECOVERY).with_context(|| format!("{} holds no {RECOVERY} partition", stock.package))?;
    let sum = hex::encode(image.sha256.as_deref().context("the package's own sum of the image was not checked")?);
    ensure!(sum == stock.recovery, "the {RECOVERY} partition in {} has the SHA-256 {sum}, where the configuration names {}", stock.package, stock.recovery);
    std::fs::rename(&image.path, out).with_context(|| format!("move {} to {}", image.path.display(), out.display()))?;
    println!("wrote {} ({} bytes), its SHA-256 the one the configuration names", out.display(), image.size);
    Ok(())
}

/// The size of the partition `stock` fills: all of it, as the phone and a
/// full update package hold it, which its AVB footer ends.
fn partition_size(stock: &Path) -> Result<usize> {
    let mut file = File::open(stock).with_context(|| format!("open {}", stock.display()))?;
    let size = file.metadata().with_context(|| format!("stat {}", stock.display()))?.len();
    let mut magic = [0u8; 4];
    if size >= 64 {
        file.seek(SeekFrom::Start(size - 64)).with_context(|| format!("seek in {}", stock.display()))?;
        file.read_exact(&mut magic).with_context(|| format!("read {}", stock.display()))?;
    }
    ensure!(
        &magic == b"AVBf",
        "{} does not end in an AVB footer; give the whole recovery partition, as the phone or a full update package holds it",
        stock.display()
    );
    Ok(usize::try_from(size)?)
}

/// What a recovery image says about the build it is: the version and patch
/// levels of its own ramdisk's system and vendor partition, which are older
/// than those installed beside it whenever the phone has taken an update.
/// The phone's secure services read these to tell the secure world which
/// build is running, and keys the installed system made are refused when
/// they say the wrong one, so they are left out here for the recovery to
/// fill in from the installed system once it has that mounted.
const BUILD_VERSION: [&str; 3] = ["ro.build.version.release", "ro.build.version.security_patch", "ro.vendor.build.security_patch"];

/// The scheduling profiles a recovery's programs look up by name. A
/// recovery's cgroup setup mounts no controllers, so of all a system's
/// profiles do, only the timer slack applies here.
const TASK_PROFILES: &[u8] = include_bytes!("task_profiles.json");

/// The init rules that give the USB port file transfer beside adb, which a
/// stock recovery does not offer.
const MTP_RC: &[u8] = include_bytes!("mtp.rc");

/// A property file without the lines that set any of `names`.
fn without(props: &[u8], names: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(props.len());
    for line in props.split_inclusive(|b| *b == b'\n') {
        let key = line.split(|b| *b == b'=').next().unwrap_or_default();
        if !names.iter().any(|n| n.as_bytes() == key) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// The image layouts supported by the host's inspection and repack commands.
enum PackImage {
    Legacy(LegacyBootImage),
    Modern(BootImage),
}

impl PackImage {
    fn read(path: &Path) -> Result<Self> {
        let read = || {
            let bytes = std::fs::read(path)?;
            // All standard headers store the little-endian version at byte 40.
            if bytes.get(40..44) == Some(&2u32.to_le_bytes()) {
                LegacyBootImage::parse(&bytes).map(Self::Legacy)
            } else {
                BootImage::parse(&bytes).map(Self::Modern)
            }
        };
        read().with_context(|| format!("read {}", path.display()))
    }

    fn ramdisk(&self) -> &[u8] {
        match self {
            Self::Legacy(img) => &img.ramdisk,
            Self::Modern(img) => &img.ramdisk,
        }
    }

    fn ramdisk_mut(&mut self) -> &mut Vec<u8> {
        match self {
            Self::Legacy(img) => &mut img.ramdisk,
            Self::Modern(img) => &mut img.ramdisk,
        }
    }

    fn kernel_len(&self) -> usize {
        match self {
            Self::Legacy(img) => img.kernel.len(),
            Self::Modern(img) => img.kernel.len(),
        }
    }

    fn to_bytes(&self) -> Result<Vec<u8>> {
        match self {
            Self::Legacy(img) => img.to_bytes(),
            Self::Modern(img) => img.to_bytes(),
        }
    }
}

fn write(out: &Path, kernel_len: usize, ramdisk_len: usize, bytes: &[u8]) -> Result<()> {
    std::fs::write(out, bytes).with_context(|| format!("write {}", out.display()))?;
    println!(
        "wrote {} ({kernel_len} bytes kernel, {ramdisk_len} bytes ramdisk, {} bytes in all)",
        out.display(),
        bytes.len()
    );
    Ok(())
}

fn list(image: &Path) -> Result<()> {
    let img = PackImage::read(image)?;
    let (version, os) = match &img {
        PackImage::Legacy(img) => (img.header.header_version(), &img.header.osversionpatch),
        PackImage::Modern(img) => (img.header.header_version(), &img.header.osversionpatch),
    };
    println!("header v{version}  kernel {} bytes  ramdisk {} bytes  {os:?}", img.kernel_len(), img.ramdisk().len());
    if img.ramdisk().is_empty() {
        return Ok(());
    }
    let rd = Ramdisk::decode(img.ramdisk())?;
    for e in &rd.entries {
        println!("{:06o} {:>5}:{:<5} {:>10}  {}", e.mode, e.uid, e.gid, e.data.len(), e.name);
    }
    println!("{} entries", rd.entries.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_preview_starts_once_with_an_explicit_init_command() {
        let init = "service recovery /system/bin/recovery\n    user root\n";
        assert_eq!(
            probe_init(init, "preview-ui").unwrap(),
            "service recovery /system/bin/recovery preview-ui\n    oneshot\n    user root\n"
        );
        assert!(probe_init(&init.repeat(2), "preview-ui").is_err());
    }

    #[test]
    fn the_lines_that_set_a_name_are_the_only_ones_left_out() {
        let props = b"ro.build.id=TQ\nro.build.version.release=13\nro.build.version.release_or_codename=13\nro.product.name=ishtar\n\
ro.vendor.build.security_patch=2026-10-01\nro.vendor.build.security_patch_level=1\n";
        let out = without(props, &BUILD_VERSION);
        let text = String::from_utf8(out).expect("property files are text");
        assert!(!text.contains("ro.build.version.release=13"));
        assert!(!text.contains("ro.vendor.build.security_patch=2026-10-01"), "the vendor partition's patch level is left out too");
        assert!(text.contains("ro.build.version.release_or_codename=13"), "a longer name is left alone");
        assert!(text.contains("ro.vendor.build.security_patch_level=1"), "a longer name is left alone");
        assert!(text.contains("ro.build.id=TQ") && text.contains("ro.product.name=ishtar"));
    }

    #[test]
    fn the_profiles_name_all_the_recovery_asks_for() {
        let profiles: serde_json::Value = serde_json::from_slice(TASK_PROFILES).expect("the profiles are JSON");
        let names = |key: &str| -> Vec<String> {
            profiles[key].as_array().expect("a list").iter().map(|p| p["Name"].as_str().expect("a name").to_string()).collect()
        };
        let (plain, aggregate) = (names("Profiles"), names("AggregateProfiles"));
        // What libprocessgroup asks for when a program sets its scheduling
        // policy, and what the recovery's own init scripts give logd.
        let asked = [
            "SCHED_SP_DEFAULT",
            "SCHED_SP_BACKGROUND",
            "SCHED_SP_FOREGROUND",
            "SCHED_SP_TOP_APP",
            "SCHED_SP_SYSTEM",
            "SCHED_SP_RT_APP",
            "CPUSET_SP_DEFAULT",
            "CPUSET_SP_BACKGROUND",
            "CPUSET_SP_FOREGROUND",
            "CPUSET_SP_TOP_APP",
            "CPUSET_SP_SYSTEM",
            "CPUSET_SP_RESTRICTED",
            "BlkIOBackground",
            "BlkIOForeground",
            "ServiceCapacityLow",
        ];
        for name in asked {
            assert!(plain.iter().chain(&aggregate).any(|n| n == name), "{name} is missing");
        }
        for a in profiles["AggregateProfiles"].as_array().expect("a list") {
            for part in a["Profiles"].as_array().expect("a list") {
                let part = part.as_str().expect("a name");
                assert!(plain.iter().any(|n| n == part), "{} is made of {part}, which is not a profile", a["Name"]);
            }
        }
    }

    #[test]
    fn the_partition_is_as_large_as_a_stock_image_ending_in_its_footer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.img");
        let mut whole = vec![0u8; 8192];
        whole[8192 - 64..8192 - 60].copy_from_slice(b"AVBf");
        std::fs::write(&path, &whole).unwrap();
        assert_eq!(partition_size(&path).unwrap(), 8192);
        std::fs::write(&path, &whole[..8000]).unwrap();
        assert!(partition_size(&path).is_err(), "an image cut short of its partition's end");
        std::fs::write(&path, b"AVBf").unwrap();
        assert!(partition_size(&path).is_err(), "too short to end in a footer");
    }

    #[test]
    fn a_file_without_a_last_newline_keeps_its_last_line() {
        let out = without(b"a=1\nro.build.version.release=13", &BUILD_VERSION);
        assert_eq!(out, b"a=1\n");
        let kept = without(b"a=1\nb=2", &BUILD_VERSION);
        assert_eq!(kept, b"a=1\nb=2");
    }
}
