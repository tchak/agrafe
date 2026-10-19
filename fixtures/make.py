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


def docx(text, media_name, media):
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        z.writestr("_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        paragraphs = f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>" if text else ""
        z.writestr("word/document.xml", f'<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>{paragraphs}<w:p><w:r><w:drawing/></w:r></w:p></w:body></w:document>')
        z.writestr("word/_rels/document.xml.rels", f'<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/{media_name}"/></Relationships>')
        z.writestr(f"word/media/{media_name}", media)
    return out.getvalue()


def odt(text, media):
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        z.writestr("mimetype", "application/vnd.oasis.opendocument.text", compress_type=zipfile.ZIP_STORED)
        z.writestr("META-INF/manifest.xml", '<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="Pictures/1.png" manifest:media-type="image/png"/></manifest:manifest>', compress_type=zipfile.ZIP_DEFLATED)
        paragraph = f"<text:p>{text}</text:p>" if text else ""
        z.writestr("content.xml", f'<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text>{paragraph}<text:p><draw:frame><draw:image xlink:href="Pictures/1.png"/></draw:frame></text:p></office:text></office:body></office:document-content>', compress_type=zipfile.ZIP_DEFLATED)
        z.writestr("Pictures/1.png", media, compress_type=zipfile.ZIP_DEFLATED)
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
    }
    for name, data in probe_files.items():
        (PROBE / name).write_bytes(data)
        print(f"probe/{name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
