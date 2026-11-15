use anyhow::{ensure, Context, Result};
use rustix::fs::{self as at, Mode, OFlags};
use rustix::io::Errno;
use skyward_files::Ownership;
use skyward_ui::app::{FileKind, FileRow};
use skyward_ui::wallpaper::{Wallpaper, MIN_OPACITY};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::{ffi::OsStr, io::ErrorKind};

#[cfg(target_os = "android")]
pub const OWNER_STORAGE: &str = "/data/media/0";
const SKYWARD: &str = "Skyward";
const NAME: &str = ".wallpaper";
const FROSTED_CARDS: &str = ".frosted-cards";
const MAGIC: &[u8; 4] = b"SWP1";
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SAVED_BYTES: u64 = 16 * 1024 * 1024;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub fn list(root: &Path, folder: &str) -> Result<Vec<FileRow>> {
    let directory = open_folder(root, Path::new(folder))?;
    let mut images = Vec::new();
    for entry in fs::read_dir(fd_path(&directory)).context("list the picture folder")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("read the picture folder's entry"),
        };
        let kind = if meta.is_dir() {
            FileKind::Folder { items: None }
        } else if meta.is_file() && is_picture(Path::new(name)) {
            FileKind::File
        } else {
            continue;
        };
        images.push(FileRow {
            name: name.into(),
            kind,
            size: meta.len(),
            modified: meta.mtime(),
            mode: meta.mode(),
            uid: meta.uid(),
            gid: meta.gid(),
        });
    }
    images.sort_by_cached_key(|row| (row.kind == FileKind::File, row.name.to_lowercase()));
    Ok(images)
}

pub fn import(
    root: &Path,
    relative: &str,
    opacity: u8,
    size: (u32, u32),
    ownership: Ownership,
) -> Result<Wallpaper> {
    let path = Path::new(relative);
    ensure!(is_picture(path), "choose a JPG or PNG picture");
    let name = path.file_name().context("the picture has no name")?;
    let folder = open_folder(
        root,
        path.parent().context("the picture has no parent folder")?,
    )?;
    let file = open_regular(&folder, name).context("open the selected picture")?;
    ensure!(
        file.metadata()?.len() <= MAX_SOURCE_BYTES,
        "the selected picture is larger than 32 MiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read the selected picture")?;
    ensure!(
        bytes.len() as u64 <= MAX_SOURCE_BYTES,
        "the selected picture is larger than 32 MiB"
    );
    let image = Wallpaper::decode(Cursor::new(bytes), size.0, size.1)?;

    // Decode fully before creating or changing anything under Skyward.
    let saved = open_store(root, Some(ownership))?.context("no Skyward folder")?;
    replace(&saved, NAME, ownership, |to| {
        write_header(to, opacity)?;
        image.write_png(to)
    })?;
    Ok(image)
}

pub fn load(root: &Path, size: (u32, u32)) -> Result<Option<(Wallpaper, u8)>> {
    let Some(folder) = open_store(root, None)? else {
        return Ok(None);
    };
    let Some(mut file) = open_saved(&folder)? else {
        return Ok(None);
    };
    let opacity = read_header(&mut file)?;
    let image = Wallpaper::decode(BufReader::new(file), size.0, size.1)?;
    Ok(Some((image, opacity)))
}

pub fn save_opacity(root: &Path, opacity: u8, ownership: Ownership) -> Result<()> {
    let folder = open_store(root, None)?.context("there is no wallpaper to save")?;
    let mut current = open_saved(&folder)?.context("there is no wallpaper to save")?;
    if read_header(&mut current)? == opacity {
        return Ok(());
    }
    replace(&folder, NAME, ownership, |to| {
        write_header(to, opacity)?;
        io::copy(&mut current.take(MAX_SAVED_BYTES + 1), to)?;
        Ok(())
    })
}

pub fn load_frosted_cards(root: &Path) -> Result<bool> {
    let Some(folder) = open_store(root, None)? else {
        return Ok(false);
    };
    let file = match open_regular(&folder, OsStr::new(FROSTED_CARDS)) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("open the frosted cards setting"),
    };
    let mut setting = Vec::new();
    file.take(2)
        .read_to_end(&mut setting)
        .context("read the frosted cards setting")?;
    match setting.as_slice() {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => anyhow::bail!("invalid frosted cards setting"),
    }
}

