//! Inspect complete A-only OTAs and manually selected recovery-script ZIPs.

use crate::Command;
pub use crate::signature::ZipSignature;
use crate::block_preflight::{self, BlockPreflight};
use crate::package::{Metadata, UPDATE_BINARY};
use anyhow::{Context, Result, ensure};
use std::io::{Read, Seek};
use zip::ZipArchive;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Block(BlockPreflight),
    Script,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub kind: Kind,
    pub sha256: [u8; 32],
    pub signature: ZipSignature,
}

impl Report {
    pub fn requires_approval(&self) -> bool {
        self.signature != ZipSignature::Trusted
    }

    pub fn accepts(&self, command: &Command) -> bool {
        matches!((self.requires_approval(), command), (false, Command::Install) | (true, Command::InstallUnverifiedZip))
    }

    pub fn sha256_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(64);
        for byte in self.sha256 {
            write!(text, "{byte:02x}").expect("writing to a string succeeds");
        }
        text
    }
}

pub fn inspect(source: impl Read + Seek, device: &str) -> Result<Kind> {
    let mut zip = ZipArchive::new(source).context("open the install ZIP")?;
    ensure!(zip.index_for_name("payload.bin").is_none(), "A/B payload OTAs are not supported on this device");
    ensure!(!skyward_systems::package::is_system(&zip), "second-system ZIPs are not supported on this device");
    let updater = zip.by_name(UPDATE_BINARY).context("the ZIP has no recovery installer")?;
    ensure!(updater.is_file() && updater.size() > 0, "the ZIP has no usable recovery installer");
    drop(updater);

    let metadata = if zip.index_for_name(block_preflight::METADATA).is_some() {
        Metadata::parse(&block_preflight::text(&mut zip, block_preflight::METADATA)?)
    } else {
        Metadata::default()
    };
    if let Some(devices) = metadata.get("pre-device") {
        ensure!(!device.is_empty() && devices.split('|').any(|name| name.trim() == device),
            "the ZIP declares {devices}, but this device is {device}");
    }
    if metadata.get("ota-type").is_some() || zip.index_for_name(block_preflight::OPERATIONS).is_some() {
        return block_preflight::inspect_archive(&mut zip, &metadata, device).map(Kind::Block);
    }
    Ok(Kind::Script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn archive(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap()
    }

    #[test]
    fn confirmation_is_bound_to_the_verified_status_of_this_package() {
        let mut report = Report { kind: Kind::Script, sha256: [0; 32], signature: ZipSignature::Unsigned };
        for status in [ZipSignature::Unsigned, ZipSignature::Unverified, ZipSignature::PublicTest] {
            report.signature = status;
            assert!(!report.accepts(&Command::Install));
            assert!(!report.accepts(&Command::Cancel));
            assert!(report.accepts(&Command::InstallUnverifiedZip));
        }
        report.signature = ZipSignature::Trusted;
        assert!(!report.accepts(&Command::InstallUnverifiedZip));
        assert!(report.accepts(&Command::Install));
    }

    #[test]
    fn accepts_a_zip_with_only_an_installer_as_a_script() {
        let zip = archive(&[(UPDATE_BINARY, b"#!/sbin/sh\nexit 0")]);
        assert_eq!(inspect(zip, "lmi").unwrap(), Kind::Script);
    }

    #[test]
    fn requires_an_actual_recovery_installer() {
        assert!(inspect(archive(&[("readme", b"hello")]), "lmi").is_err());
        assert!(inspect(archive(&[(UPDATE_BINARY, b"")]), "lmi").is_err());
    }

    #[test]
    fn a_foreign_device_declared_by_a_script_zip_is_rejected() {
        let zip = archive(&[(UPDATE_BINARY, b"script"), (block_preflight::METADATA, b"pre-device=other\n")]);
        assert!(inspect(zip, "lmi").is_err());
    }

    #[test]
    fn ota_markers_cannot_fall_back_to_the_script_path() {
        for extra in [
            ("payload.bin", b"payload".as_slice()),
            (block_preflight::METADATA, b"ota-type=AB\npre-device=lmi\n"),
            (block_preflight::OPERATIONS, b"remove_all_groups\n"),
        ] {
            assert!(inspect(archive(&[(UPDATE_BINARY, b"script"), extra]), "lmi").is_err());
        }
    }
}
