//! Media type detection by extension.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Photo,
    Raw,
    Video,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Photo => "photo",
            Kind::Raw => "raw",
            Kind::Video => "video",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "photo" => Some(Kind::Photo),
            "raw" => Some(Kind::Raw),
            "video" => Some(Kind::Video),
            _ => None,
        }
    }
}

pub const PHOTO_EXT: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif",
];
pub const RAW_EXT: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "dng", "srw",
    "x3f", "3fr", "iiq", "erf", "kdc", "mrw",
];
pub const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "avi", "mkv", "webm", "wmv", "mts", "m2ts", "ts", "3gp", "3g2", "mpg",
    "mpeg", "flv", "vob",
];

/// Formats the webview (WebView2 / WebKit) can display natively without conversion.
pub const BROWSER_IMAGE_EXT: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "bmp", "avif"];

/// Formats the pure-Rust `image` crate decodes (used for thumbnails without any OS help).
pub const RUST_DECODABLE_EXT: &[&str] =
    &["jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "bmp", "tif", "tiff"];

pub fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => String::new(),
    }
}

pub fn kind_for_ext(ext: &str) -> Option<Kind> {
    if PHOTO_EXT.contains(&ext) {
        Some(Kind::Photo)
    } else if RAW_EXT.contains(&ext) {
        Some(Kind::Raw)
    } else if VIDEO_EXT.contains(&ext) {
        Some(Kind::Video)
    } else {
        None
    }
}

pub fn is_browser_image(ext: &str) -> bool {
    BROWSER_IMAGE_EXT.contains(&ext)
}

pub fn is_rust_decodable(ext: &str) -> bool {
    RUST_DECODABLE_EXT.contains(&ext)
}