pub fn save_frosted_cards(root: &Path, enabled: bool, ownership: Ownership) -> Result<()> {
    let folder = open_store(root, Some(ownership))?.context("no Skyward folder")?;
    replace(&folder, FROSTED_CARDS, ownership, |to| {
        to.write_all(&[u8::from(enabled)])?;
        Ok(())
    })
}

pub fn reset(root: &Path) -> Result<()> {
    let Some(folder) = open_store(root, None)? else {
        return Ok(());
    };
    match at::unlinkat(&folder, NAME, at::AtFlags::empty()) {
        Ok(()) => folder.sync_all().context("sync the wallpaper folder"),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error).context("remove the saved wallpaper"),
    }
}

fn is_picture(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        ["jpg", "jpeg", "png"]
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known))
    })
}

fn open_folder(root: &Path, relative: &Path) -> Result<File> {
    let mut current =
        File::from(at::open(root, DIRECTORY, Mode::empty()).context("open internal storage")?);
    for part in relative.components() {
        let Component::Normal(name) = part else {
            anyhow::bail!("the picture path leaves internal storage")
        };
        current = File::from(
            at::openat(&current, name, DIRECTORY, Mode::empty())
                .context("open a picture folder")?,
        );
    }
    Ok(current)
}

fn open_regular(folder: &File, name: &OsStr) -> io::Result<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = File::from(at::openat(folder, name, flags, Mode::empty())?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(ErrorKind::InvalidData, "not a regular file"));
    }
    Ok(file)
}

fn fd_path(directory: &File) -> PathBuf {
    PathBuf::from("/proc/self/fd").join(directory.as_raw_fd().to_string())
}

fn open_store(root: &Path, create: Option<Ownership>) -> Result<Option<File>> {
    let root = open_folder(root, Path::new(""))?;
    let open = || at::openat(&root, SKYWARD, DIRECTORY, Mode::empty());
    match open() {
        Ok(folder) => Ok(Some(folder.into())),
        Err(Errno::NOENT) => {
            let Some(ownership) = create else {
                return Ok(None);
            };
            skyward_files::make_folder(&fd_path(&root).join(SKYWARD), ownership)
                .context("create the Skyward folder")?;
            root.sync_all().context("sync internal storage")?;
            Ok(Some(File::from(
                open().context("open the new Skyward folder")?,
            )))
        }
        Err(error) => Err(error).context("open the Skyward folder"),
    }
}

