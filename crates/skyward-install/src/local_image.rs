//! Local v2 boot and recovery images for the single-slot lmi layout.

use crate::image::{self, ImageKind, Ramdisk};
use anyhow::{bail, ensure, Context, Result};
use rustix::fs::{fcntl_add_seals, memfd_create, MemfdFlags, SealFlags};
use sha2::{Digest, Sha256};
use skyward_bootimg::LegacyBootImage;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The two independent boot-image partitions on lmi are both 128 MiB.
pub const PARTITION_BYTES: u64 = 128 << 20;
const CHUNK: usize = 1 << 20;
const MAX_UNPACKED: u64 = 512 << 20;
const MAX_CONFIG: usize = 64 << 10;

/// The accepted, sealed image. The source digest describes the original
/// file; the private snapshot includes partition padding and footer placement.
pub struct Prepared {
    pub kind: ImageKind,
    pub partition: &'static str,
    pub size: u64,
    pub sha256: String,
    snapshot: File,
}

impl Prepared {
    /// Only the detected single target can be written. The caller supplies
    /// the device's trusted by-name directory, never a user-selected target.
    pub fn flash(&mut self, dir: &Path, partition: &str, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        ensure!(partition == self.partition && matches!(partition, "boot" | "recovery"), "the image does not fit the selected partition");
        let target = dir.join(partition);
        let capacity = File::open(&target).with_context(|| format!("open {partition}"))?
            .seek(SeekFrom::End(0)).with_context(|| format!("read {partition} capacity"))?;
        ensure!(capacity == PARTITION_BYTES, "unexpected {partition} partition capacity");
        image::flash(&mut self.snapshot, PARTITION_BYTES, &[target], progress)
    }
}

/// Cache a bounded source in an immutable anonymous RAM file before asking
/// for confirmation, so later edits to internal storage cannot alter it.
pub fn prepare(path: &Path, cancelled: &dyn Fn() -> bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Prepared> {
    check_cancel(cancelled)?;
    ensure!(path.is_absolute() && path.canonicalize()? == path, "the image path contains a symbolic link or is not canonical");
    let source = OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path).context("open the image")?;
    prepare_file(source, cancelled, progress)
}

