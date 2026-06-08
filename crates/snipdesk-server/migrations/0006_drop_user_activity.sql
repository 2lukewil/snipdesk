-- Drop the pre-aggregated user_activity table from 0001.
--
-- It was meant as an O(1) cache for the dashboard's per-user snippet
-- counts but was never wired up - every read path uses live SELECTs
-- against personal_snippets + library_usage, and the v1.0 audit
-- decided that's fast enough at the scale we expect. Keeping a
-- table that nothing populates is just a future-foot-gun (any code
-- that later assumes it's populated will read zeros).
--
-- No data loss: the table is empty in every deployment because no
-- code path ever inserted into it.

DROP TABLE IF EXISTS user_activity;
