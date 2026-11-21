//! Validate the Android user records that keep shared storage tied to its owner.

use super::{DataRoot, entries, mount_id};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::fgetxattr;
use rustix::io::Errno;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const FLAG_EPHEMERAL: i32 = 0x100;

/// Check the user list Android will read, each listed record, and any existing
/// serial-number attributes on its storage directories.
pub(super) fn check(root: &DataRoot) -> Result<BTreeMap<u32, i32>> {
    let registry = root
        .open_relative(Path::new("system/users"))?
        .context("missing Android user identities")?;
    let users = read_record(root, &registry, "userlist.xml", parse_list)?;
    let mut serials = BTreeMap::new();
    let mut used_serials = BTreeSet::new();
    for &id in &users {
        let record = read_record(root, &registry, &format!("{id}.xml"), |bytes| {
            parse_user(bytes, id)
        })?;
        ensure!(
            used_serials.insert(record),
            "Android users share the same serial number"
        );
        serials.insert(id, record);
    }

    // UserDataPreparer reconciles these directory serials before Android
    // starts a user. A stale serial can cause it to destroy that user's CE
    // and DE storage, including the media that this operation promises to keep.
    if let Some(owner) = root.open_relative(Path::new("data"))? {
        check_serial(&owner, serials[&0])
            .context("owner app data has a different user identity")?;
    }
    for (path, has_serial) in [
        ("media", false),
        ("user", true),
        ("user_de", true),
        ("system_ce", true),
        ("system_de", true),
        ("misc_ce", true),
        ("misc_de", true),
        ("misc/vold/user_keys/ce", false),
        ("misc/vold/user_keys/de", false),
    ] {
        let Some(folder) = root.open_relative(Path::new(path))? else {
            continue;
        };
        for entry in entries(&folder)? {
            let entry = entry?;
            let name = entry.file_name();
            let Ok(text) = name.to_str() else { continue };
            let Ok(id) = text.parse::<i32>() else {
                // Java's integer parser recognizes non-ASCII decimal digits.
                ensure!(
                    !text
                        .trim_start_matches(['+', '-'])
                        .chars()
                        .any(|c| !c.is_ascii() && c.is_numeric()),
                    "unsupported Android user directory name"
                );
                continue;
            };
            ensure!(
                id >= 0 && text == id.to_string(),
                "noncanonical Android user directory name"
            );
            let id = id as u32;
            let expected = serials
                .get(&id)
                .with_context(|| format!("{path}/{id} is missing from the Android user list"))?;
            if path == "user" && id == 0 {
                let entry_type = super::entry_type(&folder, name)?;
                if entry_type == rustix::fs::FileType::Symlink {
                    let target = rustix::fs::readlinkat(&folder, name, Vec::new())?;
                    let base = std::fs::read_link(format!(
                        "/proc/self/fd/{}",
                        root.directory.as_raw_fd()
                    ))?;
                    let target = Path::new(OsStr::from_bytes(target.as_bytes()));
                    ensure!(
                        target == Path::new("../data") || target == base.join("data"),
                        "invalid Android owner directory link"
                    );
                    continue;
                }
                if root.is_owner_app_alias(&folder, OsStr::new(text))? {
                    continue;
                }
            }
            let directory = root
                .open_child(&folder, OsStr::new(text))?
                .with_context(|| format!("{path}/{id} is not a user directory"))?;
            if has_serial {
                check_serial(&directory, *expected)
                    .with_context(|| format!("{path}/{id} has a different user identity"))?;
            }
        }
    }

    Ok(serials)
}

fn check_serial(directory: &File, serial: i32) -> Result<()> {
    let mut buffer = [0u8; 16];
    let len = match fgetxattr(directory, "user.serial", &mut buffer[..]) {
        Ok(len) => len,
        // Android initializes the serial on a directory without this attribute.
        Err(Errno::NODATA) => return Ok(()),
        Err(error) => return Err(error).context("read the Android user serial"),
    };
    let found: i32 = std::str::from_utf8(&buffer[..len])?
        .parse()
        .context("invalid Android user serial")?;
    ensure!(found == serial, "Android user serial numbers disagree");
    Ok(())
}

