-- noinspection SqlNoDataSourceInspectionForFile
-- noinspection SqlResolveForFile
CREATE SINK nexmark_q103 AS
SELECT
    a.id AS auction_id,
    a.item_name AS auction_item_name
FROM auction a
WHERE a.id IN (
    SELECT b.auction FROM bid b
    GROUP BY b.auction
    HAVING COUNT(*) >= 20
)
WITH ( connector = 'blackhole', type = 'append-only', force_append_only = 'true');
