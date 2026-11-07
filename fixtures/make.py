#!/usr/bin/env python3
"""Builds the fixture files next to this script, and the renderer probe
files under crates/agrafe-render/assets/probe. Needs Pillow. The outputs are
committed; run this only to change them. The XML map fixtures (`.gpx`,
`.kml`, `.gml`) are hand-written and edited directly; the `.kmz` ones are built
here from them, and the zipped shapefiles are written here.

The Office 97-2003 fixtures (`letter.doc`, `table.xls`, `slides.ppt`)
cannot be written by hand: they are converted from the ZIP ones by
LibreOffice, when one is found, and only when missing or with `--legacy`,
since each conversion stamps the time into the file. `slides.ppt` is about
600 KB, nearly all of it an uncompressed white thumbnail its PowerPoint 97
filter always writes, which git stores in about 11 KB. The iWork fixtures
(`pages5-file.pages`, `numbers3-file.numbers`, `keynote6-file.key`) are
libetonyek's own test documents, the import library's, downloaded from its
release tag (`CORPUS`) only when missing or with `--legacy` and checked
against their SHA-256: nothing here writes an iWork package, and a corpus
file is one Pages, Numbers and Keynote wrote. `keynote6-file.key` is about
360 KB, the images of its theme. The probe's HEIC
(`pixel.heic`), which Pillow cannot write, is likewise made by `vips` from
the probe's PNG, only when missing or with `--legacy`; and so is the
self-signed certificate of the fetch tests' TLS remote
(`self-signed.crt.pem`, `self-signed.key.pem`), by `openssl`, since a new
key is a new file every run."""

import base64
import hashlib
import io
import json
import shutil
import struct
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
import zlib
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).parent
PROBE = HERE / "../crates/agrafe-render/assets/probe"


def jpeg(width, height, quality=60):
    im = Image.new("RGB", (width, height), (200, 40, 40))
    ImageDraw.Draw(im).rectangle([width // 4, height // 4, width // 2, height // 2], fill=(40, 40, 200))
    out = io.BytesIO()
    im.save(out, "JPEG", quality=quality)
    return out.getvalue()


def png(width, height):
    im = Image.new("RGB", (width, height), (40, 160, 60))
    out = io.BytesIO()
    im.save(out, "PNG")
    return out.getvalue()


def raster(format, width, height):
    """A flat image in one of the formats Pillow writes, for the probe of
    the vips loader that reads it."""
    im = Image.new("RGB", (width, height), (40, 160, 60))
    out = io.BytesIO()
    im.save(out, format)
    return out.getvalue()


def with_orientation(jpeg_bytes, orientation):
    """Insert an EXIF APP1 segment carrying only the orientation tag."""
    assert jpeg_bytes[:2] == b"\xff\xd8"
    tiff = b"MM" + struct.pack(">HI", 42, 8)
    ifd = struct.pack(">H", 1) + struct.pack(">HHIHH", 0x0112, 3, 1, orientation, 0) + struct.pack(">I", 0)
    exif = b"Exif\x00\x00" + tiff + ifd
    app1 = b"\xff\xe1" + struct.pack(">H", len(exif) + 2) + exif
    return jpeg_bytes[:2] + app1 + jpeg_bytes[2:]


def pdf(objects):
    """A PDF from object bodies; object 1 is the catalog."""
    out = b"%PDF-1.4\n"
    offsets = []
    for i, body in enumerate(objects, 1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return out


def stream(dictionary, data):
    return f"<< {dictionary} /Length {len(data)} >>\nstream\n".encode() + data + b"\nendstream"


LONG_TEXT = "This page is a real document with a paragraph of text next to the picture."
TABLE = [["nom", "ville", "montant"], ["Dupont", "Besançon", "12,50"], ["Martin", "Orléans", "7,00"]]


def image_pdf(image, width, height, page_w, page_h, place, text=None, filt="/DCTDecode", colorspace="/DeviceRGB"):
    resources = "/XObject << /Im0 5 0 R >>"
    content = f"q {place} cm /Im0 Do Q"
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        None,
        None,
        stream(f"/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace {colorspace} /BitsPerComponent 8 /Filter {filt}", image),
    ]
    if text:
        resources += " /Font << /F1 6 0 R >>"
        content += f" BT /F1 6 Tf 2 2 Td ({text}) Tj ET"
        objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    objects[2] = f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w} {page_h}] /Resources << {resources} >> /Contents 4 0 R >>".encode()
    objects[3] = stream("", content.encode())
    return pdf(objects)


def text_pdf(page_w, page_h, text):
    return pdf([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w} {page_h}] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".encode(),
        stream("", f"BT /F1 24 Tf 20 40 Td ({text}) Tj ET".encode()),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ])


def empty_pdf(page_w, page_h):
    return pdf([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w} {page_h}] >>".encode(),
    ])


def zip_entry(z, name, data, compress=zipfile.ZIP_DEFLATED):
    """An entry with a fixed timestamp, so that a run of this script changes
    only the files whose content changed."""
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = compress
    z.writestr(info, data)


