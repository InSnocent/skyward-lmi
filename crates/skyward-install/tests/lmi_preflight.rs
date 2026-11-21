use skyward_install::{lmi_zip, signature};
use std::{env, fs::File, io::BufReader};

fn input(name: &str) -> File {
    let path = env::var_os(name).unwrap_or_else(|| panic!("set {name} to a local test input"));
    File::open(&path).unwrap_or_else(|e| panic!("open {name}: {e}"))
}

fn key_archive(name: &str) -> Vec<signature::Key> {
    signature::keys(BufReader::new(input(name))).unwrap()
}

const INFINITY_SHA256: &str = "9cf66b7fa39e1f5afd2b22dfdb13739bbba6140c9456ab659e1df2434933843e";
const LINEAGE_SHA256: &str = "ac6c2501fc16d5e5436416425bd727dd862c55c366a8633bad2b98806e22ae21";

fn check_rom(name: &str, expected_sha256: &str, expected_signature: signature::ZipSignature, keys: &[signature::Key]) {
    let file = input(name);
    let size = file.metadata().unwrap().len();
    let (signature, sha256) = signature::verify_and_hash(BufReader::new(file), size, keys, |_, _| true)
        .unwrap()
        .unwrap();
    assert_eq!(signature, expected_signature, "{name}");
    let kind = lmi_zip::inspect(BufReader::new(input(name)), "lmi").unwrap();
    let lmi_zip::Kind::Block(declaration) = &kind else {
        panic!("{name} must be a full block OTA");
    };
    assert_eq!(declaration.device, "lmi");
    let mut logical = declaration.logical.clone();
    logical.sort_unstable();
    assert_eq!(logical, ["odm", "product", "system", "system_ext", "vendor"]);
    let mut physical = declaration.physical.clone();
    physical.sort_unstable();
    assert_eq!(physical, ["boot", "dtbo", "vbmeta", "vbmeta_system"]);
    let report = lmi_zip::Report { kind, sha256, signature };
    assert_eq!(report.sha256_hex(), expected_sha256);
    eprintln!("{name}: {signature:?}, {} logical and {} physical targets, {expected_sha256}", logical.len(), physical.len());
}

// Real ROMs are not repository fixtures. See docs/lmi.zh-CN.md for inputs.
// This test never runs anything from a ZIP.
#[test]
#[ignore = "requires local lmi ROM packages and independently verified OTA keys"]
fn official_lineage_and_infinity_x_pass_read_only_preflight() {
    let keys = key_archive("SKYWARD_LMI_BASE_CERTS");
    check_rom("SKYWARD_LMI_INFINITY_ZIP", INFINITY_SHA256, signature::ZipSignature::PublicTest, &keys);
    check_rom("SKYWARD_LMI_LOS_ZIP", LINEAGE_SHA256, signature::ZipSignature::Trusted, &keys);

    // v1.2.0 combined the original certificates with the Infinity-X digest.
    // This reproduces its error with the exact same LineageOS ZIP and keys.
    let legacy_digest = std::array::from_fn(|i| u8::from_str_radix(&INFINITY_SHA256[2 * i..2 * i + 2], 16).unwrap());
    let lineage = input("SKYWARD_LMI_LOS_ZIP");
    let size = lineage.metadata().unwrap().len();
    let legacy = signature::verify_pinned(BufReader::new(lineage), size, &keys, legacy_digest, |_, _| true).unwrap();
    assert_eq!(legacy, signature::Verdict::Unverified);
    eprintln!("v1.2.0 pin reproduced: official LineageOS is rejected with the Infinity-X digest");
}