fn open_saved(folder: &File) -> Result<Option<File>> {
    match open_regular(folder, OsStr::new(NAME)) {
        Ok(file) => {
            ensure!(
                file.metadata()?.len() <= MAX_SAVED_BYTES,
                "the saved wallpaper is too large"
            );
            Ok(Some(file))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("open the saved wallpaper"),
    }
}

fn read_header(file: &mut File) -> Result<u8> {
    let mut header = [0; 5];
    file.read_exact(&mut header)
        .context("read the saved wallpaper settings")?;
    ensure!(
        header[..4] == *MAGIC && header[4] <= 100,
        "invalid saved wallpaper settings"
    );
    Ok(header[4].max(MIN_OPACITY))
}

fn write_header(to: &mut dyn Write, opacity: u8) -> Result<()> {
    ensure!(opacity <= 100, "invalid wallpaper opacity");
    to.write_all(MAGIC)?;
    to.write_all(&[opacity.max(MIN_OPACITY)])?;
    Ok(())
}

fn replace(
    folder: &File,
    name: &str,
    ownership: Ownership,
    write: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    let path = fd_path(folder);
    let mut pending = tempfile::Builder::new()
        .prefix(name)
        .suffix(".tmp")
        .make_in(&path, |candidate| {
            skyward_files::make_file(candidate, ownership).map_err(io::Error::other)
        })
        .context("create a temporary appearance file")?;
    {
        let mut output = BufWriter::new(pending.as_file_mut());
        write(&mut output)?;
        output.flush().context("write the appearance file")?;
    }
    ensure!(
        pending.as_file().metadata()?.len() <= MAX_SAVED_BYTES,
        "the appearance file is too large to save"
    );
    pending
        .as_file()
        .sync_all()
        .context("sync the appearance file")?;
    pending
        .persist(path.join(name))
        .context("replace the appearance file")?;
    folder.sync_all().context("sync the wallpaper folder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyward_ui::{Canvas, Rgba};
    use std::os::unix::fs::symlink;

    fn picture(root: &Path, name: &str) {
        let canvas = Canvas::filled(12, 8, Rgba::rgb(230, 60, 130));
        canvas
            .write_png(File::create(root.join(name)).unwrap())
            .unwrap();
    }

    #[test]
    fn wallpaper_picker_lists_folders_and_images_and_stays_inside_shared_storage() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        std::fs::create_dir(root.join("DCIM")).unwrap();
        picture(root, "photo.PNG");
        std::fs::write(root.join("rom.zip"), b"zip").unwrap();
        symlink(root.join("DCIM"), root.join("link")).unwrap();
        assert_eq!(
            list(root, "")
                .unwrap()
                .into_iter()
                .map(|row| row.name)
                .collect::<Vec<_>>(),
            ["DCIM", "photo.PNG"]
        );
        assert!(list(root, "link").is_err());
        assert!(list(root, "../").is_err());
        symlink(root.join("photo.PNG"), root.join("link.png")).unwrap();
        assert!(import(root, "link.png", 35, (8, 16), Ownership::Kept).is_err());
    }

    #[test]
    fn wallpaper_survives_a_restart_without_its_source_and_keeps_the_new_opacity() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        picture(root, "photo.png");
        let first = import(root, "photo.png", 35, (8, 16), Ownership::Kept).unwrap();
        std::fs::remove_file(root.join("photo.png")).unwrap();
        save_opacity(root, MIN_OPACITY, Ownership::Kept).unwrap();
        assert_eq!(load(root, (8, 16)).unwrap(), Some((first, MIN_OPACITY)));
        reset(root).unwrap();
        assert!(load(root, (8, 16)).unwrap().is_none());
    }

    #[test]
    fn wallpaper_loads_an_older_zero_percent_record_at_the_current_minimum() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        picture(root, "photo.png");
        let old = import(root, "photo.png", 35, (8, 16), Ownership::Kept).unwrap();
        let saved = root.join(SKYWARD).join(NAME);
        let mut bytes = fs::read(&saved).unwrap();
        bytes[4] = 0;
        fs::write(&saved, bytes).unwrap();
        assert_eq!(load(root, (8, 16)).unwrap(), Some((old, MIN_OPACITY)));
    }

    #[test]
    fn frosted_cards_setting_survives_reloading_and_preserves_existing_wallpaper() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        picture(root, "photo.png");
        let first = import(root, "photo.png", 35, (8, 16), Ownership::Kept).unwrap();
        let saved = root.join(SKYWARD).join(NAME);
        let original = fs::read(&saved).unwrap();

        assert!(!load_frosted_cards(root).unwrap());
        save_frosted_cards(root, true, Ownership::Kept).unwrap();
        assert!(load_frosted_cards(root).unwrap());
        assert_eq!(fs::read(&saved).unwrap(), original);
        assert_eq!(load(root, (8, 16)).unwrap(), Some((first.clone(), 35)));

        save_opacity(root, MIN_OPACITY, Ownership::Kept).unwrap();
        assert_eq!(load(root, (8, 16)).unwrap(), Some((first, MIN_OPACITY)));
        assert!(load_frosted_cards(root).unwrap());
        save_frosted_cards(root, false, Ownership::Kept).unwrap();
        assert!(!load_frosted_cards(root).unwrap());
    }

    #[test]
    fn wallpaper_keeps_the_current_image_when_a_replacement_is_invalid() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        picture(root, "first.png");
        let first = import(root, "first.png", 35, (8, 16), Ownership::Kept).unwrap();
        std::fs::write(root.join("broken.jpg"), b"\xff\xd8\xffbroken").unwrap();
        assert!(import(root, "broken.jpg", 35, (8, 16), Ownership::Kept).is_err());
        assert_eq!(load(root, (8, 16)).unwrap(), Some((first, 35)));
    }
}