# A4 in twentieths of a point (DOCX) and in centimetres (ODF), so that a
# page renders the same whatever the converter's locale.
A4_TWIPS = (11906, 16838)
A4_CM = ("21.0cm", "29.7cm")
PAGE_BREAK = '<w:p><w:r><w:br w:type="page"/></w:r></w:p>'


def kmz(document, icon=None):
    """A KMZ: the document as `doc.kml` first, as Google Earth writes it,
    then its icon."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "doc.kml", document)
        if icon is not None:
            zip_entry(z, "files/icon.png", icon)
    return out.getvalue()


# The shape types a `.shp` record may hold, of those the fixtures use.
SHP_POINT_Z, SHP_POLYLINE, SHP_POLYGON = 11, 3, 5

# Lambert-93 as ArcGIS and QGIS write it in a `.prj`.
LAMBERT93_PRJ = (
    'PROJCS["RGF_1993_Lambert_93",GEOGCS["GCS_RGF_1993",DATUM["D_RGF_1993",'
    'SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],'
    'UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],'
    'PARAMETER["False_Easting",700000.0],PARAMETER["False_Northing",6600000.0],'
    'PARAMETER["Central_Meridian",3.0],PARAMETER["Standard_Parallel_1",49.0],'
    'PARAMETER["Standard_Parallel_2",44.0],PARAMETER["Latitude_Of_Origin",46.5],'
    'UNIT["Meter",1.0]]'
)


def shp_header(kind, words, points):
    """The 100-byte header of a `.shp` or `.shx` of `words` 16-bit words,
    over the bounding box of `points`."""
    xs, ys = [p[0] for p in points], [p[1] for p in points]
    return (struct.pack(">7i", 9994, 0, 0, 0, 0, 0, words)
            + struct.pack("<2i8d", 1000, kind, min(xs), min(ys), max(xs), max(ys), 0, 0, 0, 0))


def shp_content(kind, parts):
    """A record's content: a PointZ of the one point of the one part, else
    the parts of a PolyLine or a Polygon."""
    points = [p for part in parts for p in part]
    if kind == SHP_POINT_Z:
        x, y = parts[0][0]
        return struct.pack("<i4d", kind, x, y, 35.0, 0.0)
    xs, ys = [p[0] for p in points], [p[1] for p in points]
    starts = [sum(len(part) for part in parts[:i]) for i in range(len(parts))]
    return (struct.pack("<i4d2i", kind, min(xs), min(ys), max(xs), max(ys), len(parts), len(points))
            + struct.pack(f"<{len(parts)}i", *starts)
            + b"".join(struct.pack("<2d", *p) for p in points))


def dbf(names):
    """A dBase III table of one `NAME` text column, a row per name."""
    width = 32
    header = struct.pack("<B3BIHH20x", 3, 80, 1, 1, len(names), 32 + 32 + 1, 1 + width)
    field = struct.pack("<11sc4xBB14x", b"NAME", b"C", width, 0)
    rows = b"".join(b" " + name.encode().ljust(width) for name in names)
    return header + field + b"\r" + rows + b"\x1a"


def shapefile(kind, records):
    """The `.shp`, `.shx` and `.dbf` of one layer of `kind`: `records` is a
    list of (name, parts), each part a list of (x, y)."""
    contents = [shp_content(kind, parts) for _, parts in records]
    points = [p for _, parts in records for part in parts for p in part]
    shp, shx, offset = b"", b"", 50
    for number, content in enumerate(contents, 1):
        words = len(content) // 2
        shp += struct.pack(">2i", number, words) + content
        shx += struct.pack(">2i", offset, words)
        offset += 4 + words
    shp = shp_header(kind, 50 + len(shp) // 2, points) + shp
    shx = shp_header(kind, 50 + len(shx) // 2, points) + shx
    return shp, shx, dbf([name for name, _ in records])


def zipped_shapefile(layers, prj=LAMBERT93_PRJ):
    """A ZIP of `layers`, {path stem: (kind, records)}, each with its `.shx`
    and `.dbf`, and its `.prj` unless `prj` is None, the `.shp` first as
    QGIS writes them."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        for stem, (kind, records) in layers.items():
            shp, shx, table = shapefile(kind, records)
            zip_entry(z, f"{stem}.shp", shp)
            zip_entry(z, f"{stem}.shx", shx)
            zip_entry(z, f"{stem}.dbf", table)
            if prj is not None:
                zip_entry(z, f"{stem}.prj", prj)
    return out.getvalue()


def docx(text, media_name=None, media=None, pages=1):
    """A Word document: `text` on each of `pages` A4 pages, and, with a media
    file, a drawing that references it."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        zip_entry(z, "_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        paragraph = f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>" if text else ""
        body = PAGE_BREAK.join(paragraph.replace("{page}", str(n + 1)) for n in range(pages))
        if media is not None:
            body += "<w:p><w:r><w:drawing/></w:r></w:p>"
        body += f'<w:sectPr><w:pgSz w:w="{A4_TWIPS[0]}" w:h="{A4_TWIPS[1]}"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="0" w:footer="0" w:gutter="0"/></w:sectPr>'
        zip_entry(z, "word/document.xml", f'<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>{body}</w:body></w:document>')
        if media is not None:
            zip_entry(z, "word/_rels/document.xml.rels", f'<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/{media_name}"/></Relationships>')
            zip_entry(z, f"word/media/{media_name}", media)
    return out.getvalue()


def broken_docx():
    """A Word document that is a well-formed ZIP whose document part is cut
    mid-element: LibreOffice cannot load it."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        zip_entry(z, "_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        zip_entry(z, "word/document.xml", '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>cut')
    return out.getvalue()