fn read_record<T>(
    root: &DataRoot,
    parent: &File,
    file: &str,
    parse: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    // AOSP ResilientAtomicFile opens the interrupted-write backup before the
    // primary file, then uses the reserve copy only if neither exists.
    for suffix in [".backup", "", ".reservecopy"] {
        let name = format!("{file}{suffix}");
        match crate::storage::open_regular(parent, OsStr::new(&name)) {
            Ok(opened) => {
                ensure!(
                    mount_id(&opened)? == root.mount,
                    "Android user record is on a different filesystem"
                );
                let mut bytes = Vec::new();
                opened
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("read Android user record {name}"))?;
                ensure!(
                    bytes.len() as u64 <= MAX_FILE_BYTES,
                    "Android user record {name} is too large"
                );
                return parse(&bytes)
                    .with_context(|| format!("invalid Android user record {name}"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("open Android user record {name}"));
            }
        }
    }
    bail!("missing Android user record {file}")
}

fn parse_list(bytes: &[u8]) -> Result<BTreeSet<u32>> {
    let mut ids = BTreeSet::new();
    visit_xml(bytes, 16_384, |event| {
        let Xml::Start(depth, name, attrs) = event else {
            return Ok(());
        };
        if depth == 0 {
            ensure!(name == b"users", "expected Android's users list");
            for key in [
                b"nextSerialNumber".as_slice(),
                b"version",
                b"userTypeConfigVersion",
            ] {
                integer(attrs, key)?;
            }
        } else if name == b"user" {
            ensure!(depth == 1, "unexpected nested Android user record");
            let id = u32::try_from(integer(attrs, b"id")?.context("missing Android user ID")?)?;
            ensure!(ids.insert(id), "duplicate Android user ID");
        } else if name == b"restrictions" {
            restrictions(attrs)?;
        }
        Ok(())
    })?;
    ensure!(ids.contains(&0), "Android user list has no system user");
    Ok(ids)
}

fn parse_user(bytes: &[u8], expected: u32) -> Result<i32> {
    let mut serial = None;
    let mut checked_type = false;
    let mut quiet_time = Vec::new();
    visit_xml(bytes, 16_384, |event| {
        let (depth, name, attrs) = match event {
            Xml::Start(depth, name, attrs) => (depth, name, attrs),
            Xml::Text(b"lastRequestQuietModeEnabledCall", text) => {
                ensure!(
                    quiet_time.len() + text.len() <= 20,
                    "invalid Android user quiet-mode timestamp"
                );
                quiet_time.extend_from_slice(text);
                return Ok(());
            }
            Xml::Text(_, _) => return Ok(()),
        };
        if depth == 0 {
            ensure!(name == b"user", "expected Android's user record");
            let id = integer(attrs, b"id")?.context("missing Android user ID")?;
            ensure!(
                u32::try_from(id)? == expected,
                "Android user ID does not match its record"
            );
            let found = integer(attrs, b"serialNumber")?.unwrap_or(id);
            ensure!(found >= 0, "invalid Android user serial");
            let flags = integer(attrs, b"flags")?.unwrap_or(0);
            ensure!(
                flags & FLAG_EPHEMERAL == 0
                    && !boolean(attrs, b"partial")?
                    && !boolean(attrs, b"preCreated")?
                    && !boolean(attrs, b"guestToRemove")?,
                "Android user is temporary or scheduled for removal"
            );
            boolean(attrs, b"convertedFromPreCreated")?;
            for key in [
                b"profileGroupId".as_slice(),
                b"profileBadge",
                b"restrictedProfileParentId",
            ] {
                integer(attrs, key)?;
            }
            for key in [
                b"created".as_slice(),
                b"lastLoggedIn",
                b"lastEnteredForeground",
            ] {
                signed_long(attrs, key)?;
            }
            if let Some(kind) = value(attrs, b"type") {
                ensure!(
                    matches!(
                        kind,
                        Value::Text(
                            b"android.os.usertype.full.SYSTEM"
                                | b"android.os.usertype.system.HEADLESS"
                                | b"android.os.usertype.full.SECONDARY"
                                | b"android.os.usertype.full.RESTRICTED"
                                | b"android.os.usertype.full.GUEST"
                                | b"android.os.usertype.full.DEMO"
                                | b"android.os.usertype.profile.MANAGED"
                                | b"android.os.usertype.profile.CLONE"
                                | b"android.os.usertype.profile.PRIVATE"
                                | b"android.os.usertype.profile.COMMUNAL"
                        )
                    ),
                    "unsupported Android user type"
                );
                checked_type = true;
            }
            serial = Some(found);
        } else {
            match name {
                b"userProperties" => {
                    ensure!(
                        depth == 1 && checked_type,
                        "Android user properties require a known user type"
                    );
                    properties(attrs)?;
                }
                b"restrictions"
                | b"device_policy_restrictions"
                | b"device_policy_local_restrictions"
                | b"device_policy_global_restrictions" => restrictions(attrs)?,
                b"seedAccountOptions" => bail!(
                    "Android user contains provisioning options outside the supported record layout"
                ),
                _ => {}
            }
        }
        Ok(())
    })?;
    if !quiet_time.is_empty() {
        std::str::from_utf8(&quiet_time)?
            .parse::<i64>()
            .context("invalid Android user quiet-mode timestamp")?;
    }
    serial.context("no Android user identity")
}

