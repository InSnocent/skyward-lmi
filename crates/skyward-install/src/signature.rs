//! Whether a package was signed with one of the keys the phone takes its
//! updates signed with, checked as Android's recovery checks it: one
//! signature of all of the zip before its comment's length, kept at the end
//! of the comment.

use anyhow::{bail, ensure, Context, Result};
use cms::content_info::ContentInfo;
use cms::signed_data::SignedData;
use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use x509_cert::der::oid::ObjectIdentifier;
use x509_cert::der::{Decode, DecodePem, Encode};
use x509_cert::Certificate;
use zip::ZipArchive;

/// Where Android's recovery keeps the certificates of the keys the phone's
/// updates are signed with.
pub const CERTIFICATES: &str = "/system/etc/security/otacerts.zip";

/// A signed package ends in six bytes: how far back from its end the
/// signature starts, two bytes of 0xff, and how long the zip's comment is,
/// both little-endian.
const FOOTER: usize = 6;

/// The record that ends a zip's directory, before the comment that ends
/// it, and the bytes it starts with.
const EOCD: usize = 22;
const EOCD_MAGIC: &[u8; 4] = b"PK\x05\x06";

/// How much of a package is read at a time.
const CHUNK: usize = 1 << 20;

const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const RSA_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const EC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");