def odt(text, media=None):
    """An OpenDocument text: `text` on an A4 page and, with a media file, a
    frame that shows it."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "mimetype", "application/vnd.oasis.opendocument.text", zipfile.ZIP_STORED)
        picture = '<manifest:file-entry manifest:full-path="Pictures/1.png" manifest:media-type="image/png"/>' if media is not None else ""
        zip_entry(z, "META-INF/manifest.xml", f'<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>{picture}</manifest:manifest>')
        paragraph = f"<text:p>{text}</text:p>" if text else ""
        frame = '<text:p><draw:frame><draw:image xlink:href="Pictures/1.png"/></draw:frame></text:p>' if media is not None else ""
        zip_entry(z, "content.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text>{paragraph}{frame}</office:text></office:body></office:document-content>')
        zip_entry(z, "styles.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="{A4_CM[0]}" fo:page-height="{A4_CM[1]}" fo:margin="2cm"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1"/></office:master-styles></office:document-styles>')
        if media is not None:
            zip_entry(z, "Pictures/1.png", media)
    return out.getvalue()


def xlsx(rows, macro_enabled=False, break_after=None):
    """A workbook of one sheet holding `rows`, lists of strings, as inline
    strings. Macro-enabled, the workbook part declares the XLSM content
    type; the package is otherwise the same, there being no macro. With
    `break_after`, a manual page break follows that row, so the sheet
    prints on two pages."""
    def cell(col, row, value):
        return f'<c r="{chr(65 + col)}{row}" t="inlineStr"><is><t>{value}</t></is></c>'
    data = "".join(
        f'<row r="{r + 1}">' + "".join(cell(c, r + 1, v) for c, v in enumerate(row)) + "</row>"
        for r, row in enumerate(rows)
    )
    breaks = "" if break_after is None else f'<rowBreaks count="1" manualBreakCount="1"><brk id="{break_after}" max="16383" man="1"/></rowBreaks>'
    main = "application/vnd.ms-excel.sheet.macroEnabled.main+xml" if macro_enabled else "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "[Content_Types].xml", f'<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="{main}"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
        zip_entry(z, "_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        zip_entry(z, "xl/workbook.xml", '<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Feuille1" sheetId="1" r:id="rId1"/></sheets></workbook>')
        zip_entry(z, "xl/_rels/workbook.xml.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
        zip_entry(z, "xl/worksheets/sheet1.xml", f'<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData>{breaks}</worksheet>')
    return out.getvalue()


def ods(rows):
    """An OpenDocument spreadsheet of one sheet holding `rows`, lists of
    strings."""
    def cell(value):
        return f'<table:table-cell office:value-type="string"><text:p>{value}</text:p></table:table-cell>'
    data = "".join("<table:table-row>" + "".join(cell(v) for v in row) + "</table:table-row>" for row in rows)
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "mimetype", "application/vnd.oasis.opendocument.spreadsheet", zipfile.ZIP_STORED)
        zip_entry(z, "META-INF/manifest.xml", '<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>')
        zip_entry(z, "content.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Feuille1">{data}</table:table></office:spreadsheet></office:body></office:document-content>')
    return out.getvalue()


PPTX_NS = 'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
# 10 by 7.5 inches, in EMU, PowerPoint's 4:3 default.
SLIDE_EMU = (9144000, 6858000)


def pptx_package(z, slides):
    """The parts of a presentation of `slides`, one title each, into the
    archive `z`: a minimal package, with no layout, master or theme, which
    LibreOffice fills in from its defaults."""
    overrides = "".join(
        f'<Override PartName="/ppt/slides/slide{n + 1}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>'
        for n in range(len(slides))
    )
    zip_entry(z, "[Content_Types].xml", f'<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>{overrides}</Types>')
    zip_entry(z, "_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>')
    ids = "".join(f'<p:sldId id="{256 + n}" r:id="rId{n + 1}"/>' for n in range(len(slides)))
    zip_entry(z, "ppt/presentation.xml", f'<?xml version="1.0" encoding="UTF-8"?><p:presentation {PPTX_NS}><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="{SLIDE_EMU[0]}" cy="{SLIDE_EMU[1]}"/><p:notesSz cx="{SLIDE_EMU[1]}" cy="{SLIDE_EMU[0]}"/></p:presentation>')
    rels = "".join(
        f'<Relationship Id="rId{n + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{n + 1}.xml"/>'
        for n in range(len(slides))
    )
    zip_entry(z, "ppt/_rels/presentation.xml.rels", f'<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>')
    for n, text in enumerate(slides):
        zip_entry(z, f"ppt/slides/slide{n + 1}.xml", f'<?xml version="1.0" encoding="UTF-8"?><p:sld {PPTX_NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="457200" y="457200"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:rPr lang="fr-FR" sz="4400"/><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>')


def pptx(slides):
    """A PowerPoint presentation: one 4:3 slide per entry of `slides`, each
    with that text as its title."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        pptx_package(z, slides)
    return out.getvalue()


