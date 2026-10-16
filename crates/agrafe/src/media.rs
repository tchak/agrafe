//! Derived representations: which files can be rendered, from what, and the
//! tools that do it. Decoding and rendering never happen in this process;
//! `sandbox` runs them as subprocesses with resource limits.

pub mod extract;
pub mod icons;
pub mod render;
pub mod sandbox;
pub mod text;

use crate::blob::Representation;
use crate::classify::{self, Category};
use crate::sniff;

pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const ODT: &str = "application/vnd.oasis.opendocument.text";

/// Where a representation of a file can come from. A file lists its sources
/// in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The file is an image: decode and transcode it.
    Image,
    /// A document that wraps one image with next to no text: pull the image
    /// out at full resolution.
    EmbeddedImage,
    /// Render the first page.
    FirstPage,
    /// Set the first lines of the file in a monospace font. A hint of what
    /// the file is, not a way to read it: thumbnails only.
    TextSample,
}

impl Source {
    /// Whether this source can produce `kind`.
    pub fn serves(self, kind: Representation) -> bool {
        match self {
            Source::TextSample => kind == Representation::Thumbnail,
            _ => true,
        }
    }
}

/// The sources for a content type, in order. Empty when the file cannot be
/// rendered, in which case the thumbnail is an icon and there is no
/// preview. Image types are an allowlist of what libvips loads natively:
/// everything else would go through ImageMagick, which is more attack
/// surface than it is worth.
pub fn sources(content_type: &str) -> &'static [Source] {
    match sniff::canonical(content_type) {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/tiff" | "image/heif"
        | "image/avif" | "image/svg+xml" | "image/bmp" | "image/jp2" => &[Source::Image],
        classify::PDF => &[Source::EmbeddedImage, Source::FirstPage],
        DOCX | ODT => &[Source::EmbeddedImage],
        classify::CSV | "text/tab-separated-values" => &[Source::TextSample],
        other if classify::classify(other) == Category::Text => &[Source::TextSample],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_by_type() {
        assert_eq!(sources("image/jpg"), &[Source::Image]);
        assert_eq!(sources("image/heic"), &[Source::Image]);
        assert_eq!(
            sources(classify::PDF),
            &[Source::EmbeddedImage, Source::FirstPage]
        );
        assert_eq!(sources(DOCX), &[Source::EmbeddedImage]);
        assert!(sources("image/vnd.adobe.photoshop").is_empty());
        assert_eq!(sources("text/plain"), &[Source::TextSample]);
        assert_eq!(sources("application/json"), &[Source::TextSample]);
        assert_eq!(sources(classify::CSV), &[Source::TextSample]);
        assert!(sources(classify::XLSX).is_empty());
        assert!(Source::TextSample.serves(Representation::Thumbnail));
        assert!(!Source::TextSample.serves(Representation::Preview));
        assert!(Source::FirstPage.serves(Representation::Preview));
    }
}