/// The algorithms a certificate may be signed with, and the digest of the
/// package a signature with its key is made over. Certificates signed with
/// MD5 have always been taken as signed with SHA-1.
const DIGESTS: [(ObjectIdentifier, Hash); 6] = [
    // md5WithRSAEncryption, md5WithRSA
    (ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.4"), Hash::Sha1),
    (ObjectIdentifier::new_unwrap("1.3.14.3.2.3"), Hash::Sha1),
    // sha1WithRSAEncryption, sha1WithRSA
    (ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.5"), Hash::Sha1),
    (ObjectIdentifier::new_unwrap("1.3.14.3.2.29"), Hash::Sha1),
    // sha256WithRSAEncryption, ecdsa-with-SHA256
    (ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11"), Hash::Sha256),
    (ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2"), Hash::Sha256),
];

/// A key packages may be signed with, and the digest of a package its
/// signature is made over.
pub struct Key {
    public: Public,
    hash: Hash,
    public_test_key: bool,
}

// SHA-256 of the DER SubjectPublicKeyInfo for AOSP's public testkey.
// Match the key, not the certificate name or signature, since the same
// key can appear in several certificates.
const AOSP_TEST_KEY: [u8; 32] = [
    0xef, 0x57, 0xb6, 0x90, 0x16, 0x5c, 0xb5, 0x61,
    0xb5, 0x02, 0x69, 0x22, 0xc0, 0x0d, 0x2d, 0x65,
    0x74, 0xe8, 0xb1, 0x84, 0xfa, 0x7d, 0x16, 0x1e,
    0x07, 0x6f, 0x06, 0xe0, 0x6e, 0x6d, 0x35, 0xdb,
];

enum Public {
    Rsa(RsaPublicKey),
    Ec(p256::ecdsa::VerifyingKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha256,
}

/// What checking a package's signature found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Signed with one of the keys.
    Signed,
    /// The package has no signature of the whole of it.
    Unsigned,
    /// Its signature was made with none of the keys, or not of the package
    /// as it is now.
    Unverified,
    /// The check was stopped before it had read the package.
    Stopped,
}

/// Provenance on the manual lmi ZIP path. A public development key
/// can verify the package's signature without identifying its publisher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipSignature {
    Trusted,
    PublicTest,
    Unsigned,
    Unverified,
}

impl ZipSignature {
    pub fn matches_included_key(self) -> bool {
        matches!(self, Self::Trusted | Self::PublicTest)
    }

    fn legacy(self) -> Verdict {
        match self {
            Self::Trusted | Self::PublicTest => Verdict::Signed,
            Self::Unsigned => Verdict::Unsigned,
            Self::Unverified => Verdict::Unverified,
        }
    }
}

/// The keys of the certificates in the zip `zip`: those of its entries
/// whose names end in `.x509.pem`, as Android's recovery takes them.
pub fn keys<R: Read + Seek>(zip: R) -> Result<Vec<Key>> {
    let mut zip = ZipArchive::new(zip).context("read the certificates' zip")?;
    let mut keys = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("open an entry of the certificates' zip")?;
        if !entry.name().ends_with(".x509.pem") {
            continue;
        }
        let name = entry.name().to_string();
        let mut pem = Vec::new();
        entry.read_to_end(&mut pem).with_context(|| format!("read {name}"))?;
        keys.push(key(&pem).with_context(|| format!("take the key of {name}"))?);
    }
    ensure!(!keys.is_empty(), "the zip holds no certificate");
    Ok(keys)
}

/// The key of a PEM certificate. Android's recovery takes an RSA key of
/// 2048 or 4096 bits with the exponent 3 or 65537, or a P-256 key.
fn key(pem: &[u8]) -> Result<Key> {
    let cert = Certificate::from_pem(pem).context("read the certificate")?;
    let signed_with = cert.signature_algorithm.oid;
    let Some(&(_, hash)) = DIGESTS.iter().find(|(oid, _)| *oid == signed_with) else {
        bail!("a certificate signed with {signed_with} is not taken");
    };
    let info = &cert.tbs_certificate.subject_public_key_info;
    let der = info.to_der().context("encode the public key")?;
    let public = match info.algorithm.oid {
        RSA_KEY => {
            let rsa = RsaPublicKey::from_public_key_der(&der).context("read the RSA key")?;
            let bits = rsa.n().bits();
            ensure!(bits == 2048 || bits == 4096, "an RSA key of {bits} bits is not taken");
            ensure!([3u32, 65537].map(BigUint::from).contains(rsa.e()), "an RSA key with the exponent {} is not taken", rsa.e());
            Public::Rsa(rsa)
        }
        EC_KEY => Public::Ec(p256::PublicKey::from_public_key_der(&der).context("read the key as a P-256 key")?.into()),
        other => bail!("a key of the algorithm {other} is not taken"),
    };
    let public_test_key = <[u8; 32]>::from(Sha256::digest(&der)) == AOSP_TEST_KEY;
    Ok(Key { public, hash, public_test_key })
}

/// Check that the package `file`, `size` bytes, was signed with one of
/// `keys`. `read` hears, as it is read, how much of what the signature is
/// of has been read, and of how much; the check stops when it answers
/// false.
pub fn verify<R: Read + Seek>(file: R, size: u64, keys: &[Key], read: impl FnMut(u64, u64) -> bool) -> Result<Verdict> {
    verify_zip(file, size, keys, None, false, read).map(|(status, _)| status.map_or(Verdict::Stopped, ZipSignature::legacy))
}

/// Verify the OTA signature and match the exact ZIP selected for this build.
pub fn verify_pinned<R: Read + Seek>(
    file: R, size: u64, keys: &[Key], digest: [u8; 32], read: impl FnMut(u64, u64) -> bool,
) -> Result<Verdict> {
    verify_zip(file, size, keys, Some(digest), false, read).map(|(status, _)| status.map_or(Verdict::Stopped, ZipSignature::legacy))
}

/// Read and hash the whole file in the same pass that checks its signature.
/// Even an unsigned ZIP is read to the end, so its bytes are pinned by the
/// package filesystem before the installer asks the user to confirm.
pub fn verify_and_hash<R: Read + Seek>(
    file: R, size: u64, keys: &[Key], read: impl FnMut(u64, u64) -> bool,
) -> Result<Option<(ZipSignature, [u8; 32])>> {
    let (signature, digest) = verify_zip(file, size, keys, None, true, read)?;
    Ok(signature.zip(digest))
}

struct SignedEnd {
    record: Vec<u8>,
    signature_start: usize,
}

fn signed_end(file: &mut (impl Read + Seek), size: u64) -> Result<Result<SignedEnd, ZipSignature>> {
    if size < FOOTER as u64 {
        return Ok(Err(ZipSignature::Unsigned));
    }
    let mut footer = [0u8; FOOTER];
    file.seek(SeekFrom::Start(size - FOOTER as u64)).context("seek to the package's last bytes")?;
    file.read_exact(&mut footer).context("read the package's last bytes")?;
    if footer[2..4] != [0xff, 0xff] {
        return Ok(Err(ZipSignature::Unsigned));
    }
    let start = usize::from(u16::from_le_bytes([footer[0], footer[1]]));
    let comment = usize::from(u16::from_le_bytes([footer[4], footer[5]]));
    // There must be only one end record, or a ZIP parser could look at
    // a directory outside the bytes that were signed.
    let tail = EOCD + comment;
    if start > comment || start <= FOOTER || size < tail as u64 {
        return Ok(Err(ZipSignature::Unverified));
    }
    let mut record = vec![0u8; tail];
    file.seek(SeekFrom::Start(size - tail as u64)).context("seek to the end of the zip's directory")?;
    file.read_exact(&mut record).context("read the end of the zip's directory")?;
    if !record.starts_with(EOCD_MAGIC) || record[4..].windows(4).any(|w| w == EOCD_MAGIC) {
        return Ok(Err(ZipSignature::Unverified));
    }
    Ok(Ok(SignedEnd { record, signature_start: tail - start }))
}

fn verify_zip<R: Read + Seek>(
    mut file: R, size: u64, keys: &[Key], expected: Option<[u8; 32]>, whole: bool,
    mut read: impl FnMut(u64, u64) -> bool,
) -> Result<(Option<ZipSignature>, Option<[u8; 32]>)> {
    let end = signed_end(&mut file, size)?;
    if let Err(verdict) = end
        && !whole
    {
        return Ok((Some(verdict), None));
    }
    // The signature is of all but the comment and its length. An unsigned
    // or structurally invalid signature leaves the entire file to hash.
    let suffix = end.as_ref().map_or(&[][..], |end| &end.record[EOCD - 2..]);
    let signed = size - suffix.len() as u64;
    let want_digest = whole || expected.is_some();
    let wanted = |hash: Hash| end.is_ok() && keys.iter().any(|k| k.hash == hash);
    let mut sha1 = wanted(Hash::Sha1).then(Sha1::new);
    let mut sha256 = (wanted(Hash::Sha256) || want_digest).then(Sha256::new);
    file.seek(SeekFrom::Start(0)).context("rewind the package")?;
    let mut chunk = vec![0u8; CHUNK];
    let mut done = 0;
    let total = if whole { size } else { signed };
    while done < signed {
        let n = (signed - done).min(CHUNK as u64) as usize;
        file.read_exact(&mut chunk[..n]).context("read the package")?;
        if let Some(h) = &mut sha1 {
            h.update(&chunk[..n]);
        }
        if let Some(h) = &mut sha256 {
            h.update(&chunk[..n]);
        }
        done += n as u64;
        if !read(done, total) {
            return Ok((None, None));
        }
    }
    let digest: Option<[u8; 32]> = want_digest.then(|| {
        sha256.as_ref().expect("the package is being hashed").clone().chain_update(suffix).finalize().into()
    });
    if whole && !suffix.is_empty() && !read(size, size) {
        return Ok((None, None));
    }
    if expected.is_some() && digest != expected {
        return Ok((Some(ZipSignature::Unverified), digest));
    }
    let end = match end {
        Ok(end) => end,
        Err(verdict) => return Ok((Some(verdict), digest)),
    };
    let sha1 = sha1.map(|h| h.finalize().to_vec()).unwrap_or_default();
    let sha256 = sha256.map(|h| h.finalize().to_vec()).unwrap_or_default();
    let Some(signature) = signature(&end.record[end.signature_start..end.record.len() - FOOTER]) else {
        return Ok((Some(ZipSignature::Unverified), digest));
    };
    let made_with = |key: &Key| match (&key.public, key.hash) {
        (Public::Rsa(rsa), Hash::Sha1) => rsa.verify(Pkcs1v15Sign::new::<Sha1>(), &sha1, &signature).is_ok(),
        (Public::Rsa(rsa), Hash::Sha256) => rsa.verify(Pkcs1v15Sign::new::<Sha256>(), &sha256, &signature).is_ok(),
        (Public::Ec(ec), Hash::Sha256) => {
            p256::ecdsa::Signature::from_der(&signature).is_ok_and(|s| ec.verify_prehash(&sha256, &s).is_ok())
        }
        // An EC key signs only SHA-256 digests.
        (Public::Ec(_), Hash::Sha1) => false,
    };
    let mut status = ZipSignature::Unverified;
    for key in keys.iter().filter(|key| made_with(key)) {
        if !key.public_test_key {
            status = ZipSignature::Trusted;
            break;
        }
        status = ZipSignature::PublicTest;
    }
    Ok((Some(status), digest))
}

/// The signature in a PKCS#7 signed-data block, its first signer's; none
/// if the block is not one.
fn signature(block: &[u8]) -> Option<Vec<u8>> {
    let info = ContentInfo::from_der(block).ok()?;
    if info.content_type != SIGNED_DATA {
        return None;
    }
    let data: SignedData = info.content.decode_as().ok()?;
    data.signer_infos.0.iter().next().map(|s| s.signature.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{self, Package};
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    /// A zip with an install script signed whole, laid out as `signapk -w`
    /// lays it out, with openssl's CMS signatures, `cms -sign -binary
    /// -noattr`: by the Android source's test key; by a 2048-bit RSA key
    /// with the exponent 3 whose certificate is signed with SHA-1, as the
    /// phone's update key is; and by a P-256 key.
    const SIGNED_TESTKEY: &[u8] = include_bytes!("../tests/signature/signed-testkey.zip");
    const SIGNED_SHA1: &[u8] = include_bytes!("../tests/signature/signed-sha1.zip");
    const SIGNED_EC: &[u8] = include_bytes!("../tests/signature/signed-ec.zip");
    /// Their certificates, in PEM, and that of a 1024-bit RSA key.
    const TESTKEY: &str = include_str!("../tests/signature/testkey.crt");
    const SHA1: &str = include_str!("../tests/signature/sha1.crt");
    const EC: &str = include_str!("../tests/signature/ec.crt");
    const RSA1024: &str = include_str!("../tests/signature/rsa1024.crt");

    /// An otacerts zip of these entries.
    fn certificates(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in entries {
            w.start_file(*name, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)).unwrap();
            w.write_all(text.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn keys_of(pems: &[&str]) -> Vec<Key> {
        let names: Vec<String> = (0..pems.len()).map(|k| format!("keys/{k}.x509.pem")).collect();
        let entries: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(pems.iter().copied()).collect();
        keys(Cursor::new(certificates(&entries))).unwrap()
    }

    fn check(package: &[u8], keys: &[Key]) -> Verdict {
        verify(Cursor::new(package), package.len() as u64, keys, |_, _| true).unwrap()
    }

    /// Where the signature's footer says the comment starts.
    fn comment_start(package: &[u8]) -> usize {
        let n = package.len();
        n - usize::from(u16::from_le_bytes([package[n - 2], package[n - 1]]))
    }

    #[test]
    fn each_key_verifies_the_package_it_signed_and_no_other() {
        let signed = [(SIGNED_TESTKEY, TESTKEY), (SIGNED_SHA1, SHA1), (SIGNED_EC, EC)];
        let all = keys_of(&[TESTKEY, SHA1, EC]);
        for (package, cert) in signed {
            assert_eq!(package::identify(Cursor::new(package)).unwrap(), Package::Script, "a signed zip still installs as one");
            assert_eq!(check(package, &keys_of(&[cert])), Verdict::Signed);
            assert_eq!(check(package, &all), Verdict::Signed);
            for (_, other) in signed.iter().filter(|(_, c)| *c != cert) {
                assert_eq!(check(package, &keys_of(&[other])), Verdict::Unverified);
            }
        }
    }

    #[test]
    fn hashes_the_complete_zip_for_known_unknown_and_unsigned_packages() {
        let keys = keys_of(&[TESTKEY]);
        for (package, verdict) in [
            (SIGNED_TESTKEY, ZipSignature::Trusted),
            (SIGNED_EC, ZipSignature::Unverified),
            (b"unsigned".as_slice(), ZipSignature::Unsigned),
        ] {
            let mut progress = None;
            let checked = verify_and_hash(Cursor::new(package), package.len() as u64, &keys, |done, total| {
                progress = Some((done, total));
                true
            }).unwrap();
            assert_eq!(checked, Some((verdict, Sha256::digest(package).into())));
            assert_eq!(progress, Some((package.len() as u64, package.len() as u64)));
        }
    }

    #[test]
    fn a_valid_public_test_signature_does_not_authenticate_the_publisher() {
        let mut keys = keys_of(&[TESTKEY]);
        keys[0].public_test_key = true;
        let checked = verify_and_hash(Cursor::new(SIGNED_TESTKEY), SIGNED_TESTKEY.len() as u64, &keys, |_, _| true)
            .unwrap()
            .unwrap();
        assert_eq!(checked.0, ZipSignature::PublicTest);
        assert!(checked.0.matches_included_key());
        assert_eq!(check(SIGNED_TESTKEY, &keys), Verdict::Signed, "older paths still report a valid signature");
    }

    #[test]
    fn cancelling_a_full_check_produces_no_digest() {
        let keys = keys_of(&[TESTKEY]);
        assert_eq!(verify_and_hash(Cursor::new(SIGNED_TESTKEY), SIGNED_TESTKEY.len() as u64, &keys, |_, _| false).unwrap(), None);
    }

    #[test]
    fn a_pinned_package_requires_the_exact_zip_including_its_comment() {
        let keys = keys_of(&[TESTKEY]);
        let expected: [u8; 32] = Sha256::digest(SIGNED_TESTKEY).into();
        let verify = |package: &[u8]| verify_pinned(Cursor::new(package), package.len() as u64, &keys, expected, |_, _| true).unwrap();
        assert_eq!(verify(SIGNED_TESTKEY), Verdict::Signed);
        let mut changed = SIGNED_TESTKEY.to_vec();
        let start = comment_start(&changed);
        changed[start] ^= 1;
        assert_eq!(check(&changed, &keys), Verdict::Signed);
        assert_eq!(verify(&changed), Verdict::Unverified);
    }

    #[test]
    fn a_package_changed_after_it_was_signed_is_not_verified_but_its_comments_text_is_not_signed() {
        let keys = keys_of(&[TESTKEY]);
        let mut changed = SIGNED_TESTKEY.to_vec();
        let at = changed.windows(20).position(|w| w == b"Skyward test package").expect("the script's text");
        changed[at] = b's';
        assert_eq!(check(&changed, &keys), Verdict::Unverified);
        let mut renamed = SIGNED_TESTKEY.to_vec();
        let start = comment_start(&renamed);
        renamed[start..start + 17].copy_from_slice(b"signed by someone");
        assert_eq!(check(&renamed, &keys), Verdict::Signed);
    }

    #[test]
    fn a_second_end_of_the_directory_in_the_comment_is_not_verified() {
        // The comment's text is not signed, so only the record turning up
        // there makes the package fail.
        let mut sneaky = SIGNED_TESTKEY.to_vec();
        let start = comment_start(&sneaky);
        sneaky[start..start + 4].copy_from_slice(EOCD_MAGIC);
        assert_eq!(check(&sneaky, &keys_of(&[TESTKEY])), Verdict::Unverified);
    }

    #[test]
    fn a_footer_that_points_out_of_the_comment_is_not_verified() {
        let keys = keys_of(&[TESTKEY]);
        let n = SIGNED_TESTKEY.len();
        for start in [6u16, 0xfff0] {
            let mut moved = SIGNED_TESTKEY.to_vec();
            moved[n - 6..n - 4].copy_from_slice(&start.to_le_bytes());
            assert_eq!(check(&moved, &keys), Verdict::Unverified, "a signature {start} bytes from the end");
        }
        let mut long = SIGNED_TESTKEY.to_vec();
        long[n - 2..].copy_from_slice(&(n as u16).to_le_bytes());
        assert_eq!(check(&long, &keys), Verdict::Unverified, "a comment longer than the zip");
    }

    #[test]
    fn a_zip_without_a_signature_of_all_of_it_is_unsigned() {
        let keys = keys_of(&[TESTKEY]);
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file(package::UPDATE_BINARY, SimpleFileOptions::default()).unwrap();
        w.write_all(b"#!/sbin/sh\n").unwrap();
        let plain = w.finish().unwrap().into_inner();
        assert_eq!(check(&plain, &keys), Verdict::Unsigned);
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.set_comment("made by hand").unwrap();
        w.start_file(package::UPDATE_BINARY, SimpleFileOptions::default()).unwrap();
        let commented = w.finish().unwrap().into_inner();
        assert_eq!(check(&commented, &keys), Verdict::Unsigned);
        assert_eq!(check(b"PK", &keys), Verdict::Unsigned);
    }

    #[test]
    fn the_check_tells_how_much_it_has_read_and_stops_when_told() {
        let keys = keys_of(&[EC]);
        let signed = (comment_start(SIGNED_EC) - 2) as u64;
        let mut heard = Vec::new();
        let verdict = verify(Cursor::new(SIGNED_EC), SIGNED_EC.len() as u64, &keys, |done, total| {
            heard.push((done, total));
            true
        });
        assert_eq!(verdict.unwrap(), Verdict::Signed);
        assert_eq!(heard.last(), Some(&(signed, signed)), "all before the comment's length is read");
        let stopped = verify(Cursor::new(SIGNED_EC), SIGNED_EC.len() as u64, &keys, |_, _| false);
        assert_eq!(stopped.unwrap(), Verdict::Stopped);
    }

    #[test]
    fn only_the_keys_androids_recovery_takes_are_read_from_certificates_named_as_it_names_them() {
        let other = certificates(&[("keys/testkey.x509.pem", TESTKEY), ("README", "not a certificate"), ("keys/ec.pem", EC)]);
        assert_eq!(keys(Cursor::new(other)).unwrap().len(), 1, "only *.x509.pem");
        let short = keys(Cursor::new(certificates(&[("short.x509.pem", RSA1024)]))).err().expect("a 1024-bit key is refused");
        assert!(format!("{short:#}").contains("1024 bits"), "{short:#}");
        assert!(keys(Cursor::new(certificates(&[("README", "")]))).is_err(), "a zip without certificates");
    }
}