def broken_pptx():
    """A PowerPoint presentation that is a well-formed ZIP whose
    presentation part is cut mid-element: LibreOffice cannot load it."""
    out = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(pptx(["cut"]))) as src, zipfile.ZipFile(out, "w") as z:
        for info in src.infolist():
            content = src.read(info.filename)
            if info.filename == "ppt/presentation.xml":
                content = content[: content.index(b"<p:sldIdLst>") + 20]
            zip_entry(z, info.filename, content)
    return out.getvalue()


ODF_NS = 'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:script="urn:oasis:names:tc:opendocument:xmlns:script:1.0"'
# A 4:3 slide or drawing page, in centimetres.
SLIDE_CM = ("28cm", "21cm")


def odf(mimetype, body, page, scripts="", manifest="", entries=()):
    """An OpenDocument package of `mimetype` whose body is `body` on pages
    of `page` (width, height), plus `scripts` before the body, `manifest`
    entries and extra `entries` (name, bytes)."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "mimetype", mimetype, zipfile.ZIP_STORED)
        zip_entry(z, "META-INF/manifest.xml", f'<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="{mimetype}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>{manifest}</manifest:manifest>')
        zip_entry(z, "content.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-content {ODF_NS}>{scripts}<office:body>{body}</office:body></office:document-content>')
        orientation = "landscape" if page[0] > page[1] else "portrait"
        # Impress and Draw take the page size from the master page's layout
        # only when an `office:styles` element precedes it, empty or not.
        zip_entry(z, "styles.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-styles {ODF_NS}><office:styles/><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="{page[0]}" fo:page-height="{page[1]}" fo:margin="1cm" style:print-orientation="{orientation}"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1"/></office:master-styles></office:document-styles>')
        for name, data in entries:
            zip_entry(z, name, data)
    return out.getvalue()


def odp(slides):
    """An OpenDocument presentation: one 4:3 slide per entry of `slides`,
    each with that text in a box."""
    pages = "".join(
        f'<draw:page draw:name="page{n + 1}" draw:master-page-name="Standard"><draw:frame svg:width="24cm" svg:height="4cm" svg:x="2cm" svg:y="2cm"><draw:text-box><text:p>{text}</text:p></draw:text-box></draw:frame></draw:page>'
        for n, text in enumerate(slides)
    )
    return odf("application/vnd.oasis.opendocument.presentation", f"<office:presentation>{pages}</office:presentation>", SLIDE_CM)


def odg(pages):
    """An OpenDocument drawing of `pages` pages, a rectangle on each at a
    different place."""
    body = "".join(
        f'<draw:page draw:name="page{n + 1}" draw:master-page-name="Standard"><draw:rect svg:width="10cm" svg:height="6cm" svg:x="{2 + 12 * (n % 2)}cm" svg:y="4cm"><text:p>{n + 1}</text:p></draw:rect></draw:page>'
        for n in range(pages)
    )
    return odf("application/vnd.oasis.opendocument.graphics", f"<office:drawing>{body}</office:drawing>", SLIDE_CM)


# A Basic macro that creates a folder next to the document it is in. Bound
# to the document's load event, it would run when LibreOffice opens the
# document, if LibreOffice ran macros from documents it converts.
MACRO = '''<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE script:module PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "module.dtd">
<script:module xmlns:script="http://openoffice.org/2000/script" script:name="Module1" script:language="StarBasic">Sub Main
  Dim fa As Object
  fa = CreateUnoService(&quot;com.sun.star.ucb.SimpleFileAccess&quot;)
  fa.createFolder(ThisComponent.getURL() &amp; &quot;.macro-ran&quot;)
End Sub
</script:module>'''


def macro_odt():
    """An OpenDocument text of one page carrying `MACRO` in its Standard
    Basic library, bound to the document's load event, as LibreOffice
    itself stores one."""
    scripts = '<office:scripts><office:event-listeners><script:event-listener script:language="ooo:script" script:event-name="dom:load" xlink:href="vnd.sun.star.script:Standard.Module1.Main?language=Basic&amp;location=document"/></office:event-listeners></office:scripts>'
    manifest = '<manifest:file-entry manifest:full-path="Basic/" manifest:media-type="application/binary"/><manifest:file-entry manifest:full-path="Basic/script-lc.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="Basic/Standard/" manifest:media-type="application/binary"/><manifest:file-entry manifest:full-path="Basic/Standard/script-lb.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="Basic/Standard/Module1.xml" manifest:media-type="text/xml"/>'
    entries = [
        ("Basic/script-lc.xml", '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE library:libraries PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "libraries.dtd"><library:libraries xmlns:library="http://openoffice.org/2000/library" xmlns:xlink="http://www.w3.org/1999/xlink"><library:library library:name="Standard" library:link="false"/></library:libraries>'),
        ("Basic/Standard/script-lb.xml", '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE library:library PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "library.dtd"><library:library xmlns:library="http://openoffice.org/2000/library" library:name="Standard" library:readonly="false" library:passwordprotected="false"><library:element library:name="Module1"/></library:library>'),
        ("Basic/Standard/Module1.xml", MACRO),
    ]
    return odf("application/vnd.oasis.opendocument.text", "<office:text><text:p>A document with a macro.</text:p></office:text>", A4_CM, scripts, manifest, entries)


def rtf(text):
    """A Rich Text Format letter: `text`, ASCII, on an A4 page."""
    return ("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\\paperw%d\\paperh%d\\f0\\fs48 %s\\par}" % (A4_TWIPS[0], A4_TWIPS[1], text)).encode("ascii")


def wordperfect(text):
    """A WordPerfect 5.1 document: the 16-byte prefix (the `\\xffWPC`
    magic, the document's offset, the WordPerfect product and document
    file type, version 5.1, no encryption) and `text`, ASCII, as one
    hard-return-ended line."""
    prefix = b"\xffWPC" + struct.pack("<I", 16) + bytes([1, 0x0A, 0, 1]) + struct.pack("<HH", 0, 0)
    return prefix + text.encode("ascii") + b"\x0a"


def abiword(text):
    """An AbiWord document: `text` on an A4 page."""
    return f'''<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE abiword PUBLIC "-//ABISOURCE//DTD AWML 1.0 Strict//EN" "http://www.abisource.com/awml.dtd">
<abiword xmlns="http://www.abisource.com/awml.dtd" version="3.0.0" fileformat="1.1">
<pagesize pagetype="A4" orientation="portrait" width="210.000000" height="297.000000" units="mm" page-scale="1.000000"/>
<section>
<p>{text}</p>
</section>
</abiword>
'''.encode()


VISIO_NS = 'xmlns="http://schemas.microsoft.com/office/visio/2012/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
# A4 in inches, Visio's unit.
A4_IN = (8.267716535433071, 11.69291338582677)


def visio_rels(rel_type, target):
    """A relationships part of one relationship, Visio's `rel_type`."""
    return f'<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.microsoft.com/visio/2010/relationships/{rel_type}" Target="{target}"/></Relationships>'


