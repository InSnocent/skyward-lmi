//! The filesystem stored inside an installed Android APEX.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::Path,
};

#[cfg(target_os = "android")]
mod zram;
#[cfg(target_os = "android")]
pub use zram::Mounted;

const BUNDLE_IMAGE: &str = "apex_payload.img";
const PAGE: u64 = 4096;
const SUPERBLOCK: u64 = 1024;
const MAX_PAYLOAD: u64 = 128 * 1024 * 1024;

pub struct Payload {
    file: File,
    offset: u64,
    length: u64,
    filesystem: &'static str,
}

impl Payload {
    fn copy_to(mut self, destination: &mut impl Write) -> Result<()> {
        self.file.seek(SeekFrom::Start(self.offset))?;
        let copied = io::copy(&mut self.file.take(self.length), destination)?;
        ensure!(copied == self.length, "the installed APEX payload ended before its declared length");
        Ok(())
    }
}

/// The stored payload's location for the recovery's existing APEX path.
pub fn image(file: &Path) -> Result<(u64, u64)> {
    let input = File::open(file).with_context(|| format!("open {}", file.display()))?;
    extent(&input, file)
}

fn extent(input: &File, path: &Path) -> Result<(u64, u64)> {
    let mut bundle = zip::ZipArchive::new(input).with_context(|| format!("read what is in {}", path.display()))?;
    let entry = bundle.by_name(BUNDLE_IMAGE).with_context(|| format!("{} holds no {BUNDLE_IMAGE}", path.display()))?;
    ensure!(entry.compression() == zip::CompressionMethod::Stored,
        "{} keeps its {BUNDLE_IMAGE} packed, which cannot be opened where it lies", path.display());
    let offset = entry.data_start().with_context(|| format!("{BUNDLE_IMAGE} in {} says where it is not", path.display()))?;
    Ok((offset, entry.size()))
}

/// Open only the extent described by an aligned, stored APEX payload.
pub fn mountable_image(path: &Path) -> Result<Payload> {
    let input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let (offset, length) = extent(&input, path)?;
    let file_length = input.metadata()?.len();
    ensure!(offset.is_multiple_of(PAGE) && length >= PAGE && length.is_multiple_of(PAGE)
        && offset.checked_add(length).is_some_and(|end| end <= file_length),
        "{} has an invalid APEX image extent", path.display());
    ensure!(length <= MAX_PAYLOAD, "{} has an APEX payload too large to stage in RAM", path.display());
    let mut superblock = [0; 58];
    input.read_exact_at(&mut superblock, offset + SUPERBLOCK)
        .with_context(|| format!("read the APEX superblock in {}", path.display()))?;
    let filesystem = if superblock[..4] == [0xe2, 0xe1, 0xf5, 0xe0] {
        "erofs"
    } else if superblock[56..58] == [0x53, 0xef] {
        "ext4"
    } else {
        anyhow::bail!("the APEX image in {} has an unsupported filesystem", path.display());
    };
    Ok(Payload { file: input, offset, length, filesystem })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    fn sample(alignment: u16, length: usize, magic_offset: usize, magic: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut payload = vec![0; length];
        payload[magic_offset..magic_offset + magic.len()].copy_from_slice(magic);
        let mut archive = ZipWriter::new(file.reopen().unwrap());
        archive.start_file(BUNDLE_IMAGE, SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
            .with_alignment(alignment)).unwrap();
        archive.write_all(&payload).unwrap();
        archive.finish().unwrap();
        file
    }

    #[test]
    fn an_aligned_apex_uses_its_actual_filesystem() {
        for (offset, magic, expected) in [(1024, &b"\xe2\xe1\xf5\xe0"[..], "erofs"), (1080, &b"\x53\xef"[..], "ext4")] {
            let file = sample(4096, 8192, offset, magic);
            let payload = mountable_image(file.path()).unwrap();
            assert_eq!((payload.offset, payload.length, payload.filesystem), (4096, 8192, expected));
            assert_eq!(image(file.path()).unwrap(), (payload.offset, payload.length));
        }
    }

    #[test]
    fn copies_only_the_stored_filesystem_and_rejects_a_short_read() {
        let file = sample(4096, 8192, 1080, b"\x53\xef");
        let bytes = std::fs::read(file.path()).unwrap();
        let payload = mountable_image(file.path()).unwrap();
        let mut result = Vec::new();
        payload.copy_to(&mut result).unwrap();
        assert_eq!(result, &bytes[4096..12288]);

        let payload = mountable_image(file.path()).unwrap();
        file.as_file().set_len(8192).unwrap();
        assert!(payload.copy_to(&mut Vec::new()).is_err());
    }

    #[test]
    fn a_short_unaligned_or_unknown_apex_is_refused_before_mounting() {
        for file in [
            sample(4096, 2048, 1080, b"\x53\xef"),
            sample(1, 4096, 1080, b"\x53\xef"),
            sample(4096, 4096, 1080, b"\x00\x00"),
        ] {
            assert!(mountable_image(file.path()).is_err());
        }
    }
}
