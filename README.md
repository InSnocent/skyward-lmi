![Skyward Recovery](assets/hero.jpg)

# Skyward Recovery for Redmi K30 Pro

English | [简体中文](README.zh-CN.md)

An unofficial preview of Skyward Recovery for the Redmi K30 Pro (`lmi`). This port builds on Skyward, the Android recovery written in Rust. The features below describe the current source; previously released or installed images may not include the latest changes. [Upstream Skyward](https://github.com/kirklin/skyward)

## Screenshots

The Home screen, wallpaper settings and frosted cards in dark mode, rendered on a computer with a sample wallpaper.

<p align="center">
  <img src="assets/lmi/home.png" width="30%" alt="Skyward home on the Redmi K30 Pro">
  <img src="assets/lmi/wallpaper.png" width="30%" alt="Wallpaper import and opacity settings">
  <img src="assets/lmi/frosted-dark.png" width="30%" alt="Appearance settings with frosted cards in dark mode">
</p>

## Features

### Decryption

Enter the Android lock-screen PIN on the Decrypt page to unlock internal storage. PIN decryption has been tested on the ROM build listed below.

### Installation preflight

The **ADB sideload preflight** verifies the matching Infinity-X update ZIP's signature and SHA-256, then lists the target partitions. The same ZIP can also be selected from decrypted internal storage: validation through the confirmation screen and a non-writing mock package have been tested on-device, while actual local ROM installation has not. Reinstalling that exact ROM over ADB sideload was tested with an earlier installer build; root was lost and the original Recovery returned afterward. [Sideload and reinstall details](docs/lmi.zh-CN.md#临时启动与同版重装)

### Backup and restore

Back up and verify the supported contents of `/data`. Backups appear on the Restore page. Shared storage, including photos and downloads, needs a separate backup. Restore has been tested with a sample app. Restoring all of `/data` is still awaiting device validation. [Backup details](docs/lmi.zh-CN.md#数据备份)

### Wallpaper and appearance

After decryption, choose a JPG or PNG from internal storage. Drag and zoom the picture to frame it for the screen. The dimmed part outside the frame provides a reference as you adjust the crop. Tap **Apply wallpaper** to save. Wallpaper opacity ranges from 5% to 100%. The **Frosted cards** switch applies to the main pages in both themes. Skyward loads the saved wallpaper and switch setting after decryption.

### Other features

- Basic preferences save to `/metadata/skyward/settings.json` and load before data decryption. The home page provides a **Language / 语言** shortcut.
- Touch, screen brightness and vibration control.
- Read-only inspection of the system's logical partitions.
- Reboot to Android from the Recovery menu.
- Browse decrypted internal storage in the file manager; rename and copy have been tested with a sample file. The terminal, USB file transfer (MTP), log export and screenshots have received basic device testing.

## Current limitations

Some upstream menu entries still appear. An unavailable action displays “Unavailable in the UI preview.”

- **Advanced tools.** Other file manager write operations and large file sets need further testing. Names containing non-UTF-8 bytes are shown but cannot be operated on in the GUI; MTP omits names with non-UTF-8 bytes or longer than its protocol can represent. MTP temporarily withdraws internal storage during file writes, text editing and terminal jobs. Background processes that deliberately leave the terminal session cannot be fully tracked; ensure they have stopped before backup, restore or installation. SELinux relabeling is not yet adapted.
- **Maintenance and reboot.** Data wipe, factory reset, filesystem checks, repair and formatting are pending. The Mount page shows the logical system partitions read-only. The Reboot menu can return to Android.
- **Installation.** ADB sideload and internal storage accept only the matching full Infinity-X ZIP. Actual installation from internal storage has not been tested. General ZIP or IMG flashing, installation from a USB drive, online updates and dual boot are not yet adapted.
- **Backup and restore.** Backups of `/data` go to internal storage; backup directories containing non-UTF-8 bytes are not listed. A full `/data` restore and migration from another ROM still need device testing. Skyward refuses to restore backups tied to the old encryption key after a factory reset.
- **Settings and wallpaper.** Older preferences in shared storage migrate after the first unlock. Wallpaper, opacity and frosted cards load after internal storage is decrypted.

## Supported device

| Device | Codename | Tested ROM | Build |
| --- | --- | --- | --- |
| Redmi K30 Pro | `lmi` | Unofficial Infinity-X 3.12 (Android 16) | `1793266721` |

Device testing covers this model and ROM build. [Matching Infinity-X build](https://github.com/InSnocent/infinityx-lmi)

## Installing Skyward

Use an unlocked bootloader and Android SDK Platform-Tools. Keep the original Infinity-X Recovery image from the same ROM build so you can roll back. Verify the Skyward image against its published SHA-256 or the checksum generated when you built it. Substitute your image's filename for `skyward-lmi.img` in these commands. [Recorded SHA-256 checksums](docs/lmi.zh-CN.md#sha-256)

### Temporary boot

Enter bootloader Fastboot and check the product codename.

```sh
fastboot getvar product
```

On a Redmi K30 Pro that reports `lmi`, start the image.

```sh
fastboot boot skyward-lmi.img
```

Check that Recovery opens, that you can decrypt storage with your existing PIN, and that **Reboot to system** returns to Android.

### Persistent installation and rollback

Return to bootloader Fastboot, check the product codename again and flash the tested image.

```sh
fastboot flash recovery skyward-lmi.img
fastboot reboot
```

Once Android starts, power off and hold Volume Up and Power to enter Skyward. To roll back, flash the original Infinity-X Recovery for the installed build to the same partition. [Rollback steps](docs/lmi.zh-CN.md#永久安装与回退)

## Building

Build on Linux or WSL with Rust, the `aarch64-linux-android` target and Android NDK r29. Use the base Recovery and build inputs from the matching Infinity-X build. [Build commands and inputs](docs/lmi.zh-CN.md#从源码构建)

## License

[GPL-3.0-or-later](LICENSE)