def vsdx(text):
    """A Visio drawing: one A4 page with a 4 by 2 inch rectangle labelled
    `text`, a minimal package with no master, theme or window part."""
    w, h = 4, 2
    corners = [(0, 0), (w, 0), (w, h), (0, h), (0, 0)]
    rows = "".join(
        f'<Row T="{"MoveTo" if n == 0 else "LineTo"}" IX="{n + 1}"><Cell N="X" V="{x}"/><Cell N="Y" V="{y}"/></Row>'
        for n, (x, y) in enumerate(corners)
    )
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/visio/document.xml" ContentType="application/vnd.ms-visio.drawing.main+xml"/><Override PartName="/visio/pages/pages.xml" ContentType="application/vnd.ms-visio.pages+xml"/><Override PartName="/visio/pages/page1.xml" ContentType="application/vnd.ms-visio.page+xml"/></Types>')
        zip_entry(z, "_rels/.rels", visio_rels("document", "visio/document.xml"))
        zip_entry(z, "visio/document.xml", f'<?xml version="1.0" encoding="UTF-8"?><VisioDocument {VISIO_NS}/>')
        zip_entry(z, "visio/_rels/document.xml.rels", visio_rels("pages", "pages/pages.xml"))
        zip_entry(z, "visio/pages/pages.xml", f'<?xml version="1.0" encoding="UTF-8"?><Pages {VISIO_NS}><Page ID="0" NameU="Page-1" Name="Page-1"><PageSheet><Cell N="PageWidth" V="{A4_IN[0]}"/><Cell N="PageHeight" V="{A4_IN[1]}"/></PageSheet><Rel r:id="rId1"/></Page></Pages>')
        zip_entry(z, "visio/pages/_rels/pages.xml.rels", visio_rels("page", "page1.xml"))
        zip_entry(z, "visio/pages/page1.xml", f'<?xml version="1.0" encoding="UTF-8"?><PageContents {VISIO_NS}><Shapes><Shape ID="1" Type="Shape"><Cell N="PinX" V="4"/><Cell N="PinY" V="8"/><Cell N="Width" V="{w}"/><Cell N="Height" V="{h}"/><Cell N="LocPinX" V="{w / 2}"/><Cell N="LocPinY" V="{h / 2}"/><Section N="Geometry" IX="0">{rows}</Section><Text>{text}</Text></Shape></Shapes></PageContents>')
    return out.getvalue()


def dxf(lines):
    """An ASCII DXF drawing: an ENTITIES section of one LINE per entry of
    `lines`, (x1, y1, x2, y2) in drawing units, and nothing else, which
    is all a reader needs."""
    pairs = []

    def group(code, value):
        pairs.append(f"{code:>3}\n{value}\n")

    group(0, "SECTION")
    group(2, "ENTITIES")
    for x1, y1, x2, y2 in lines:
        group(0, "LINE")
        group(8, "0")
        for code, value in [(10, x1), (20, y1), (30, 0), (11, x2), (21, y2), (31, 0)]:
            group(code, f"{value:.1f}")
    group(0, "ENDSEC")
    group(0, "EOF")
    return "".join(pairs).encode("ascii")


