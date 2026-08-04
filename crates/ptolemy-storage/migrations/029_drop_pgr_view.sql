-- the view cast junction uuids text->bigint, which fails on any real row; the
-- routing queries rank uuids to bigints per statement instead and nothing
-- reads the view
DROP VIEW IF EXISTS pgr_network_edges;
