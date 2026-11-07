//! The office formats, by content type: the application that owns one,
//! how it is packaged, and the extension LibreOffice picks its import
//! filter from. One table for what is otherwise three: the category a
//! format classifies as, the plan the renderer gives it, and the package
//! the extractor opens for its embedded image. A format is here when
//! LibreOffice imports it, Microsoft's and OpenDocument's and the others
//! it reads through its import libraries: Apple's iWork, Visio,
//! Publisher, WordPerfect, Works, AbiWord, and DXF drawings.
//!
//! The variants are matched on their own content types: a macro-enabled
//! workbook is not an XLSX to the allowlist (`sniff::canonical` keeps
//! them distinct), and here it is a Calc file with its own extension,
//! which LibreOffice needs since `.xlsm` is not the `.xlsx` filter.

use crate::sniff;

/// The LibreOffice application that reads a format, which is the PDF
/// export filter the renderer asks for and the category the format
/// classifies as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    /// Word processing: Word, OpenDocument text, RTF, Pages, WordPerfect,
    /// Works, AbiWord.
    Writer,
    /// Spreadsheets: Excel, OpenDocument spreadsheet, Numbers.
    Calc,
    /// Presentations: PowerPoint, OpenDocument presentation, Keynote.
    Impress,
    /// Drawings: OpenDocument graphics, Visio, Publisher, DXF.
    Draw,
}

/// How a format is packaged, which is what the extractor can open and
/// what parser LibreOffice reads it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    /// An Office Open XML package, a ZIP: DOCX, XLSX, PPTX and their
    /// macro-enabled, template and slideshow variants, plus XLSB, whose
    /// parts are binary in the same package, and Visio's VSDX and VSDM.
    Ooxml,
    /// An OpenDocument package, a ZIP: ODT, ODS, ODP, ODG and their
    /// templates and masters.
    Odf,
    /// An OLE compound file: the Office 97-2003 DOC, XLS and PPT, Visio's
    /// VSD, Publisher and Works files.
    Ole,
    /// Rich Text Format, text.
    Rtf,
    /// An iWork package, a ZIP of Snappy-compressed protobuf archives:
    /// Pages, Numbers and Keynote.
    Iwork,
    /// The format's own encoding, binary or text, in no container:
    /// WordPerfect, AbiWord's XML, DXF.
    Native,
}

/// An office format: what reads it, how it is packaged, and the extension
/// LibreOffice picks its import filter from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// The application that owns the format.
    pub application: Application,
    /// How the file is packaged.
    pub package: Package,
    /// The extension the file is given for LibreOffice.
    pub extension: &'static str,
}

/// The content type of a Word document (Office Open XML).
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// The content type of a macro-enabled Word document.
pub const DOCM: &str = "application/vnd.ms-word.document.macroenabled.12";
/// The content type of a Word template (Office Open XML).
pub const DOTX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.template";
/// The content type of a macro-enabled Word template.
pub const DOTM: &str = "application/vnd.ms-word.template.macroenabled.12";
/// The content type of a Word 97-2003 document.
pub const DOC: &str = "application/msword";
/// The content type of a Rich Text Format document; `text/rtf` is its
/// alias.
pub const RTF: &str = "application/rtf";
/// The content type of an OpenDocument text.
pub const ODT: &str = "application/vnd.oasis.opendocument.text";
/// The content type of an OpenDocument text template.
pub const OTT: &str = "application/vnd.oasis.opendocument.text-template";
/// The content type of an OpenDocument master document.
pub const ODM: &str = "application/vnd.oasis.opendocument.text-master";
/// The content type of an OpenDocument master document template.
pub const OTM: &str = "application/vnd.oasis.opendocument.text-master-template";
/// The content type of an OpenDocument HTML document template.
pub const OTH: &str = "application/vnd.oasis.opendocument.text-web";