def soffice():
    """The LibreOffice binary, on the PATH or in the macOS bundle, or None."""
    for candidate in [shutil.which("soffice"), "/Applications/LibreOffice.app/Contents/MacOS/soffice"]:
        if candidate and Path(candidate).is_file():
            return candidate
    return None


def legacy(command, source, export):
    """`source` converted by LibreOffice with the `export` filter
    (`"doc:MS Word 97"`), in a throwaway profile."""
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(
            [command, f"-env:UserInstallation=file://{tmp}/profile", "--headless", "--norestore", "--nologo",
             "--convert-to", export, "--outdir", tmp, str(source)],
            check=True, capture_output=True, timeout=300,
        )
        return (Path(tmp) / f"{source.stem}.{export.split(':')[0]}").read_bytes()


# The fixtures taken from a corpus rather than written: libetonyek's test
# documents, under the MPL-2.0 like the library, at the release tag they
# were taken from, with the SHA-256 of each. A Pages 5 letter, a Numbers 3
# table and a Keynote 6 deck, each written by the application itself.
LIBETONYEK = "https://git.libreoffice.org/libetonyek/+/refs/tags/libetonyek-0.1.13/src/test/data"
CORPUS = {
    "pages5-file.pages": "b5de90dfa78077c62e1c88ddaa188e412ffa9dd31558e2774715733f9d58dfe7",
    "numbers3-file.numbers": "a37aeccdd6413e8aa74c68d6dcd173351494946ea85a07e759aaf16b9855f1ae",
    "keynote6-file.key": "47d9cd0cf08b8c1157255d7d9299418b79adb2272e2b5966e6133deedd22441a",
}


def corpus(name, digest):
    """The file `name` of libetonyek's test data, from gitiles, which
    serves a blob as base64 with `?format=TEXT`; exits when its SHA-256 is
    not `digest`."""
    with urllib.request.urlopen(f"{LIBETONYEK}/{name}?format=TEXT", timeout=120) as response:
        data = base64.b64decode(response.read())
    actual = hashlib.sha256(data).hexdigest()
    if actual != digest:
        sys.exit(f"{name}: SHA-256 {actual}, expected {digest}")
    return data


def feature(geometry, properties=None):
    return {"type": "Feature", "properties": properties or {}, "geometry": geometry}


def collection(*features):
    return {"type": "FeatureCollection", "features": list(features)}


def geojson(value):
    return (json.dumps(value, ensure_ascii=False, indent=1) + "\n").encode()


def square(lon, lat, side):
    """A closed ring of about `side` metres around (lon, lat)."""
    d = side / 111_320 / 2
    return [[lon - d, lat - d], [lon + d, lat - d], [lon + d, lat + d], [lon - d, lat + d], [lon - d, lat - d]]


PARIS = [2.3522, 48.8566]
# The same, in RGF93 / Lambert-93 metres, from PROJ.
PARIS_L93 = [652469.023, 6862035.259]
LYON = [4.8357, 45.7640]

def l93(dx, dy):
    """A point `dx`, `dy` metres from Paris in Lambert-93."""
    return (PARIS_L93[0] + dx, PARIS_L93[1] + dy)


# Three layers of a Lambert-93 export under a folder: a survey point with
# its altitude, two paths of one record, and a parcel with a courtyard,
# its outer ring clockwise and its hole anticlockwise.
PARCELS = {
    "export/points": (SHP_POINT_Z, [("Borne", [[l93(0, 0)]])]),
    "export/chemins": (SHP_POLYLINE, [("Chemins", [
        [l93(-400, -300), l93(-200, -250), l93(0, -300)],
        [l93(-400, 300), l93(0, 300)],
    ])]),
    "export/parcelles": (SHP_POLYGON, [("AB 12", [
        [l93(-100, -100), l93(-100, 100), l93(100, 100), l93(100, -100), l93(-100, -100)],
        [l93(-30, -30), l93(30, -30), l93(30, 30), l93(-30, 30), l93(-30, -30)],
    ])]),
}

