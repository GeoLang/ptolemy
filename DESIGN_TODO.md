# Open items

## having: verify against a live Esri client

The `having` grammar follows Esri's REST docs (aggregate functions over source
fields, both `having` and `havingClause` spellings). No live client emission
was ever captured: sampleserver6 advertises `supportsHavingClause` but refuses
every having request, and the docs contradict themselves on whether having
aggregates must appear in `outStatistics` (we allow either). Before advertising
Esri compatibility for dashboards, capture one real request from ArcGIS JS API
or Dashboards and check it parses.

## verne against private ptolemy: partially closed

Ptolemy now accepts `X-Esri-Authorization` on facade paths, so
`VERNE_ARCGIS_TOKEN` with a ptolemy JWT works for extraction. Untested live;
the change-tracking gate ran against a public dataset.