/// The content type of an Excel workbook (Office Open XML).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// The content type of a macro-enabled Excel workbook.
pub const XLSM: &str = "application/vnd.ms-excel.sheet.macroenabled.12";
/// The content type of an Excel template (Office Open XML).
pub const XLTX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.template";
/// The content type of a macro-enabled Excel template.
pub const XLTM: &str = "application/vnd.ms-excel.template.macroenabled.12";
/// The content type of an Excel binary workbook.
pub const XLSB: &str = "application/vnd.ms-excel.sheet.binary.macroenabled.12";
/// The content type of an Excel 97-2003 workbook.
pub const XLS: &str = "application/vnd.ms-excel";
/// The content type of an OpenDocument spreadsheet.
pub const ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";
/// The content type of an OpenDocument spreadsheet template.
pub const OTS: &str = "application/vnd.oasis.opendocument.spreadsheet-template";

/// The content type of a PowerPoint presentation (Office Open XML).
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
/// The content type of a macro-enabled PowerPoint presentation.
pub const PPTM: &str = "application/vnd.ms-powerpoint.presentation.macroenabled.12";
/// The content type of a PowerPoint slideshow (Office Open XML).
pub const PPSX: &str = "application/vnd.openxmlformats-officedocument.presentationml.slideshow";
/// The content type of a macro-enabled PowerPoint slideshow.
pub const PPSM: &str = "application/vnd.ms-powerpoint.slideshow.macroenabled.12";
/// The content type of a PowerPoint template (Office Open XML).
pub const POTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.template";
/// The content type of a macro-enabled PowerPoint template.
pub const POTM: &str = "application/vnd.ms-powerpoint.template.macroenabled.12";
/// The content type of a PowerPoint 97-2003 presentation.
pub const PPT: &str = "application/vnd.ms-powerpoint";
/// The content type of an OpenDocument presentation.
pub const ODP: &str = "application/vnd.oasis.opendocument.presentation";
/// The content type of an OpenDocument presentation template.
pub const OTP: &str = "application/vnd.oasis.opendocument.presentation-template";

/// The content type of an OpenDocument drawing.
pub const ODG: &str = "application/vnd.oasis.opendocument.graphics";
/// The content type of an OpenDocument drawing template.
pub const OTG: &str = "application/vnd.oasis.opendocument.graphics-template";

/// The content type of an Apple Pages document;
/// `application/x-iwork-pages-sffpages` is its alias.
pub const PAGES: &str = "application/vnd.apple.pages";
/// The content type of an Apple Numbers spreadsheet;
/// `application/x-iwork-numbers-sffnumbers` is its alias.
pub const NUMBERS: &str = "application/vnd.apple.numbers";
/// The content type of an Apple Keynote presentation;
/// `application/x-iwork-keynote-sffkey` is its alias.
pub const KEYNOTE: &str = "application/vnd.apple.keynote";

/// The content type of a WordPerfect document; `application/wordperfect`,
/// `application/x-wordperfect` and `application/wordperfect5.1` are its
/// aliases.
pub const WPD: &str = "application/vnd.wordperfect";
/// The content type of a Microsoft Works file. The type covers Works'
/// spreadsheets and databases too; it is read as the word processor's
/// `.wps`, by far the most common, and a spreadsheet declared as it
/// fails its conversion.
pub const WPS: &str = "application/vnd.ms-works";
/// The content type of an AbiWord document.
pub const ABW: &str = "application/x-abiword";

/// The content type of a Visio 2003-2010 drawing; `application/x-visio`
/// is its alias.
pub const VSD: &str = "application/vnd.visio";
/// The content type of a Visio drawing (Office Open XML).
pub const VSDX: &str = "application/vnd.ms-visio.drawing";
/// The content type of a macro-enabled Visio drawing.
pub const VSDM: &str = "application/vnd.ms-visio.drawing.macroenabled.12";
/// The content type of a Publisher document; `application/x-mspublisher`
/// is its alias.
pub const PUB: &str = "application/vnd.ms-publisher";
/// The content type of a DXF drawing, AutoCAD's exchange format, text or
/// binary; `application/dxf`, `application/x-dxf` and `image/x-dxf` are
/// its aliases.
pub const DXF: &str = "image/vnd.dxf";