GEOJSON = {
    "point.geojson": feature({"type": "Point", "coordinates": PARIS}, {"name": "Paris"}),
    # A parcel-sized square with a square hole: the hole must stay a hole.
    "polygon-hole.geojson": collection(
        feature({"type": "Polygon", "coordinates": [square(*LYON, 100), square(*LYON, 30)]}, {"section": "AB", "numero": "12"}),
    ),
    # Two lines of a few kilometres, with properties of no interest.
    "multiline.geojson": feature(
        {"type": "MultiLineString", "coordinates": [
            [[4.80, 45.75], [4.81, 45.755], [4.82, 45.752], [4.83, 45.76]],
            [[4.80, 45.77], [4.83, 45.77]],
        ]},
        {"name": "paths", "length": 4200},
    ),
    # A point, a line and a polygon in one geometry.
    "collection.geojson": feature({"type": "GeometryCollection", "geometries": [
        {"type": "Point", "coordinates": LYON},
        {"type": "LineString", "coordinates": [[4.83, 45.76], [4.84, 45.77]]},
        {"type": "Polygon", "coordinates": [square(4.84, 45.76, 200)]},
    ]}),
    # From one corner of the map to the other, with a point past the
    # projection's latitude.
    "world.geojson": collection(
        feature({"type": "MultiPoint", "coordinates": [[-170, -80], [170, 80], [0, 89]]}),
        feature({"type": "Polygon", "coordinates": [[[113, -39], [153, -39], [153, -10], [113, -10], [113, -39]]]}),
    ),
    "empty.geojson": collection(),
    # What QGIS exports from a Lambert-93 layer: the legacy crs member, and
    # metres. Paris, and a 100 m square around it.
    "lambert93.geojson": dict(
        collection(
            feature({"type": "Point", "coordinates": PARIS_L93}, {"name": "Paris"}),
            feature({"type": "Polygon", "coordinates": [[
                [PARIS_L93[0] - 50, PARIS_L93[1] - 50], [PARIS_L93[0] + 50, PARIS_L93[1] - 50],
                [PARIS_L93[0] + 50, PARIS_L93[1] + 50], [PARIS_L93[0] - 50, PARIS_L93[1] + 50],
                [PARIS_L93[0] - 50, PARIS_L93[1] - 50],
            ]]}),
        ),
        crs={"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::2154"}},
    ),
    # A coordinate off the globe.
    "off-earth.geojson": feature({"type": "Point", "coordinates": [200, 95]}),
    # What geojson.io exports: simplestyle properties on each feature.
    "styled.geojson": collection(
        feature({"type": "Polygon", "coordinates": [square(*PARIS, 300)]}, {
            "stroke": "#ff0000", "stroke-width": 2, "stroke-opacity": 1,
            "fill": "#00ff00", "fill-opacity": 0.5, "title": "Plot", "description": "A plot of land",
        }),
        feature({"type": "Point", "coordinates": PARIS}, {
            "marker-color": "#0000ff", "marker-size": "large", "marker-symbol": "star",
        }),
    ),
    # Values a file could carry to paint the map or break the SVG.
    "hostile-style.geojson": collection(
        feature({"type": "Polygon", "coordinates": [square(*PARIS, 300)]}, {
            "stroke": "red", "stroke-width": 10000, "stroke-opacity": "1",
            "fill": "#12345", "fill-opacity": 7, "marker-color": 12, "marker-size": 3,
        }),
        feature({"type": "Point", "coordinates": PARIS}, {"marker-size": "huge\"/><script>", "marker-color": "#fff\""}),
        {"type": "Feature", "properties": None, "geometry": {"type": "Point", "coordinates": [2.36, 48.86]}},
    ),
}


