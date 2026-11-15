//! The single log-only package accepted by the temporary sideload test image.
use crate::package::UPDATE_BINARY;
use anyhow::Result;
use std::io::{Read, Seek};

pub const CERTIFICATES: &str = "/skyward/sideload-test-otacerts.zip";
pub const MAX_SIZE: u64 = 64 * 1024;
pub const SCRIPT: &[u8] = include_bytes!("sideload_test.sh");

/// Check the archive before exposing an install confirmation. Only the
/// built-in script may be executed, even when a package has a valid signature.
pub fn accepts(source: impl Read + Seek) -> Result<bool> {
    let mut zip = zip::ZipArchive::new(source)?;
    if zip.len() != 1 {
        return Ok(false);
    }
    let mut entry = zip.by_index(0)?;
    if entry.name() != UPDATE_BINARY || entry.size() != SCRIPT.len() as u64 {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(SCRIPT.len());
    (&mut entry).take(SCRIPT.len() as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes == SCRIPT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn archive(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents).unwrap();
        }
        let mut zip = zip.finish().unwrap();
        zip.set_position(0);
        zip
    }

    #[test]
    fn accepts_only_the_builtin_script_as_the_only_entry() {
        assert!(accepts(archive(&[(UPDATE_BINARY, SCRIPT)])).unwrap());
        let mut changed = SCRIPT.to_vec();
        changed[0] ^= 1;
        assert!(!accepts(archive(&[(UPDATE_BINARY, &changed)])).unwrap());
        assert!(!accepts(archive(&[(UPDATE_BINARY, SCRIPT), ("extra", b"")])).unwrap());
        assert!(!accepts(archive(&[("elsewhere", SCRIPT)])).unwrap());
    }
}
