<?xml version="1.0" encoding="UTF-8"?>
<!-- Hand-written for agrafe: features with no geometry, nothing to draw. -->
<wfs:FeatureCollection
    xmlns:wfs="http://www.opengis.net/wfs/2.0"
    xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:cadastre="https://example.org/cadastre"
    numberMatched="2" numberReturned="2">
  <wfs:boundedBy>
    <gml:Envelope srsName="urn:ogc:def:crs:EPSG::2154">
      <gml:lowerCorner>652300 6861900</gml:lowerCorner>
      <gml:upperCorner>652700 6862200</gml:upperCorner>
    </gml:Envelope>
  </wfs:boundedBy>
  <wfs:member>
    <cadastre:Lieu gml:id="lieu.1">
      <cadastre:nom>Lieu-dit</cadastre:nom>
    </cadastre:Lieu>
  </wfs:member>
  <wfs:member>
    <cadastre:Lieu gml:id="lieu.2">
      <cadastre:nom>Autre lieu-dit</cadastre:nom>
      <cadastre:geometrie/>
    </cadastre:Lieu>
  </wfs:member>
</wfs:FeatureCollection>