def main():
    photo = jpeg(600, 300)
    small_png = png(64, 32)
    files = {
        "photo.jpg": photo,
        "photo.png": small_png,
        "rotated.jpg": with_orientation(photo, 6),
        "truncated.jpg": photo[: len(photo) * 2 // 5],
        # The JPEG covers a page a tenth of its size, so the extracted image
        # and a page render differ in resolution, not in shape.
        "image.pdf": image_pdf(photo, 600, 300, 60, 30, "60 0 0 30 0 0"),
        "mixed.pdf": image_pdf(photo, 600, 300, 60, 30, "60 0 0 30 0 0", text=LONG_TEXT),
        "small.pdf": image_pdf(photo, 600, 300, 60, 30, "6 0 0 3 0 0"),
        "rotated.pdf": image_pdf(photo, 600, 300, 30, 60, "0 60 -30 0 30 0"),
        "flate.pdf": image_pdf(zlib.compress(bytes(range(24))), 4, 2, 4, 2, "4 0 0 2 0 0", filt="/FlateDecode"),
        "text.pdf": text_pdf(200, 100, "Hello, agrafe"),
        "image.docx": docx("", "image1.png", small_png),
        "text.docx": docx(LONG_TEXT, "image1.png", small_png),
        "vector.docx": docx("", "image1.emf", b"\x01\x00\x00\x00" + b"\x00" * 60),
        "image.odt": odt("", small_png),
        # Real documents, for the converter: two pages, a letter, a table.
        "pages.docx": docx("Page {page} of a two-page report.", pages=2),
        "broken.docx": broken_docx(),
        "text.odt": odt("Bonjour, ceci est une lettre."),
        "table.xlsx": xlsx(TABLE),
        "table.ods": ods(TABLE),
        # The variants, RTF, presentations and a drawing, for the converter:
        # two slides so that the one-page export is checked there too.
        "table.xlsm": xlsx(TABLE, macro_enabled=True),
        "letter.rtf": rtf("Bonjour, ceci est une lettre."),
        "slides.pptx": pptx(["Diapositive 1", "Diapositive 2"]),
        "broken.pptx": broken_pptx(),
        "slides.odp": odp(["Diapositive 1", "Diapositive 2"]),
        "drawing.odg": odg(2),
        # The formats LibreOffice reads through its import libraries that
        # can be written here: a letter, a drawing, a plan.
        "letter.wpd": wordperfect("Bonjour, ceci est une lettre."),
        "letter.abw": abiword("Bonjour, ceci est une lettre."),
        "drawing.vsdx": vsdx("Plan"),
        "plan.dxf": dxf([(0, 0, 200, 0), (200, 0, 200, 100), (200, 100, 0, 100), (0, 100, 0, 0), (0, 0, 200, 100)]),
    }
    files.update({name: geojson(value) for name, value in GEOJSON.items()})
    files["places.kmz"] = kmz((HERE / "places.kml").read_bytes(), small_png)
    files["empty.kmz"] = kmz((HERE / "empty.kml").read_bytes())
    files["parcels.zip"] = zipped_shapefile(PARCELS)
    files["parcels-no-prj.zip"] = zipped_shapefile(PARCELS, prj=None)
    for name, data in files.items():
        (HERE / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes")
    # The Office 97-2003 files, from the ZIP ones through LibreOffice.
    command = soffice()
    for name, source, export in [
        ("letter.doc", "text.odt", "doc:MS Word 97"),
        ("table.xls", "table.xlsx", "xls:MS Excel 97"),
        ("slides.ppt", "slides.pptx", "ppt:MS PowerPoint 97"),
    ]:
        if (HERE / name).exists() and "--legacy" not in sys.argv:
            continue
        if command is None:
            print(f"{name}: not written, no LibreOffice found")
            continue
        data = legacy(command, HERE / source, export)
        (HERE / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes, from {source}")
    # The iWork files, from libetonyek's corpus.
    for name, digest in CORPUS.items():
        if (HERE / name).exists() and "--legacy" not in sys.argv:
            continue
        data = corpus(name, digest)
        (HERE / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes, from libetonyek")
    # What a worker renders at startup to check its vips and helper binary.
    PROBE.mkdir(exist_ok=True)
    probe_files = {
        "pixel.png": png(1, 1),
        # One file for each loader on the allowlist that libvips may be
        # built without, or whose codec Debian packages apart.
        "pixel.gif": raster("GIF", 1, 1),
        "pixel.webp": raster("WEBP", 1, 1),
        "pixel.tiff": raster("TIFF", 1, 1),
        "pixel.avif": raster("AVIF", 8, 8),
        "pixel.jp2": raster("JPEG2000", 8, 8),
        "page.pdf": empty_pdf(20, 10),
        # A JPEG covering a one-page PDF, for agrafe-extract to pull out.
        "image.pdf": image_pdf(jpeg(16, 8), 16, 8, 16, 8, "16 0 0 8 0 0"),
        "point.geojson": geojson(feature({"type": "Point", "coordinates": PARIS})),
        # Through LibreOffice when one is configured, one file per
        # application: the DOCX, the XLSX, the PPTX and the ODG have two
        # pages so that the one-page export is checked on each filter, and
        # the ODT carries a macro that must not run.
        "pages.docx": docx("Page {page}", pages=2),
        "sheet.xlsx": xlsx([["a", "b"], ["1", "2"]], break_after=1),
        "slides.pptx": pptx(["Slide 1", "Slide 2"]),
        "drawing.odg": odg(2),
        "macro.odt": macro_odt(),
    }
    for name, data in probe_files.items():
        (PROBE / name).write_bytes(data)
        print(f"probe/{name}: {len(data)} bytes")
    cert, key = HERE / "self-signed.crt.pem", HERE / "self-signed.key.pem"
    if not cert.exists() or "--legacy" in sys.argv:
        if shutil.which("openssl") is None:
            print("self-signed.crt.pem: not written, no openssl found")
        else:
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
                            "-nodes", "-days", "36500", "-subj", "/CN=localhost",
                            "-addext", "subjectAltName=IP:127.0.0.1,DNS:localhost",
                            "-keyout", str(key), "-out", str(cert)],
                           check=True, capture_output=True, timeout=60)
            print(f"self-signed.crt.pem: {cert.stat().st_size} bytes")
    # What Pillow cannot write without a plugin: the probe's HEIC and JPEG
    # XL, and a JPEG XL photo for the tests, the rotated JPEG made upright
    # (300x600) by `vips thumbnail`, which applies its orientation.
    for name, source, args in [
        (PROBE / "pixel.heic", PROBE / "pixel.png", ["copy", "{source}", "{name}[compression=hevc]"]),
        (PROBE / "pixel.jxl", PROBE / "pixel.png", ["copy", "{source}", "{name}"]),
        (HERE / "photo.jxl", HERE / "rotated.jpg", ["thumbnail", "{source}", "{name}", "600"]),
    ]:
        label = f"probe/{name.name}" if name.parent == PROBE else name.name
        if name.exists() and "--legacy" not in sys.argv:
            continue
        if shutil.which("vips") is None:
            print(f"{label}: not written, no vips found")
            continue
        command = ["vips"] + [arg.format(source=source, name=name) for arg in args]
        subprocess.run(command, check=True, capture_output=True, timeout=60)
        print(f"{label}: {name.stat().st_size} bytes, from {source.name}")


if __name__ == "__main__":
    main()