/// The office format of a content type, `None` for anything else. An
/// alias is the format of its canonical type (`sniff::canonical`): `text/rtf`
/// is RTF, `application/x-msword` is DOC, `image/x-dxf` is DXF.
pub fn format(content_type: &str) -> Option<Format> {
    use Application::*;
    use Package::*;
    let (application, package, extension) = match sniff::canonical(content_type) {
        DOCX => (Writer, Ooxml, "docx"),
        DOCM => (Writer, Ooxml, "docm"),
        DOTX => (Writer, Ooxml, "dotx"),
        DOTM => (Writer, Ooxml, "dotm"),
        DOC => (Writer, Ole, "doc"),
        RTF => (Writer, Rtf, "rtf"),
        ODT => (Writer, Odf, "odt"),
        OTT => (Writer, Odf, "ott"),
        ODM => (Writer, Odf, "odm"),
        OTM => (Writer, Odf, "otm"),
        OTH => (Writer, Odf, "oth"),
        PAGES => (Writer, Iwork, "pages"),
        WPD => (Writer, Native, "wpd"),
        WPS => (Writer, Ole, "wps"),
        ABW => (Writer, Native, "abw"),

        XLSX => (Calc, Ooxml, "xlsx"),
        XLSM => (Calc, Ooxml, "xlsm"),
        XLTX => (Calc, Ooxml, "xltx"),
        XLTM => (Calc, Ooxml, "xltm"),
        XLSB => (Calc, Ooxml, "xlsb"),
        XLS => (Calc, Ole, "xls"),
        ODS => (Calc, Odf, "ods"),
        OTS => (Calc, Odf, "ots"),
        NUMBERS => (Calc, Iwork, "numbers"),

        PPTX => (Impress, Ooxml, "pptx"),
        PPTM => (Impress, Ooxml, "pptm"),
        PPSX => (Impress, Ooxml, "ppsx"),
        PPSM => (Impress, Ooxml, "ppsm"),
        POTX => (Impress, Ooxml, "potx"),
        POTM => (Impress, Ooxml, "potm"),
        PPT => (Impress, Ole, "ppt"),
        ODP => (Impress, Odf, "odp"),
        OTP => (Impress, Odf, "otp"),
        KEYNOTE => (Impress, Iwork, "key"),

        ODG => (Draw, Odf, "odg"),
        OTG => (Draw, Odf, "otg"),
        VSD => (Draw, Ole, "vsd"),
        VSDX => (Draw, Ooxml, "vsdx"),
        VSDM => (Draw, Ooxml, "vsdm"),
        PUB => (Draw, Ole, "pub"),
        DXF => (Draw, Native, "dxf"),

        _ => return None,
    };
    Some(Format {
        application,
        package,
        extension,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_by_type() {
        let docx = format(DOCX).unwrap();
        assert_eq!(docx.application, Application::Writer);
        assert_eq!(docx.package, Package::Ooxml);
        assert_eq!(docx.extension, "docx");
        // A variant keeps its own extension: LibreOffice picks the import
        // filter from it, and `.xlsm` is not the `.xlsx` filter.
        assert_eq!(format(XLSM).unwrap().extension, "xlsm");
        assert_eq!(format(XLSM).unwrap().application, Application::Calc);
        assert_eq!(format(XLSB).unwrap().extension, "xlsb");
        assert_eq!(format(DOC).unwrap().package, Package::Ole);
        assert_eq!(format(XLS).unwrap().package, Package::Ole);
        assert_eq!(format(PPT).unwrap().package, Package::Ole);
        assert_eq!(format(RTF).unwrap().package, Package::Rtf);
        assert_eq!(format(OTT).unwrap().package, Package::Odf);
        assert_eq!(format(ODM).unwrap().application, Application::Writer);
        assert_eq!(format(PPSX).unwrap().application, Application::Impress);
        assert_eq!(format(OTP).unwrap().application, Application::Impress);
        assert_eq!(format(ODG).unwrap().application, Application::Draw);
        assert_eq!(format(OTG).unwrap().extension, "otg");
        // The formats LibreOffice reads through its import libraries.
        assert_eq!(format(PAGES).unwrap().application, Application::Writer);
        assert_eq!(format(PAGES).unwrap().package, Package::Iwork);
        assert_eq!(format(NUMBERS).unwrap().application, Application::Calc);
        assert_eq!(format(KEYNOTE).unwrap().application, Application::Impress);
        assert_eq!(format(KEYNOTE).unwrap().extension, "key");
        assert_eq!(format(WPD).unwrap().package, Package::Native);
        assert_eq!(format(WPS).unwrap().application, Application::Writer);
        assert_eq!(format(WPS).unwrap().package, Package::Ole);
        assert_eq!(format(ABW).unwrap().application, Application::Writer);
        assert_eq!(format(VSD).unwrap().package, Package::Ole);
        assert_eq!(format(VSDX).unwrap().package, Package::Ooxml);
        assert_eq!(format(VSDX).unwrap().application, Application::Draw);
        assert_eq!(format(VSDM).unwrap().extension, "vsdm");
        assert_eq!(format(PUB).unwrap().application, Application::Draw);
        assert_eq!(format(DXF).unwrap().application, Application::Draw);
        assert_eq!(format(DXF).unwrap().package, Package::Native);
        for other in [
            "application/pdf",
            "text/csv",
            "application/zip",
            "application/x-ole-storage",
            "application/vnd.ms-excel.addin.macroenabled.12",
            "application/octet-stream",
            "image/vnd.dwg",
            "application/vnd.ms-outlook",
        ] {
            assert_eq!(format(other), None, "{other}");
        }
    }

    #[test]
    fn aliases_take_their_canonical_format() {
        assert_eq!(format("text/rtf"), format(RTF));
        assert_eq!(format("application/x-msword"), format(DOC));
        assert_eq!(format("application/x-iwork-pages-sffpages"), format(PAGES));
        assert_eq!(
            format("application/x-iwork-numbers-sffnumbers"),
            format(NUMBERS)
        );
        assert_eq!(
            format("application/x-iwork-keynote-sffkey"),
            format(KEYNOTE)
        );
        for wordperfect in [
            "application/wordperfect",
            "application/x-wordperfect",
            "application/wordperfect5.1",
        ] {
            assert_eq!(format(wordperfect), format(WPD), "{wordperfect}");
        }
        assert_eq!(format("application/x-visio"), format(VSD));
        assert_eq!(format("application/x-mspublisher"), format(PUB));
        for dxf in ["application/dxf", "application/x-dxf", "image/x-dxf"] {
            assert_eq!(format(dxf), format(DXF), "{dxf}");
        }
    }

    #[test]
    fn extensions_are_distinct() {
        let types = [
            DOCX, DOCM, DOTX, DOTM, DOC, RTF, ODT, OTT, ODM, OTM, OTH, XLSX, XLSM, XLTX, XLTM,
            XLSB, XLS, ODS, OTS, PPTX, PPTM, PPSX, PPSM, POTX, POTM, PPT, ODP, OTP, ODG, OTG,
            PAGES, NUMBERS, KEYNOTE, WPD, WPS, ABW, VSD, VSDX, VSDM, PUB, DXF,
        ];
        let mut extensions = std::collections::HashSet::new();
        for content_type in types {
            let format = format(content_type).unwrap_or_else(|| panic!("{content_type}"));
            assert!(extensions.insert(format.extension), "{content_type}");
        }
    }
}
