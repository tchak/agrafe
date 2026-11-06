//! The format the derived pictures, thumbnails and previews, are encoded
//! in: a deployment's choice, recorded on every representation it renders,
//! so what is served is named by what was built and not by what the
//! configuration says today.

use serde::{Deserialize, Serialize};

/// An output format of the renderer. WebP decodes everywhere; JPEG XL is
/// the one the web is moving to, opt-in until the browsers that lack it
/// are gone from a deployment's audience.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// WebP, lossy, `image/webp`. The default.
    #[default]
    Webp,
    /// JPEG XL, lossy, `image/jxl`.
    Jxl,
}

crate::str_enum!(ImageFormat {
    Webp => "webp",
    Jxl => "jxl",
});

impl ImageFormat {
    /// The media type a picture in this format is served as.
    pub fn content_type(self) -> &'static str {
        match self {
            ImageFormat::Webp => "image/webp",
            ImageFormat::Jxl => "image/jxl",
        }
    }

    /// The file extension, without the dot: what vips picks its saver by,
    /// and what the download name ends in.
    pub fn extension(self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types() {
        assert_eq!(ImageFormat::default(), ImageFormat::Webp);
        assert_eq!(ImageFormat::Webp.content_type(), "image/webp");
        assert_eq!(ImageFormat::Jxl.content_type(), "image/jxl");
        assert_eq!(ImageFormat::Jxl.extension(), "jxl");
        assert_eq!("jxl".parse::<ImageFormat>().unwrap(), ImageFormat::Jxl);
        assert!("avif".parse::<ImageFormat>().is_err());
        assert_eq!(
            serde_json::from_str::<ImageFormat>(r#""webp""#).unwrap(),
            ImageFormat::Webp
        );
        assert_eq!(
            serde_json::to_string(&ImageFormat::Jxl).unwrap(),
            r#""jxl""#
        );
    }
}