fn restrictions(attrs: &Attrs<'_>) -> Result<()> {
    for (name, _) in attrs {
        boolean(attrs, name)?;
    }
    Ok(())
}

fn properties(attrs: &Attrs<'_>) -> Result<()> {
    for (name, _) in attrs {
        match *name {
            b"showInLauncher"
            | b"showInSettings"
            | b"showInQuietMode"
            | b"showInSharingSurfaces"
            | b"inheritDevicePolicy"
            | b"crossProfileIntentFilterAccessControl"
            | b"crossProfileIntentResolutionStrategy"
            | b"crossProfileContentSharingStrategy"
            | b"profileApiVisibility" => {
                integer(attrs, name)?;
            }
            b"startWithParent"
            | b"useParentsContacts"
            | b"updateCrossProfileIntentFiltersOnOTA"
            | b"mediaSharedWithParent"
            | b"credentialShareableWithParent"
            | b"authAlwaysRequiredToDisableQuietMode"
            | b"allowStoppingUserWithDelayedLocking"
            | b"deleteAppWithParent"
            | b"alwaysVisible"
            | b"itemsRestrictedOnHomeScreen" => {
                boolean(attrs, name)?;
            }
            // AOSP ignores properties it does not use.
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum Value<'a> {
    Integer(i32),
    Long,
    Boolean(bool),
    Text(&'a [u8]),
    Other,
}

pub(super) type Attrs<'a> = [(&'a [u8], Value<'a>)];

pub(super) fn value<'a>(attrs: &Attrs<'a>, name: &[u8]) -> Option<Value<'a>> {
    attrs
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

pub(super) fn integer(attrs: &Attrs<'_>, name: &[u8]) -> Result<Option<i32>> {
    match value(attrs, name) {
        None => Ok(None),
        Some(Value::Integer(number)) => Ok(Some(number)),
        Some(Value::Text(text)) => Ok(Some(
            std::str::from_utf8(text)?
                .parse()
                .context("invalid integer in Android user record")?,
        )),
        _ => bail!("expected an integer in Android user record"),
    }
}

fn signed_long(attrs: &Attrs<'_>, name: &[u8]) -> Result<()> {
    match value(attrs, name) {
        None | Some(Value::Long) => Ok(()),
        Some(Value::Text(text)) => {
            std::str::from_utf8(text)?
                .parse::<i64>()
                .context("invalid long in Android user record")?;
            Ok(())
        }
        _ => bail!("expected a long in Android user record"),
    }
}

fn boolean(attrs: &Attrs<'_>, name: &[u8]) -> Result<bool> {
    match value(attrs, name) {
        None => Ok(false),
        Some(Value::Boolean(flag)) => Ok(flag),
        Some(Value::Text(text)) if text.eq_ignore_ascii_case(b"true") => Ok(true),
        Some(Value::Text(text)) if text.eq_ignore_ascii_case(b"false") => Ok(false),
        _ => bail!("expected a boolean in Android user record"),
    }
}

pub(super) enum Xml<'a> {
    Start(usize, &'a [u8], &'a Attrs<'a>),
    Text(&'a [u8], &'a [u8]),
}

pub(super) fn visit_xml(bytes: &[u8], nodes_limit: u32, mut visit: impl FnMut(Xml<'_>) -> Result<()>) -> Result<()> {
    if let Some(body) = bytes.strip_prefix(b"ABX\x00") {
        return Abx {
            body,
            strings: Vec::new(),
        }
        .scan(visit);
    }
    let text = std::str::from_utf8(bytes).context("unsupported Android user record encoding")?;
    let options = roxmltree::ParsingOptions {
        nodes_limit,
        ..Default::default()
    };
    // The parser error may quote private text from the record.
    let doc = roxmltree::Document::parse_with_options(text, options)
        .map_err(|_| anyhow::anyhow!("malformed XML"))?;
    for node in doc.descendants() {
        if let Some(text) = node.text().filter(|_| node.is_text()) {
            if let Some(parent) = node.parent().filter(roxmltree::Node::is_element) {
                visit(Xml::Text(
                    parent.tag_name().name().as_bytes(),
                    text.as_bytes(),
                ))?;
            }
        } else if node.is_element() {
            let depth = node
                .ancestors()
                .take_while(roxmltree::Node::is_element)
                .count()
                - 1;
            ensure!(
                depth < 64 && node.tag_name().namespace().is_none(),
                "unsupported Android XML element"
            );
            ensure!(
                node.attributes().len() <= 128,
                "too many Android XML attributes"
            );
            let attrs: Vec<_> = node
                .attributes()
                .filter(|attr| attr.namespace().is_none())
                .map(|attr| (attr.name().as_bytes(), Value::Text(attr.value().as_bytes())))
                .collect();
            visit(Xml::Start(depth, node.tag_name().name().as_bytes(), &attrs))?;
        }
    }
    Ok(())
}

// Android Binary XML v0 interleaves XML events with typed attributes. Names
// use the stream's intern table; lengths and integer values are big-endian.
struct Abx<'a> {
    body: &'a [u8],
    strings: Vec<&'a [u8]>,
}

impl<'a> Abx<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let (head, tail) = self
            .body
            .split_at_checked(count)
            .context("truncated Android binary XML")?;
        self.body = tail;
        Ok(head)
    }

    fn length(&mut self) -> Result<usize> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?) as usize)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.length()?;
        self.take(len)
    }

    fn interned(&mut self) -> Result<&'a [u8]> {
        let index = self.length()?;
        if index == u16::MAX as usize {
            let fresh = self.bytes()?;
            if self.strings.len() < index {
                self.strings.push(fresh);
            }
            return Ok(fresh);
        }
        self.strings
            .get(index)
            .copied()
            .context("invalid Android XML string reference")
    }

    fn name(&mut self) -> Result<&'a [u8]> {
        let name = self.interned()?;
        ensure!(
            name.first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                && name
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(b)),
            "invalid Android XML element or attribute name"
        );
        Ok(name)
    }

    fn attribute(&mut self, kind: u8) -> Result<Value<'a>> {
        Ok(match kind {
            0x10 => Value::Other,
            0x20 => Value::Text(self.bytes()?),
            0x30 => Value::Text(self.interned()?),
            0x40 | 0x50 => {
                self.bytes()?;
                Value::Other
            }
            0x60 | 0x70 => Value::Integer(i32::from_be_bytes(self.take(4)?.try_into()?)),
            0x80 | 0x90 => {
                self.take(8)?;
                Value::Long
            }
            0xb0 => {
                self.take(8)?;
                Value::Other
            }
            0xa0 => {
                self.take(4)?;
                Value::Other
            }
            0xc0 => Value::Boolean(true),
            0xd0 => Value::Boolean(false),
            _ => bail!("unknown Android XML attribute type"),
        })
    }

    fn scan(mut self, mut visit: impl FnMut(Xml<'_>) -> Result<()>) -> Result<()> {
        let mut stack = Vec::new();
        let mut seen_root = false;
        if self.body.first() == Some(&0x10) {
            self.take(1)?;
        }
        while let Some(&event) = self.body.first() {
            self.take(1)?;
            match event {
                0x32 => {
                    ensure!(
                        stack.len() < 64 && (!stack.is_empty() || !seen_root),
                        "invalid Android XML nesting"
                    );
                    let name = self.name()?;
                    let mut attrs = Vec::new();
                    while let Some(&attr) = self.body.first().filter(|&&token| token & 0x0f == 0x0f)
                    {
                        self.take(1)?;
                        let name = self.name()?;
                        ensure!(
                            attrs.len() < 128 && !attrs.iter().any(|(key, _)| *key == name),
                            "invalid Android XML attributes"
                        );
                        let value = self.attribute(attr & 0xf0)?;
                        attrs.push((name, value));
                    }
                    visit(Xml::Start(stack.len(), name, &attrs))?;
                    stack.push(name);
                    seen_root = true;
                }
                0x33 => ensure!(
                    stack.pop() == Some(self.name()?),
                    "unmatched Android XML end tag"
                ),
                // Text, CDATA, whitespace, processing instruction, comment.
                0x24 | 0x25 | 0x27 | 0x28 | 0x29 => {
                    let text = self.bytes()?;
                    ensure!(
                        !stack.is_empty()
                            || matches!(event, 0x28 | 0x29)
                            || text.iter().all(u8::is_ascii_whitespace),
                        "content outside Android XML root"
                    );
                    if let Some(parent) = stack.last()
                        && matches!(event, 0x24 | 0x25 | 0x27)
                    {
                        visit(Xml::Text(parent, text))?;
                    }
                }
                0x11 => {
                    ensure!(
                        seen_root && stack.is_empty() && self.body.is_empty(),
                        "incomplete Android binary XML document"
                    );
                    return Ok(());
                }
                _ => bail!("unknown or misplaced Android XML event"),
            }
        }
        bail!("Android binary XML document has no end marker")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_registry_uses_integer_attribute_values_and_interned_names() {
        const USERS: &[u8] = b"ABX\x00\x10\
            \x32\xff\xff\x00\x05users\
            \x32\xff\xff\x00\x04user\
            \x6f\xff\xff\x00\x02id\x00\x00\x00\x00\
            \x33\x00\x01\
            \x32\x00\x01\x6f\x00\x02\x00\x00\x00\x0a\x33\x00\x01\
            \x33\x00\x00\x11";
        assert_eq!(parse_list(USERS).unwrap(), BTreeSet::from([0, 10]));
        for len in 0..USERS.len() {
            assert!(
                parse_list(&USERS[..len]).is_err(),
                "accepted a truncated record at {len}"
            );
        }
    }

    #[test]
    fn binary_user_record_reads_serial_without_exposing_a_display_name() {
        const USER: &[u8] = b"ABX\x00\x10\
            \x32\xff\xff\x00\x04user\
            \x6f\xff\xff\x00\x02id\x00\x00\x00\x0a\
            \x6f\xff\xff\x00\x0cserialNumber\x00\x00\x00\x2f\
            \xdf\xff\xff\x00\x07partial\
            \x32\xff\xff\x00\x04name\
            \x24\x00\x04\xf0\x9f\x93\xb7\
            \x33\x00\x04\x33\x00\x00\x11";
        assert_eq!(parse_user(USER, 10).unwrap(), 47);
        assert!(parse_user(USER, 11).is_err());
        let mut partial = USER.to_vec();
        let flag = partial.iter_mut().find(|byte| **byte == 0xdf).unwrap();
        *flag = 0xcf;
        assert!(parse_user(&partial, 10).is_err());
    }
}
