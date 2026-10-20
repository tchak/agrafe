#!/usr/bin/env python3
"""Builds the fixture files next to this script, and the renderer probe
files under crates/agrafe-render/assets/probe. Needs Pillow. The outputs are
committed; run this only to change them."""

import io
import json
import struct
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


def xlsx(rows):
    """A workbook of one sheet holding `rows`, lists of strings, as inline
    strings."""
    def cell(col, row, value):
        return f'<c r="{chr(65 + col)}{row}" t="inlineStr"><is><t>{value}</t></is></c>'
    data = "".join(
        f'<row r="{r + 1}">' + "".join(cell(c, r + 1, v) for c, v in enumerate(row)) + "</row>"
        for r, row in enumerate(rows)
    )
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        zip_entry(z, "[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
        zip_entry(z, "_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        zip_entry(z, "xl/workbook.xml", '<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Feuille1" sheetId="1" r:id="rId1"/></sheets></workbook>')
        zip_entry(z, "xl/_rels/workbook.xml.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
        zip_entry(z, "xl/worksheets/sheet1.xml", f'<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>')
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
        {"name": "chemins", "length": 4200},
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
            "fill": "#00ff00", "fill-opacity": 0.5, "title": "Parcelle", "description": "Une parcelle",
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
        "text.odt": odt("Bonjour, ceci est une lettre."),
        "table.xlsx": xlsx(TABLE),
        "table.ods": ods(TABLE),
    }
    files.update({name: geojson(value) for name, value in GEOJSON.items()})
    for name, data in files.items():
        (HERE / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes")
    # What a worker renders at startup to check its vips and helper binary.
    PROBE.mkdir(exist_ok=True)
    probe_files = {
        "pixel.png": png(1, 1),
        "page.pdf": empty_pdf(20, 10),
        "point.geojson": geojson(feature({"type": "Point", "coordinates": PARIS})),
        # Through LibreOffice when one is configured: the DOCX has two pages
        # so that the one-page export is checked.
        "pages.docx": docx("Page {page}", pages=2),
        "sheet.xlsx": xlsx([["a", "b"], ["1", "2"]]),
    }
    for name, data in probe_files.items():
        (PROBE / name).write_bytes(data)
        print(f"probe/{name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
