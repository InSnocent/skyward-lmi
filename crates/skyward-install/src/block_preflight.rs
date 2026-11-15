//! Read the declared targets of an Android non-A/B block OTA.
use crate::package::{Metadata, UPDATE_BINARY};
use anyhow::{ensure, Context, Result};
use std::io::{Read, Seek};
use zip::ZipArchive;

pub const ENABLED: &str = "skyward/block-ota-preflight";
const METADATA: &str = "META-INF/com/android/metadata";
const SCRIPT: &str = "META-INF/com/google/android/updater-script";
const OPERATIONS: &str = "dynamic_partitions_op_list";
const MAX_TEXT: u64 = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockPreflight {
    pub device: String,
    pub logical: Vec<String>,
    pub physical: Vec<String>,
}

pub fn inspect(source: impl Read + Seek, device: &str) -> Result<BlockPreflight> {
    let mut zip = ZipArchive::new(source).context("open the OTA zip")?;
    let metadata = Metadata::parse(&text(&mut zip, METADATA)?);
    ensure!(metadata.get("ota-type") == Some("BLOCK"), "expected a non-A/B BLOCK OTA");
    ensure!(!device.is_empty() && metadata.get("pre-device").is_some_and(|names| names.split('|').any(|name| name == device)),
        "the OTA declares {:?} and this device is {device}", metadata.get("pre-device"));
    ensure!(zip.index_for_name(UPDATE_BINARY).is_some(), "the OTA is missing its updater");
    let script = text(&mut zip, SCRIPT)?;
    let operations = text(&mut zip, OPERATIONS)?;
    ensure!(script.contains("update_dynamic_partitions("), "the OTA script does not declare a dynamic partition update");

    let mut logical = Vec::new();
    for line in operations.lines() {
        let mut words = line.split_whitespace();
        if let (Some("resize"), Some(name), Some(size), None) = (words.next(), words.next(), words.next(), words.next()) {
            ensure!(valid_name(name), "invalid logical partition name");
            ensure!(size.parse::<u64>()? > 0, "invalid size for {name}");
            if script.contains(&format!("block_image_update(map_partition(\"{name}\")"))
                && !logical.iter().any(|p| p == name)
            {
                logical.push(name.to_owned());
            }
        }
    }
    ensure!(!logical.is_empty(), "the OTA declares no block-image targets");

    let mut physical = Vec::new();
    for line in script.lines().map(str::trim_start).filter(|line| line.starts_with("package_extract_file(")) {
        let Some((_, destination)) = line.split_once(',') else { continue };
        let Some(destination) = destination.trim().strip_prefix('"').and_then(|rest| rest.split('"').next()) else { continue };
        let Some(name) = destination.strip_prefix("/dev/block/bootdevice/by-name/")
            .or_else(|| destination.strip_prefix("/dev/block/by-name/")) else { continue };
        ensure!(valid_name(name), "invalid physical partition name");
        if !physical.iter().any(|p| p == name) {
            physical.push(name.to_owned());
        }
    }
    Ok(BlockPreflight { device: device.into(), logical, physical })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn text(zip: &mut ZipArchive<impl Read + Seek>, name: &str) -> Result<String> {
    let entry = zip.by_name(name).with_context(|| format!("open {name}"))?;
    ensure!(entry.size() <= MAX_TEXT, "{name} is too large to inspect");
    let mut text = String::new();
    entry.take(MAX_TEXT + 1).read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= MAX_TEXT, "{name} is too large to inspect");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn archive(ty: &str, device: &str) -> Cursor<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let metadata = format!("ota-type={ty}\npre-device={device}\n");
        for (name, bytes) in [
            (METADATA, metadata.as_bytes()),
            (UPDATE_BINARY, b"this entry is never executed"),
            (SCRIPT, br#"assert(update_dynamic_partitions(package_extract_file("dynamic_partitions_op_list")));
block_image_update(map_partition("system"), package_extract_file("system.transfer.list"), "system.new.dat.br", "system.patch.dat");
package_extract_file("boot.img", "/dev/block/bootdevice/by-name/boot");
package_extract_file("extra.img", "/tmp/extra.img");"#),
            (OPERATIONS, b"remove_all_groups\nresize system 4096\nresize vendor 8192\n"),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let mut data = zip.finish().unwrap();
        data.set_position(0);
        data
    }

    #[test]
    fn reports_the_targets_declared_by_the_block_script() {
        let report = inspect(archive("BLOCK", "lmi"), "lmi").unwrap();
        assert_eq!(report.logical, ["system"]);
        assert_eq!(report.physical, ["boot"]);
    }

    #[test]
    fn rejects_incompatible_metadata() {
        assert!(inspect(archive("AB", "lmi"), "lmi").is_err());
        assert!(inspect(archive("BLOCK", "alioth"), "lmi").is_err());
    }
}
