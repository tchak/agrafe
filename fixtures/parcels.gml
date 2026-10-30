<?xml version="1.0" encoding="UTF-8"?>
<!-- Hand-written for agrafe: a WFS 2.0 response in GML 3.2, around the
     Hôtel de Ville in Paris, in Lambert-93 named on the envelope only. -->
<wfs:FeatureCollection
    xmlns:wfs="http://www.opengis.net/wfs/2.0"
    xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:cadastre="https://example.org/cadastre"
    numberMatched="5" numberReturned="5" timeStamp="2026-10-30T12:00:00Z">
  <wfs:boundedBy>
    <gml:Envelope srsName="urn:ogc:def:crs:EPSG::2154">
      <gml:lowerCorner>652300 6861900</gml:lowerCorner>
      <gml:upperCorner>652700 6862200</gml:upperCorner>
    </gml:Envelope>
  </wfs:boundedBy>
  <wfs:member>
    <cadastre:Parcelle gml:id="parcelle.1">
      <cadastre:numero>AB 12</cadastre:numero>
      <cadastre:geometrie>
        <gml:MultiSurface gml:id="parcelle.1.geom">
          <gml:surfaceMember>
            <gml:Polygon gml:id="parcelle.1.geom.0">
              <gml:exterior>
                <gml:LinearRing>
                  <gml:posList>652400 6861950 652500 6861950 652500 6862050 652400 6862050 652400 6861950</gml:posList>
                </gml:LinearRing>
              </gml:exterior>
              <gml:interior>
                <gml:LinearRing>
                  <gml:posList>652430 6861990 652430 6862030 652470 6862030 652470 6861990 652430 6861990</gml:posList>
                </gml:LinearRing>
              </gml:interior>
            </gml:Polygon>
          </gml:surfaceMember>
        </gml:MultiSurface>
      </cadastre:geometrie>
    </cadastre:Parcelle>
  </wfs:member>
  <wfs:member>
    <cadastre:Chemin gml:id="chemin.1">
      <cadastre:nom>Chemin des quais</cadastre:nom>
      <cadastre:geometrie>
        <gml:Curve gml:id="chemin.1.geom">
          <gml:segments>
            <gml:LineStringSegment interpolation="linear">
              <gml:posList>652320 6861920 652380 6861960 652420 6861930</gml:posList>
            </gml:LineStringSegment>
            <gml:LineStringSegment interpolation="linear">
              <gml:posList>652420 6861930 652520 6861940 652600 6861910</gml:posList>
            </gml:LineStringSegment>
          </gml:segments>
        </gml:Curve>
      </cadastre:geometrie>
    </cadastre:Chemin>
  </wfs:member>
  <wfs:member>
    <cadastre:Borne gml:id="borne.1">
      <cadastre:nom>Borne</cadastre:nom>
      <cadastre:geometrie>
        <gml:Point gml:id="borne.1.geom">
          <gml:pos>652469 6862035</gml:pos>
        </gml:Point>
      </cadastre:geometrie>
    </cadastre:Borne>
  </wfs:member>
  <wfs:member>
    <cadastre:Conduite gml:id="conduite.1">
      <cadastre:nom>Conduite</cadastre:nom>
      <cadastre:geometrie>
        <gml:LineString gml:id="conduite.1.geom" srsDimension="3">
          <gml:posList>652550 6862100 33.5 652650 6862150 34 652680 6862180 34.5</gml:posList>
        </gml:LineString>
      </cadastre:geometrie>
    </cadastre:Conduite>
  </wfs:member>
  <wfs:member>
    <cadastre:Lieu gml:id="lieu.1">
      <cadastre:nom>Lieu-dit sans géométrie</cadastre:nom>
      <cadastre:proprietaire xlink:href="#proprietaire.1"/>
    </cadastre:Lieu>
  </wfs:member>
</wfs:FeatureCollection>
