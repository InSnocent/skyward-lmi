//! Letting a key into a filesystem, so the files it was used on can be
//! read. The kernel keeps the key for the filesystem, not for any one
//! file, and names it by a hash it works out itself.

#[cfg(target_os = "android")]
use anyhow::bail;
#[cfg(any(target_os = "android", test))]
use anyhow::{Context, Result};
#[cfg(any(target_os = "android", test))]
use std::path::Path;

/// How long the name the kernel gives a key is.
pub const NAME_BYTES: usize = 16;

/// Android's file encryption policy version.
#[cfg(any(target_os = "android", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    V1,
    V2,
}

#[cfg(any(target_os = "android", test))]
impl Policy {
    pub fn from_mode(mode: &str) -> Self {
        if mode.split(['+', ':']).any(|part| part == "v1") { Self::V1 } else { Self::V2 }
    }

    fn reference_bytes(self) -> usize {
        match self {
            Self::V1 => 8,
            Self::V2 => NAME_BYTES,
        }
    }
}

#[cfg(any(target_os = "android", test))]
fn key_arg(raw: &[u8], wrapped: bool, policy: Policy) -> Vec<u8> {
    const HEADER_BYTES: usize = 80;
    let mut arg = vec![0u8; HEADER_BYTES + raw.len()];
    let kind = match policy {
        Policy::V1 => {
            use sha2::{Digest, Sha512};
            // With a wrapped v1 key, Android derives the descriptor from
            // the stable half. The second half changes across boots.
            let descriptor_key = if wrapped { &raw[..raw.len() / 2] } else { raw };
            let hash = Sha512::digest(Sha512::digest(descriptor_key));
            arg[8..16].copy_from_slice(&hash[..8]);
            1u32
        }
        Policy::V2 => 2u32,
    };
    arg[0..4].copy_from_slice(&kind.to_ne_bytes());
    arg[40..44].copy_from_slice(&(raw.len() as u32).to_ne_bytes());
    if wrapped {
        arg[76..80].copy_from_slice(&1u32.to_ne_bytes());
    }
    arg[HEADER_BYTES..].copy_from_slice(raw);
    arg
}

/// Let a key into the filesystem and return the key reference it uses.
#[cfg(target_os = "android")]
pub fn add_key(mount: &Path, raw: &[u8], wrapped: bool, policy: Policy) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    const ADD_KEY: u32 = 0xc050_6617;

    let mut arg = key_arg(raw, wrapped, policy);
    let dir = std::fs::File::open(mount).with_context(|| format!("open {}", mount.display()))?;
    if unsafe { libc::ioctl(dir.as_raw_fd(), ADD_KEY as libc::Ioctl, arg.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("let a key into {}", mount.display()));
    }
    Ok(arg[8..8 + policy.reference_bytes()].to_vec())
}

/// Whether the names and contents under `dir` read as they are: it is not
/// encrypted, or the key it is encrypted with is in the kernel. A user's
/// own files read only once that user's key is let in.
#[cfg(target_os = "android")]
pub fn unlocked(dir: &Path) -> Result<bool> {
    use std::os::fd::AsRawFd;
    const GET_POLICY: u32 = 0xc009_6616;
    const GET_KEY_STATUS: u32 = 0xc080_661a;
    /// How the first kind of policy names its key, and how the second does.
    const BY_DESCRIPTOR: u32 = 1;
    const BY_IDENTIFIER: u32 = 2;
    /// The second kind of policy is the largest: its version, three bytes
    /// of how it encrypts, four reserved, and the key's name.
    const POLICY_BYTES: usize = 24;
    const PRESENT: u32 = 2;

    let folder = std::fs::File::open(dir).with_context(|| format!("open {}", dir.display()))?;
    let mut policy = [0u8; 8 + POLICY_BYTES];
    policy[..8].copy_from_slice(&(POLICY_BYTES as u64).to_ne_bytes());
    if unsafe { libc::ioctl(folder.as_raw_fd(), GET_POLICY as libc::Ioctl, policy.as_mut_ptr()) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENODATA) {
            return Ok(true);
        }
        return Err(e).with_context(|| format!("read how {} is encrypted", dir.display()));
    }
    let mut status = [0u8; 128];
    match policy[8] {
        0 => {
            status[..4].copy_from_slice(&BY_DESCRIPTOR.to_ne_bytes());
            status[8..16].copy_from_slice(&policy[12..20]);
        }
        2 => {
            status[..4].copy_from_slice(&BY_IDENTIFIER.to_ne_bytes());
            status[8..8 + NAME_BYTES].copy_from_slice(&policy[16..16 + NAME_BYTES]);
        }
        other => bail!("{} is encrypted under a policy of version {other}, which is not known here", dir.display()),
    }
    if unsafe { libc::ioctl(folder.as_raw_fd(), GET_KEY_STATUS as libc::Ioctl, status.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("ask whether the key for {} is in", dir.display()));
    }
    Ok(u32::from_ne_bytes(status[64..68].try_into().expect("four bytes")) == PRESENT)
}