/// Prepare from the exact owner-storage handle opened for the selection.
pub(crate) fn prepare_file(mut source: File, cancelled: &dyn Fn() -> bool, progress: &mut dyn FnMut(u64, u64)) -> Result<Prepared> {
    check_cancel(cancelled)?;
    let before = source.metadata()?;
    ensure!(before.is_file(), "the image is not a regular file");
    let size = before.len();
    ensure!((1660..=PARTITION_BYTES).contains(&size), "image size must fit the 128 MiB partition");
    let mut bytes = vec![0; size as usize];
    progress(0, size);
    let mut done = 0;
    for chunk in bytes.chunks_mut(CHUNK) {
        check_cancel(cancelled)?;
        source.read_exact(chunk).context("read the image")?;
        done += chunk.len() as u64;
        progress(done, size);
    }
    let after = source.metadata()?;
    ensure!(after.len() == size && after.mtime() == before.mtime() && after.mtime_nsec() == before.mtime_nsec(),
        "the image changed while it was read");
    check_cancel(cancelled)?;
    let boot = LegacyBootImage::parse(&bytes).context("read the Android v2 image")?;
    ensure!(boot.header.page_size == 4096 && !boot.kernel.is_empty(), "lmi requires a v2 image with a kernel and 4096-byte pages");
    let (recovery, configured) = inspect_ramdisk(&boot.ramdisk, cancelled)?;
    if let Some(device) = configured {
        ensure!(device == "lmi", "this Skyward image was built for {device}, not lmi");
    }
    let ramdisk = if recovery { Ramdisk::Recovery } else if boot.ramdisk.is_empty() { Ramdisk::None } else { Ramdisk::Generic };
    let kind = ImageKind::Boot { kernel: true, ramdisk };
    let partition = if recovery { "recovery" } else { "boot" };
    drop(boot);
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let footer = avb_footer(&bytes)?;
    check_cancel(cancelled)?;
    let mut snapshot = File::from(memfd_create("skyward-lmi-image", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .context("create the image RAM snapshot")?);
    snapshot.write_all(&bytes).context("cache the image")?;
    snapshot.set_len(PARTITION_BYTES).context("pad the image to its partition")?;
    if size < PARTITION_BYTES {
        if let Some(footer) = footer {
            snapshot.seek(SeekFrom::Start(size-64))?;
            snapshot.write_all(&[0; 64])?;
            snapshot.seek(SeekFrom::Start(PARTITION_BYTES-64))?;
            snapshot.write_all(&footer)?;
        }
    }
    fcntl_add_seals(&snapshot, SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)
        .context("seal the accepted image against changes")?;
    check_cancel(cancelled)?;
    Ok(Prepared { kind, partition, size, sha256, snapshot })
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<()> {
    ensure!(!cancelled(), "image preparation cancelled");
    Ok(())
}

/// Check footer bounds without treating an unlocked phone's unsigned or
/// patched image as authenticated by an AVB signing key.
fn avb_footer(bytes: &[u8]) -> Result<Option<[u8; 64]>> {
    let footer = &bytes[bytes.len()-64..];
    if !footer.starts_with(b"AVBf") { return Ok(None); }
    let u32_at = |at| u32::from_be_bytes(footer[at..at+4].try_into().unwrap());
    let u64_at = |at| u64::from_be_bytes(footer[at..at+8].try_into().unwrap());
    ensure!(u32_at(4) == 1, "unsupported AVB footer version");
    let (original, offset, length) = (u64_at(12), u64_at(20), u64_at(28));
    let end = offset.checked_add(length).context("AVB metadata end overflows")?;
    ensure!(original <= offset && length >= 256 && end <= (bytes.len()-64) as u64, "AVB footer points outside the image");
    ensure!(bytes.get(offset as usize..offset as usize+4) == Some(b"AVB0"), "AVB metadata header is missing");
    Ok(Some(footer.try_into().unwrap()))
}

struct Capped<'a, R> {
    inner: R,
    left: u64,
    cancelled: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() { return Ok(0); }
        if (self.cancelled)() { return Err(io::Error::other("image preparation cancelled")); }
        if self.left == 0 {
            let mut extra = [0];
            return match self.inner.read(&mut extra)? {
                0 => Ok(0),
                _ => Err(io::Error::other("unpacked ramdisk exceeds the limit")),
            };
        }
        let want = out.len().min(self.left as usize);
        let n = self.inner.read(&mut out[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

fn skip(from: &mut dyn Read, size: u64) -> Result<()> {
    let read = io::copy(&mut from.take(size), &mut io::sink()).context("read a ramdisk entry")?;
    ensure!(read == size, "ramdisk entry is truncated");
    Ok(())
}

/// Scan newc headers with bounded names and config data. Large binary files
/// are streamed past rather than allocated from an untrusted cpio header.
struct Packed<'a> {
    bytes: &'a [u8],
    consumed: &'a Cell<usize>,
}

impl Read for Packed<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.bytes.read(out)?;
        self.consumed.set(self.consumed.get()+n);
        Ok(n)
    }
}

/// The decoder accepts EOF while trying to read a block header. Check
/// framing first, so partial headers and missing standard end marks cannot
/// masquerade as a complete ramdisk. Only one LZ4 frame is accepted.
fn check_lz4_framing(packed: &[u8]) -> Result<()> {
    let legacy = packed.starts_with(&[0x02, 0x21, 0x4c, 0x18]);
    let (mut at, block_checks, content_check, limit) = if legacy {
        (4usize, false, false, (8 << 20) + (64 << 10))
    } else {
        ensure!(packed.len() >= 7, "truncated LZ4 frame header");
        let (flags, block) = (packed[4], packed[5]);
        ensure!(flags & 0xc0 == 0x40 && flags & 0x02 == 0 && block & 0x8f == 0, "unsupported LZ4 frame flags");
        let limit = match (block >> 4) & 7 {
            4 => 64 << 10, 5 => 256 << 10, 6 => 1 << 20, 7 => 4 << 20,
            _ => bail!("invalid LZ4 block-size descriptor"),
        };
        let at = 7 + if flags & 0x08 != 0 { 8 } else { 0 } + if flags & 0x01 != 0 { 4 } else { 0 };
        ensure!(at <= packed.len(), "truncated LZ4 frame descriptor");
        (at, flags & 0x10 != 0, flags & 0x04 != 0, limit)
    };
    loop {
        if legacy && at == packed.len() { return Ok(()); }
        let word = packed.get(at..at+4).context("missing or truncated LZ4 block header or end marker")?;
        let word = u32::from_le_bytes(word.try_into().unwrap());
        at += 4;
        if !legacy && word == 0 {
            let end = at + if content_check { 4 } else { 0 };
            ensure!(end == packed.len(), "truncated LZ4 content checksum or unsupported additional frame");
            return Ok(());
        }
        let len = if legacy { word } else { word & 0x7fff_ffff } as usize;
        ensure!(len > 0 && len <= limit, "invalid LZ4 block length");
        at = at.checked_add(len).context("LZ4 block end overflows")?;
        ensure!(at <= packed.len(), "truncated LZ4 block");
        if block_checks {
            at = at.checked_add(4).context("LZ4 checksum end overflows")?;
            ensure!(at <= packed.len(), "truncated LZ4 block checksum");
        }
    }
}

fn cpio_path(name: &[u8]) -> Result<Vec<u8>> {
    ensure!(!name.starts_with(b"/"), "absolute ramdisk entry paths are unsupported");
    let mut canonical = Vec::new();
    for component in name.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." { continue; }
        ensure!(component != b"..", "parent traversal in a ramdisk entry");
        if !canonical.is_empty() { canonical.push(b'/'); }
        canonical.extend_from_slice(component);
    }
    Ok(canonical)
}

