use std::ffi::OsStr;

/// Only list backup folders whose displayed names preserve their filesystem bytes.
pub fn backup_name(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    (!name.is_empty() && !name.starts_with('.') && !name.contains('/') && !name.contains('\0')).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn a_backup_name_must_be_reversible_and_not_hidden() {
        let invalid = OsStr::from_bytes(b"backup-\xff");
        let replacement = format!("backup-{}", char::from_u32(0xfffd).unwrap());
        assert_eq!(backup_name(invalid), None);
        assert_eq!(backup_name(OsStr::new(&replacement)), Some(replacement.as_str()));
        assert_eq!(backup_name(OsStr::new("20261115-123000")), Some("20261115-123000"));
        assert_eq!(backup_name(OsStr::new(".unfinished")), None);
    }
}
