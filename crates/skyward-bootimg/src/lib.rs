//! Android boot images and the newc archives in their ramdisks.
//! The host packer also handles the version 2 layout.

mod image;
mod legacy;
mod ramdisk;

pub use image::BootImage;
pub use legacy::LegacyBootImage;
pub use ramdisk::{lz4_legacy, Entry, Ramdisk};