/// Whether what a phone says about how it encrypts its files means the
/// keys are ones only the storage hardware can unwrap.
pub fn hardware_wrapped(mode: &str) -> bool {
    mode.split(['+', ':']).any(|part| part == "wrappedkey_v0")
}

/// Read the name the phone recorded for a key it let in, to check against
/// the name the kernel works out now.
#[cfg(any(target_os = "android", test))]
pub fn recorded_name(path: &Path, policy: Policy) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let expected = policy.reference_bytes();
    anyhow::ensure!(raw.len() == expected, "{} holds {} bytes, expected {expected}", path.display(), raw.len());
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v1_policy_uses_androids_eight_byte_descriptor() {
        let arg = key_arg(b"test-only", false, Policy::V1);
        assert_eq!(&arg[..4], &1u32.to_ne_bytes());
        assert_eq!(&arg[8..16], &[0x80, 0x3a, 0x9e, 0x0b, 0x3a, 0x19, 0x58, 0x6b]);
        assert_eq!(&arg[76..80], &0u32.to_ne_bytes());
        assert_eq!(&arg[80..], b"test-only");
        assert_eq!(Policy::from_mode("ice:aes-256-cts:v1+wrappedkey_v0"), Policy::V1);
        assert_eq!(Policy::from_mode("aes-256-xts:aes-256-cts:v2"), Policy::V2);
    }

    #[test]
    fn the_v1_wrapped_descriptor_ignores_the_per_boot_half() {
        let mut first = [0x31u8; 64];
        let mut second = first;
        first[32..].fill(0x41);
        second[32..].fill(0x42);
        let known = [0x3c, 0x36, 0x26, 0x52, 0x9a, 0x7f, 0xcb, 0xf2];
        let wrapped = key_arg(&first, true, Policy::V1);
        assert_eq!(&wrapped[8..16], &known);
        assert_eq!(&wrapped[76..80], &1u32.to_ne_bytes());
        assert_eq!(&wrapped[80..], &first);
        assert_eq!(&key_arg(&second, true, Policy::V1)[8..16], &known);
        assert_ne!(&key_arg(&first, false, Policy::V1)[8..16], &key_arg(&second, false, Policy::V1)[8..16]);
    }

    #[test]
    fn a_phone_says_whether_the_hardware_holds_the_keys() {
        assert!(hardware_wrapped("aes-256-xts:aes-256-cts:v2+inlinecrypt_optimized+wrappedkey_v0"));
        assert!(!hardware_wrapped("aes-256-xts:aes-256-cts:v2"));
        assert!(!hardware_wrapped("aes-256-xts:aes-256-cts:v2+wrappedkey_v1"));
    }

    #[test]
    fn a_name_is_read_back_only_at_its_own_length() {
        let dir = std::env::temp_dir().join(format!("skyward-fscrypt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("ref");
        std::fs::write(&good, [7u8; NAME_BYTES]).unwrap();
        assert_eq!(recorded_name(&good, Policy::V2).unwrap(), [7u8; NAME_BYTES]);
        let short = dir.join("short");
        std::fs::write(&short, [7u8; 8]).unwrap();
        assert!(recorded_name(&short, Policy::V2).is_err());
        std::fs::write(&short, [3u8; 8]).unwrap();
        assert_eq!(recorded_name(&short, Policy::V1).unwrap(), [3u8; 8]);
    }
}