fn inspect_ramdisk(packed: &[u8], cancelled: &dyn Fn() -> bool) -> Result<(bool, Option<String>)> {
    if packed.is_empty() { return Ok((false, None)); }
    let consumed = Cell::new(0);
    let input = Packed { bytes: packed, consumed: &consumed };
    let source: Box<dyn Read + '_> = if packed.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(input))
    } else if packed.starts_with(&[0x02, 0x21, 0x4c, 0x18]) || packed.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        check_lz4_framing(packed)?;
        Box::new(lz4_flex::frame::FrameDecoder::new(input))
    } else if packed.starts_with(b"070701") || packed.starts_with(b"070702") {
        Box::new(input)
    } else {
        bail!("unsupported ramdisk compression");
    };
    let mut source = BufReader::with_capacity(64 << 10, Capped { inner: source, left: MAX_UNPACKED, cancelled });
    let mut recovery = false;
    let mut configured = None;
    let mut boundary = true;
    let mut saw_trailer = false;
    let mut critical_paths = BTreeSet::new();
    for _ in 0..100_000 {
        let mut head = [0; 110];
        if boundary {
            // Reading to decoded EOF also validates gzip/LZ4 footer checks.
            // Between newc archives only NUL padding is skipped.
            loop {
                let available = source.fill_buf().context("finish the compressed ramdisk")?;
                if available.is_empty() {
                    ensure!(saw_trailer, "ramdisk has no complete cpio archive");
                    ensure!(consumed.get() == packed.len(), "ramdisk has unread compressed data or unsupported additional frames");
                    return Ok((recovery, configured));
                }
                let zeroes = available.iter().position(|byte| *byte != 0).unwrap_or(available.len());
                if zeroes == available.len() {
                    source.consume(zeroes);
                } else {
                    head[0] = available[zeroes];
                    source.consume(zeroes+1);
                    break;
                }
            }
            source.read_exact(&mut head[1..]).context("read a ramdisk header")?;
        } else {
            source.read_exact(&mut head).context("read a ramdisk header before its trailer")?;
        }
        ensure!(head.starts_with(b"070701"), "unsupported cpio format (only newc 070701 is accepted)");
        ensure!(head[6..].iter().all(|byte| byte.is_ascii_hexdigit()), "invalid cpio header fields");
        let hex = |at| -> Result<usize> {
            Ok(u32::from_str_radix(std::str::from_utf8(&head[at..at+8])?, 16)? as usize)
        };
        let (size, namesize) = (hex(54)?, hex(94)?);
        ensure!((1..=4096).contains(&namesize), "ramdisk entry name exceeds the limit");
        let mut name = vec![0; namesize];
        source.read_exact(&mut name)?;
        ensure!(name.pop() == Some(0) && !name.contains(&0), "invalid ramdisk entry name");
        skip(&mut source, ((4-(110+namesize)%4)%4) as u64)?;
        let name = cpio_path(&name)?;
        if name == b"TRAILER!!!" {
            ensure!(size == 0, "invalid ramdisk trailer");
            boundary = true;
            saw_trailer = true;
            continue;
        }
        boundary = false;
        let is_recovery = name == b"system/bin/recovery" || name == b"sbin/recovery";
        let is_config = name == skyward_config::ramdisk::CONFIG.as_bytes();
        if is_recovery || is_config {
            ensure!(critical_paths.insert(name.clone()), "duplicate critical ramdisk path");
        }
        recovery |= is_recovery;
        if is_config {
            ensure!(size <= MAX_CONFIG, "oversized Skyward device configuration");
            let mut text = vec![0; size];
            source.read_exact(&mut text)?;
            let device = skyward_config::codename(std::str::from_utf8(&text)?)?;
            if let Some(previous) = &configured {
                ensure!(previous == &device, "conflicting Skyward device configurations");
            }
            configured = Some(device);
            recovery = true;
        } else {
            skip(&mut source, size as u64)?;
        }
        skip(&mut source, ((4-size%4)%4) as u64)?;
    }
    bail!("ramdisk contains too many entries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::symlink;

    fn archive(recovery: bool, config: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        let name = if recovery { "sbin/recovery" } else { "init" };
        let writer = cpio::NewcBuilder::new(name).mode(0o100755).write(&mut out, 0);
        writer.finish().unwrap();
        if let Some(config) = config {
            let text = format!("[device]\ncodename = \"{config}\"\n");
            let mut writer = cpio::NewcBuilder::new(skyward_config::ramdisk::CONFIG)
                .mode(0o100644).write(&mut out, text.len() as u32);
            writer.write_all(text.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        cpio::newc::trailer(&mut out).unwrap();
        out
    }

    fn v2(recovery: bool, config: Option<&str>, compression: u8) -> Vec<u8> {
        let raw = archive(recovery, config);
        let packed = match compression {
            0 => raw,
            1 => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(&raw).unwrap();
                encoder.finish().unwrap()
            }
            2 => {
                let compressed = lz4_flex::block::compress(&raw);
                let mut bytes = vec![0x02, 0x21, 0x4c, 0x18];
                bytes.extend((compressed.len() as u32).to_le_bytes());
                bytes.extend(compressed);
                bytes
            }
            _ => unreachable!(),
        };
        with_packed(&packed)
    }

    fn with_packed(packed: &[u8]) -> Vec<u8> {
        let mut out = vec![0; 4096];
        out[..8].copy_from_slice(b"ANDROID!");
        for (offset, value) in [(8, 64u32), (16, packed.len() as u32), (36, 4096), (40, 2), (1644, 1660), (1648, 4)] {
            out[offset..offset+4].copy_from_slice(&value.to_le_bytes());
        }
        out.resize(8192, 0x4b);
        out.extend_from_slice(packed);
        out.resize(out.len().next_multiple_of(4096), 0);
        out.extend(b"dtb!");
        out.resize(out.len().next_multiple_of(4096), 0);
        out
    }

    fn source(root: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = root.join("test.img");
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn v2_boot_and_recovery_map_to_one_correct_partition() {
        for recovery in [false, true] {
            for compression in 0..3 {
                let root = tempfile::tempdir().unwrap();
                let bytes = v2(recovery, None, compression);
                let mut progress = Vec::new();
                let prepared = prepare(&source(root.path(), &bytes), &|| false, &mut |n, of| progress.push((n, of))).unwrap();
                assert_eq!(prepared.partition, if recovery { "recovery" } else { "boot" });
                assert_eq!(prepared.size, bytes.len() as u64);
                assert_eq!(prepared.sha256, format!("{:x}", sha2::Sha256::digest(&bytes)));
                assert_eq!(progress.last(), Some(&(bytes.len() as u64, bytes.len() as u64)));
            }
        }
    }

    fn gz(raw: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn damaged_or_truncated_gzip_footers_are_rejected() {
        for truncated in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut bytes = v2(false, None, 1);
            let size = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
            if truncated {
                bytes[16..20].copy_from_slice(&((size-4) as u32).to_le_bytes());
            } else {
                bytes[8192+size-8] ^= 1;
            }
            assert!(prepare(&source(root.path(), &bytes), &|| false, &mut |_, _| {}).is_err());
        }
    }

    #[test]
    fn recovery_and_device_markers_in_later_cpio_archives_are_checked() {
        let first = archive(false, None);
        let last = archive(true, Some("lmi"));
        let mut raw = first.clone();
        raw.extend([0; 512]);
        raw.extend_from_slice(&last);
        for packed in [raw.clone(), gz(&raw), [gz(&first), gz(&last)].concat()] {
            let root = tempfile::tempdir().unwrap();
            let prepared = prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).unwrap();
            assert_eq!(prepared.partition, "recovery");
        }
    }

    #[test]
    fn a_foreign_skyward_config_in_a_later_archive_cannot_pass_as_boot() {
        let mut raw = archive(false, None);
        raw.extend(archive(true, Some("nezha")));
        let root = tempfile::tempdir().unwrap();
        assert!(prepare(&source(root.path(), &with_packed(&gz(&raw))), &|| false, &mut |_, _| {}).is_err());
    }

    #[test]
    fn crc_cpio_is_not_accepted_without_implementing_its_checksum() {
        let mut raw = archive(false, None);
        raw[..6].copy_from_slice(b"070702");
        raw[102..110].copy_from_slice(b"00000001");
        assert!(inspect_ramdisk(&raw, &|| false).is_err());
    }

    #[test]
    fn an_additional_lz4_frame_cannot_hide_later_recovery_markers() {
        let encode = |bytes: &[u8]| {
            let mut frame = lz4_flex::frame::FrameEncoder::new(Vec::new());
            frame.write_all(bytes).unwrap();
            frame.finish().unwrap()
        };
        let packed = [encode(&archive(false, None)), encode(&archive(true, Some("lmi")))].concat();
        let root = tempfile::tempdir().unwrap();
        assert!(prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).is_err());
    }

    fn named_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            let mut writer = cpio::NewcBuilder::new(name).mode(0o100755).write(&mut out, data.len() as u32);
            writer.write_all(data).unwrap();
            writer.finish().unwrap();
        }
        cpio::newc::trailer(&mut out).unwrap();
        out
    }

    #[test]
    fn standard_lz4_requires_complete_end_marker_and_content_checksum() {
        for checksum in [false, true] {
            let info = lz4_flex::frame::FrameInfo::default().content_checksum(checksum);
            let mut frame = lz4_flex::frame::FrameEncoder::with_frame_info(info, Vec::new());
            frame.write_all(&archive(false, None)).unwrap();
            let frame = frame.finish().unwrap();
            let root = tempfile::tempdir().unwrap();
            let path = source(root.path(), &with_packed(&frame));
            assert!(prepare(&path, &|| false, &mut |_, _| {}).is_ok());
            for missing in 1..=if checksum { 8 } else { 4 } {
                fs::write(&path, with_packed(&frame[..frame.len()-missing])).unwrap();
                assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err(),
                        "truncated LZ4 footer accepted ({checksum}, {missing})");
            }
            if checksum {
                let mut changed = frame;
                *changed.last_mut().unwrap() ^= 1;
                fs::write(&path, with_packed(&changed)).unwrap();
                assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
            }
        }
    }

    #[test]
    fn legacy_lz4_rejects_partial_next_block_headers() {
        let bytes = v2(false, None, 2);
        let size = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let original = &bytes[8192..8192+size];
        for extra in 1..=3 {
            let root = tempfile::tempdir().unwrap();
            let mut packed = original.to_vec();
            packed.extend(vec![0; extra]);
            assert!(prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).is_err());
        }
    }

    #[test]
    fn cpio_path_aliases_cannot_hide_recovery_or_foreign_configuration() {
        for name in ["system/./bin/recovery", "system//bin/recovery", "./sbin/./recovery"] {
            let root = tempfile::tempdir().unwrap();
            let packed = named_archive(&[(name, b"")]);
            assert_eq!(prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).unwrap().partition,
                       "recovery", "{name}");
        }
        let alias = skyward_config::ramdisk::CONFIG.replace('/', "/./");
        let packed = named_archive(&[(&alias, b"[device]\ncodename=\"nezha\"\n")]);
        let root = tempfile::tempdir().unwrap();
        assert!(prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).is_err());
    }

    #[test]
    fn ambiguous_cpio_paths_and_duplicate_critical_paths_are_refused() {
        for names in [
            vec!["/sbin/recovery"], vec!["sbin/../init"],
            vec!["sbin/recovery", "sbin/./recovery"],
        ] {
            let root = tempfile::tempdir().unwrap();
            let entries: Vec<_> = names.iter().map(|name| (*name, &b""[..])).collect();
            let packed = named_archive(&entries);
            assert!(prepare(&source(root.path(), &with_packed(&packed)), &|| false, &mut |_, _| {}).is_err());
        }
    }

    #[test]
    fn skyward_device_configuration_must_match_lmi() {
        let root = tempfile::tempdir().unwrap();
        let path = source(root.path(), &v2(true, Some("lmi"), 1));
        assert_eq!(prepare(&path, &|| false, &mut |_, _| {}).unwrap().partition, "recovery");
        fs::write(&path, v2(true, Some("nezha"), 1)).unwrap();
        assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
    }

    #[test]
    fn malformed_truncated_or_wrong_version_images_are_refused_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let mut bytes = v2(false, None, 1);
        bytes.truncate(8200);
        let path = source(root.path(), &bytes);
        assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
        bytes = v2(false, None, 1);
        bytes[40..44].copy_from_slice(&3u32.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
        fs::write(&path, b"not an image").unwrap();
        assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
    }

    #[test]
    fn images_over_partition_capacity_and_symbolic_sources_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = source(root.path(), &v2(false, None, 1));
        let linked = root.path().join("linked.img");
        symlink(&path, &linked).unwrap();
        assert!(prepare(&linked, &|| false, &mut |_, _| {}).is_err());
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(PARTITION_BYTES + 1).unwrap();
        assert!(prepare(&path, &|| false, &mut |_, _| {}).is_err());
    }

    #[test]
    fn prepared_snapshot_stays_unchanged_when_the_original_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let bytes = v2(false, None, 1);
        let path = source(root.path(), &bytes);
        let mut prepared = prepare(&path, &|| false, &mut |_, _| {}).unwrap();
        fs::write(&path, b"changed").unwrap();
        prepared.snapshot.seek(SeekFrom::Start(0)).unwrap();
        let mut got = vec![0; bytes.len()];
        prepared.snapshot.read_exact(&mut got).unwrap();
        assert_eq!(got, bytes);
        assert!(prepared.snapshot.write_all(b"cannot alter the accepted image").is_err());
        assert_eq!(prepared.snapshot.metadata().unwrap().len(), PARTITION_BYTES);
    }

    #[test]
    fn cancellation_during_ramdisk_scanning_stops_the_preparation() {
        let root = tempfile::tempdir().unwrap();
        let bytes = v2(false, None, 1);
        let path = source(root.path(), &bytes);
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            calls.set(calls.get()+1);
            calls.get() >= 4
        };
        assert!(prepare(&path, &cancelled, &mut |_, _| {}).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn preparation_can_be_cancelled_without_modifying_the_input() {
        let root = tempfile::tempdir().unwrap();
        let bytes = v2(false, None, 1);
        let path = source(root.path(), &bytes);
        assert!(prepare(&path, &|| true, &mut |_, _| {}).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn an_avb_footer_is_moved_to_the_partition_end_without_size_underflow() {
        let root = tempfile::tempdir().unwrap();
        let mut bytes = v2(true, None, 1);
        let payload = bytes.len() as u64;
        bytes.extend(b"AVB0");
        bytes.resize(bytes.len()+252, 0);
        let footer_at = bytes.len();
        let mut footer = [0; 64];
        footer[..4].copy_from_slice(b"AVBf");
        footer[4..8].copy_from_slice(&1u32.to_be_bytes());
        footer[12..20].copy_from_slice(&payload.to_be_bytes());
        footer[20..28].copy_from_slice(&payload.to_be_bytes());
        footer[28..36].copy_from_slice(&256u64.to_be_bytes());
        bytes.extend(footer);
        let path = source(root.path(), &bytes);
        let mut prepared = prepare(&path, &|| false, &mut |_, _| {}).unwrap();
        let mut got = [0; 64];
        prepared.snapshot.seek(SeekFrom::Start(PARTITION_BYTES-64)).unwrap();
        prepared.snapshot.read_exact(&mut got).unwrap();
        assert_eq!(got, footer);
        prepared.snapshot.seek(SeekFrom::Start(footer_at as u64)).unwrap();
        prepared.snapshot.read_exact(&mut got).unwrap();
        assert_eq!(got, [0; 64]);
    }

    #[test]
    fn a_nearly_full_image_footer_can_move_even_when_the_padding_is_less_than_64_bytes() {
        let root = tempfile::tempdir().unwrap();
        let mut bytes = v2(false, None, 1);
        let payload = bytes.len() as u64;
        bytes.extend(b"AVB0");
        bytes.resize(bytes.len()+252, 0);
        bytes.resize(PARTITION_BYTES as usize-72, 0);
        let mut footer = [0; 64];
        footer[..4].copy_from_slice(b"AVBf");
        footer[4..8].copy_from_slice(&1u32.to_be_bytes());
        footer[12..20].copy_from_slice(&payload.to_be_bytes());
        footer[20..28].copy_from_slice(&payload.to_be_bytes());
        footer[28..36].copy_from_slice(&256u64.to_be_bytes());
        bytes.extend(footer);
        let mut prepared = prepare(&source(root.path(), &bytes), &|| false, &mut |_, _| {}).unwrap();
        let mut got = [1; 72];
        prepared.snapshot.seek(SeekFrom::End(-72)).unwrap();
        prepared.snapshot.read_exact(&mut got).unwrap();
        assert_eq!(&got[..8], &[0; 8]);
        assert_eq!(&got[8..], &footer);
    }

    #[test]
    fn oversized_cpio_names_are_rejected_before_allocating_the_name() {
        let mut raw = archive(false, None);
        raw[94..102].copy_from_slice(b"ffffffff");
        assert!(inspect_ramdisk(&raw, &|| false).is_err());
    }

    #[test]
    fn an_unexpected_partition_size_is_rejected_without_changing_it() {
        let root = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let bytes = v2(false, None, 1);
        let mut prepared = prepare(&source(input.path(), &bytes), &|| false, &mut |_, _| {}).unwrap();
        let boot = root.path().join("boot");
        fs::write(&boot, b"original").unwrap();
        assert!(prepared.flash(root.path(), "boot", &mut |_, _| {}).is_err());
        assert_eq!(fs::read(boot).unwrap(), b"original");
    }

    #[test]
    fn a_confirmed_image_writes_only_its_allowed_target_and_reads_it_back() {
        let root = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let bytes = v2(false, None, 1);
        let mut prepared = prepare(&source(input.path(), &bytes), &|| false, &mut |_, _| {}).unwrap();
        let boot = root.path().join("boot");
        let recovery = root.path().join("recovery");
        for p in [&boot, &recovery] {
            let mut file = fs::File::create(p).unwrap();
            file.set_len(PARTITION_BYTES).unwrap();
            file.write_all(b"original").unwrap();
        }
        assert!(prepared.flash(root.path(), "recovery", &mut |_, _| {}).is_err());
        assert!(prepared.flash(root.path(), "vbmeta", &mut |_, _| {}).is_err());
        let mut progress = Vec::new();
        prepared.flash(root.path(), "boot", &mut |n, of| progress.push((n, of))).unwrap();
        assert_eq!(progress.last(), Some(&(PARTITION_BYTES*2, PARTITION_BYTES*2)));
        let mut file = fs::File::open(boot).unwrap();
        let mut prefix = vec![0; bytes.len()];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, bytes);
        let mut tail = [1; 64];
        file.seek(SeekFrom::End(-64)).unwrap();
        file.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [0; 64]);
        let mut untouched = [0; 8];
        fs::File::open(recovery).unwrap().read_exact(&mut untouched).unwrap();
        assert_eq!(&untouched, b"original");
    }
}
