use abootimg_oxide::{binrw::io::Cursor, Header, HeaderV0, HeaderV0Versioned};
use anyhow::{bail, ensure, Context, Result};
use sha1::{Digest, Sha1};

const HEADER_V2_SIZE: usize = 1660;

/// An Android v2 image, including the sections that follow its ramdisk.
pub struct LegacyBootImage {
    pub header: HeaderV0,
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
    second: Vec<u8>,
    recovery_dtbo: Vec<u8>,
    dtb: Vec<u8>,
}

impl LegacyBootImage {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let Header::V0(header) = Header::parse(&mut Cursor::new(bytes)).context("header")? else {
            bail!("expected a version 2 boot image");
        };
        let HeaderV0Versioned::V2 { recovery_dtbo_size, recovery_dtbo_addr, dtb_size, .. } = header.versioned else {
            bail!("expected a version 2 boot image, found version {}", header.header_version());
        };
        let page = header.page_size as usize;
        ensure!(page >= HEADER_V2_SIZE && page.is_power_of_two(), "invalid v2 page size {page}");

        let mut at = page;
        let mut section = |len: u32, name: &str| -> Result<(usize, Vec<u8>)> {
            let len = len as usize;
            let end = at.checked_add(len).context("section end overflow")?;
            let data = bytes.get(at..end).with_context(|| format!("{name} runs past the end of the image"))?;
            let offset = at;
            at = end.checked_next_multiple_of(page).context("section alignment overflow")?;
            Ok((offset, data.to_vec()))
        };
        let (_, kernel) = section(header.kernel_size, "kernel")?;
        let (_, ramdisk) = section(header.ramdisk_size, "ramdisk")?;
        let (_, second) = section(header.second_bootloader_size, "second stage")?;
        let (dtbo_offset, recovery_dtbo) = section(recovery_dtbo_size, "recovery DTBO")?;
        let (_, dtb) = section(dtb_size, "DTB")?;
        ensure!(
            recovery_dtbo_size == 0 || recovery_dtbo_addr == dtbo_offset as u64,
            "v2 recovery DTBO offset disagrees with the section layout"
        );
        Ok(Self { header, kernel, ramdisk, second, recovery_dtbo, dtb })
    }

    /// Write the v2 payload, updating its lengths, DTBO offset and AOSP image ID.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut header = self.header.clone();
        let page = header.page_size as usize;
        let HeaderV0Versioned::V2 { recovery_dtbo_size, recovery_dtbo_addr, dtb_size, .. } = &mut header.versioned else {
            bail!("expected a version 2 boot image");
        };
        ensure!(page >= HEADER_V2_SIZE && page.is_power_of_two(), "invalid v2 page size {page}");

        let parts = [&self.kernel, &self.ramdisk, &self.second, &self.recovery_dtbo, &self.dtb];
        let mut out = vec![0; page];
        let mut digest = Sha1::new();
        let mut sizes = [0u32; 5];
        for (index, data) in parts.into_iter().enumerate() {
            let size = u32::try_from(data.len()).context("v2 section size")?;
            sizes[index] = size;
            digest.update(data);
            digest.update(size.to_le_bytes());
            if index == 3 {
                *recovery_dtbo_addr = if size == 0 { 0 } else { out.len() as u64 };
            }
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(page), 0);
        }
        [header.kernel_size, header.ramdisk_size, header.second_bootloader_size, *recovery_dtbo_size, *dtb_size] = sizes;
        header.hash_digest = [0; 32];
        header.hash_digest[..20].copy_from_slice(&digest.finalize());
        Header::V0(header).write(&mut &mut out[..HEADER_V2_SIZE]).context("write v2 header")?;
        Ok(out)
    }
}
