-- ---------------------------------------------------------------------------
-- Feldspar: rename the metadata tables from the `_sc_` prefix to `_fd_`.
-- ---------------------------------------------------------------------------
--
-- WHY
--   Saltcorn v1 keeps its own metadata in `_sc_*` tables. A transition project
--   runs v1 and Feldspar against the same schema, so the two servers cannot
--   both own that prefix: Feldspar bootstrapping `_sc_config` would write into
--   v1's row store, and `Table::is_system` would hide v1's tables from the very
--   admin who came to look at them. Feldspar now reserves `_fd_` instead, and
--   treats `_sc_*` as ordinary tables it can be pointed at like any other.
--
-- WHAT TO RUN
--   Postgres primary  -> section 1 (and 2, if the database also serves an
--                        application whose tables live here).
--   SQLite primary    -> section 3.
--   SQLite non-primary databases and SQLite file stores -> section 4, in each
--                        such file (they carry `_sc_object_comments`).
--
--   Then, for a database older than 2026-09-26, the stored-data upgrade in
--   section 5 (Postgres) or 6 (SQLite), after the rename. It is not part of the
--   rename; it lives here so that one file brings an older installation up to
--   date. Likewise, for a database older than 2026-09-29 with predictive
--   models, section 7 (Postgres) or 8 (SQLite): models' datasets become named
--   datasets.
--
--   Take a backup first. Run each section as one transaction, with the server
--   stopped: Feldspar caches the catalog in memory and will not notice a table
--   changing name underneath it.
--
--   Run it against a Feldspar-only database, BEFORE anything of v1's is put in
--   the same schema. The rename cannot tell whose `_sc_config` it is looking at,
--   so on a schema that already holds v1's tables it would rename those instead.
--   Re-running it on an already-migrated database is a no-op.
--
-- SCOPE
--   Renames the tables, and the indexes / constraints / sequences / triggers
--   that carry the old prefix in their own names (Postgres does *not* rename
--   those when a table is renamed, and `_sc_config_pkey` would collide with
--   v1's index of that name in a shared schema). Nothing in the stored *data*
--   needs rewriting: `_fd_tables` and `_fd_fields` refuse rows for system
--   tables, so no row anywhere names one.
--
-- WHAT THIS DOES NOT COVER — read this before sharing a schema with v1
--   * `users` is not prefixed in either system, and Feldspar's `users` table is
--     NOT the same shape as v1's. Two servers cannot share one schema until one
--     of them is moved: put v1 and Feldspar in separate Postgres *schemas* (the
--     `search_path` / `schema` setting on each connection), or in separate
--     databases. This rename removes the `_sc_*` collision; it does not remove
--     that one.
--   * Application tables (`books`, `orders`, …) are unprefixed and collide by
--     name, with the constraints and indexes Feldspar derives for them
--     (`sc_uq_*`, `sc_ix_*`, `sc_fts_*`, `sc_ck_*`) collide alongside. Same
--     answer: separate schemas.
--   * Feldspar keeps its own name for a few things on purpose, and they are not
--     touched here: `@saltcorn/…` (the npm scope v1's modules are published
--     under), `globalThis.saltcorn` (the API a v1 module is handed at run time),
--     and `saltcorn_constraint` (the key Feldspar's constraint metadata sits
--     under inside an object's comment — comments are per-object, so it cannot
--     collide).
--
-- ---------------------------------------------------------------------------
-- 1. Postgres: the primary database's metadata tables.
-- ---------------------------------------------------------------------------

BEGIN;

ALTER TABLE IF EXISTS "_sc_tables"            RENAME TO "_fd_tables";
ALTER TABLE IF EXISTS "_sc_fields"            RENAME TO "_fd_fields";
ALTER TABLE IF EXISTS "_sc_triggers"          RENAME TO "_fd_triggers";
ALTER TABLE IF EXISTS "_sc_workflow_versions" RENAME TO "_fd_workflow_versions";
ALTER TABLE IF EXISTS "_sc_agents"            RENAME TO "_fd_agents";
ALTER TABLE IF EXISTS "_sc_llm_providers"     RENAME TO "_fd_llm_providers";
ALTER TABLE IF EXISTS "_sc_runs"              RENAME TO "_fd_runs";
ALTER TABLE IF EXISTS "_sc_run_traces"        RENAME TO "_fd_run_traces";
ALTER TABLE IF EXISTS "_sc_config"            RENAME TO "_fd_config";
ALTER TABLE IF EXISTS "_sc_acme_cache"        RENAME TO "_fd_acme_cache";
ALTER TABLE IF EXISTS "_sc_applications"      RENAME TO "_fd_applications";
ALTER TABLE IF EXISTS "_sc_models"            RENAME TO "_fd_models";
ALTER TABLE IF EXISTS "_sc_model_instances"   RENAME TO "_fd_model_instances";
ALTER TABLE IF EXISTS "_sc_modules"           RENAME TO "_fd_modules";
ALTER TABLE IF EXISTS "_sc_roles"             RENAME TO "_fd_roles";
ALTER TABLE IF EXISTS "_sc_sessions"          RENAME TO "_fd_sessions";
ALTER TABLE IF EXISTS "_sc_api_tokens"        RENAME TO "_fd_api_tokens";
ALTER TABLE IF EXISTS "_sc_db_connections"    RENAME TO "_fd_db_connections";
ALTER TABLE IF EXISTS "_sc_file_stores"       RENAME TO "_fd_file_stores";
-- Left behind by the one-time session grants `feldspar auth token` used to
-- need (removed; see TECHNICAL_DESIGN "It writes the session itself"). Nothing
-- reads it any more, but a database that ever had it still does, and an
-- `_sc_*` table is an ordinary table to Feldspar — so it would be listed as
-- one. Renamed with the rest, it is a hidden metadata table like them.
ALTER TABLE IF EXISTS "_sc_session_grants"    RENAME TO "_fd_session_grants";
-- Present only in a SQLite database (Postgres has native COMMENT ON), listed
-- here so the set is complete; it is a no-op on Postgres.
ALTER TABLE IF EXISTS "_sc_object_comments"   RENAME TO "_fd_object_comments";

-- The objects hanging off those tables. Postgres leaves their names alone when
-- a table is renamed, so `_fd_config` would still be indexed by
-- `_sc_config_pkey` — the name v1's own `_sc_config` wants. Each rename
-- rewrites the *first* `_sc_` in the name, which is also what turns the derived
-- constraint name `sc_uq__sc_workflow_versions_workflow_version` into
-- `sc_uq__fd_workflow_versions_workflow_version`, the name Feldspar derives
-- for it now.
DO $rename$
DECLARE
    r record;
BEGIN
    -- Table constraints (primary keys, unique keys, checks, foreign keys).
    FOR r IN
        SELECT t.relname AS tbl, c.conname AS old
          FROM pg_constraint c
          JOIN pg_class t ON t.oid = c.conrelid
          JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE n.nspname = current_schema()
           AND t.relname LIKE '\_fd\_%'
           AND c.conname LIKE '%\_sc\_%'
    LOOP
        EXECUTE format(
            'ALTER TABLE %I RENAME CONSTRAINT %I TO %I',
            r.tbl, r.old, regexp_replace(r.old, '_sc_', '_fd_')
        );
    END LOOP;

    -- Indexes that are not backed by a constraint (a constraint's index was
    -- renamed with the constraint above).
    FOR r IN
        SELECT i.relname AS old
          FROM pg_index x
          JOIN pg_class i ON i.oid = x.indexrelid
          JOIN pg_class t ON t.oid = x.indrelid
          JOIN pg_namespace n ON n.oid = i.relnamespace
         WHERE n.nspname = current_schema()
           AND t.relname LIKE '\_fd\_%'
           AND i.relname LIKE '%\_sc\_%'
           AND NOT EXISTS (
               SELECT 1 FROM pg_constraint c WHERE c.conindid = i.oid
           )
    LOOP
        EXECUTE format(
            'ALTER INDEX %I RENAME TO %I',
            r.old, regexp_replace(r.old, '_sc_', '_fd_')
        );
    END LOOP;

    -- Sequences owned by a column of one of those tables (serial / identity).
    FOR r IN
        SELECT s.relname AS old
          FROM pg_class s
          JOIN pg_depend d ON d.objid = s.oid AND d.classid = 'pg_class'::regclass
          JOIN pg_class t ON t.oid = d.refobjid
          JOIN pg_namespace n ON n.oid = s.relnamespace
         WHERE s.relkind = 'S'
           AND n.nspname = current_schema()
           AND t.relname LIKE '\_fd\_%'
           AND s.relname LIKE '%\_sc\_%'
    LOOP
        EXECUTE format(
            'ALTER SEQUENCE %I RENAME TO %I',
            r.old, regexp_replace(r.old, '_sc_', '_fd_')
        );
    END LOOP;

    -- Triggers on those tables.
    FOR r IN
        SELECT t.relname AS tbl, g.tgname AS old
          FROM pg_trigger g
          JOIN pg_class t ON t.oid = g.tgrelid
          JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE NOT g.tgisinternal
           AND n.nspname = current_schema()
           AND t.relname LIKE '\_fd\_%'
           AND g.tgname LIKE '%\_sc\_%'
    LOOP
        EXECUTE format(
            'ALTER TRIGGER %I ON %I RENAME TO %I',
            r.old, r.tbl, regexp_replace(r.old, '_sc_', '_fd_')
        );
    END LOOP;
END
$rename$;

COMMIT;

-- Check: this should return no rows.
--
--   SELECT c.relname, c.relkind
--     FROM pg_class c
--     JOIN pg_namespace n ON n.oid = c.relnamespace
--    WHERE n.nspname = current_schema()
--      AND c.relname LIKE '\_sc\_%';

-- ---------------------------------------------------------------------------
-- 2. Postgres: a non-primary application database.
-- ---------------------------------------------------------------------------
--
-- Nothing to do. Only the primary carries `_fd_*` tables; a connected
-- application database holds application tables and nothing of Feldspar's.

-- ---------------------------------------------------------------------------
-- 3. SQLite primary.
-- ---------------------------------------------------------------------------
--
-- SQLite has no `ALTER TABLE IF EXISTS`, so run only the lines for tables the
-- file actually has (`.tables` in the sqlite3 shell lists them). SQLite renames
-- a table's indexes with it, so there is no second pass.
--
--   BEGIN;
--   ALTER TABLE "_sc_tables"            RENAME TO "_fd_tables";
--   ALTER TABLE "_sc_fields"            RENAME TO "_fd_fields";
--   ALTER TABLE "_sc_triggers"          RENAME TO "_fd_triggers";
--   ALTER TABLE "_sc_workflow_versions" RENAME TO "_fd_workflow_versions";
--   ALTER TABLE "_sc_agents"            RENAME TO "_fd_agents";
--   ALTER TABLE "_sc_llm_providers"     RENAME TO "_fd_llm_providers";
--   ALTER TABLE "_sc_runs"              RENAME TO "_fd_runs";
--   ALTER TABLE "_sc_run_traces"        RENAME TO "_fd_run_traces";
--   ALTER TABLE "_sc_config"            RENAME TO "_fd_config";
--   ALTER TABLE "_sc_acme_cache"        RENAME TO "_fd_acme_cache";
--   ALTER TABLE "_sc_applications"      RENAME TO "_fd_applications";
--   ALTER TABLE "_sc_models"            RENAME TO "_fd_models";
--   ALTER TABLE "_sc_model_instances"   RENAME TO "_fd_model_instances";
--   ALTER TABLE "_sc_modules"           RENAME TO "_fd_modules";
--   ALTER TABLE "_sc_roles"             RENAME TO "_fd_roles";
--   ALTER TABLE "_sc_sessions"          RENAME TO "_fd_sessions";
--   ALTER TABLE "_sc_api_tokens"        RENAME TO "_fd_api_tokens";
--   ALTER TABLE "_sc_db_connections"    RENAME TO "_fd_db_connections";
--   ALTER TABLE "_sc_file_stores"       RENAME TO "_fd_file_stores";
--   ALTER TABLE "_sc_session_grants"    RENAME TO "_fd_session_grants";
--   ALTER TABLE "_sc_object_comments"   RENAME TO "_fd_object_comments";
--   COMMIT;

-- ---------------------------------------------------------------------------
-- 4. Every other SQLite file Feldspar has ever written a comment into.
-- ---------------------------------------------------------------------------
--
-- SQLite has no `COMMENT ON`, so the SQLite driver keeps object comments — a
-- unique constraint's error message, a row constraint's formula, and the
-- `saltcorn_constraint` metadata that says a constraint is Feldspar's — in a
-- side table of that file's own. It is created on demand, in whichever SQLite
-- database a comment was set in: a connected application database as well as
-- the primary. Run this in each:
--
--   ALTER TABLE "_sc_object_comments" RENAME TO "_fd_object_comments";
--
-- Skip a file where the table is absent: nothing there has a comment, and
-- Feldspar creates it under the new name the first time one is set. Missing
-- this step is not silent — introspection reads no comments back, so that
-- database's constraints lose their messages and stop being recognised as
-- Feldspar's, and the next comment set there recreates the table empty.

-- ---------------------------------------------------------------------------
-- 5. Postgres: custom queries store their source as `code` (2026-09-26).
-- ---------------------------------------------------------------------------
--
-- A custom query can now be JavaScript or Python as well as SQL, so its source
-- field is `code` rather than `sql` (a query with no `language` is still SQL).
-- The queries live inside the application's `apis` JSON, at
-- `apis[*].config.queries[*]`, and a stored query that still says `sql` makes
-- every read of that application's API fail with:
--
--   the API's `queries` setting is not a list of custom queries:
--   missing field `code`
--
-- This renames the key in place, keeping the order of the APIs and of their
-- queries. Run it after section 1, since it names `_fd_applications`. It
-- touches only rows that still hold a `sql` key, so re-running it is a no-op,
-- and it does nothing on a database without an applications table.

DO $code$
BEGIN
    IF to_regclass('"_fd_applications"') IS NULL THEN
        RETURN;
    END IF;

    UPDATE "_fd_applications" a
       SET apis = (
           SELECT jsonb_agg(
                    CASE WHEN jsonb_typeof(api #> '{config,queries}') = 'array'
                         THEN jsonb_set(api, '{config,queries}', COALESCE((
                                SELECT jsonb_agg(
                                         CASE WHEN q ? 'sql' AND NOT q ? 'code'
                                              THEN (q - 'sql')
                                                   || jsonb_build_object('code', q -> 'sql')
                                              ELSE q END
                                         ORDER BY qi)
                                  FROM jsonb_array_elements(api #> '{config,queries}')
                                       WITH ORDINALITY AS qs(q, qi)), '[]'::jsonb))
                         ELSE api END
                    ORDER BY ai)
             FROM jsonb_array_elements(a.apis) WITH ORDINALITY AS aps(api, ai))
     WHERE jsonb_typeof(a.apis) = 'array'
       AND EXISTS (
           SELECT 1
             FROM jsonb_array_elements(a.apis) api,
                  jsonb_array_elements(
                      CASE WHEN jsonb_typeof(api #> '{config,queries}') = 'array'
                           THEN api #> '{config,queries}' ELSE '[]'::jsonb END) q
            WHERE q ? 'sql');
END
$code$;

-- Check: this should return 0.
--
--   SELECT count(*)
--     FROM "_fd_applications" a,
--          jsonb_array_elements(a.apis) api,
--          jsonb_array_elements(COALESCE(api #> '{config,queries}', '[]')) q
--    WHERE q ? 'sql';

-- ---------------------------------------------------------------------------
-- 6. SQLite primary: the same upgrade as section 5.
-- ---------------------------------------------------------------------------
--
-- SQLite stores `apis` as JSON text. Run after section 3; skip it if the file
-- has no `_fd_applications`. Re-running it is a no-op.
--
--   UPDATE "_fd_applications"
--      SET apis = (
--          SELECT json_group_array(json(
--                   CASE WHEN json_type(api.value, '$.config.queries') = 'array'
--                        THEN json_set(api.value, '$.config.queries', json((
--                               SELECT json_group_array(json(
--                                        CASE WHEN json_type(q.value, '$.sql') IS NOT NULL
--                                              AND json_type(q.value, '$.code') IS NULL
--                                             THEN json_set(json_remove(q.value, '$.sql'),
--                                                           '$.code', json_extract(q.value, '$.sql'))
--                                             ELSE q.value END))
--                                 FROM json_each(api.value, '$.config.queries') q)))
--                        ELSE api.value END))
--            FROM json_each("_fd_applications".apis) api)
--    WHERE EXISTS (
--          SELECT 1
--            FROM json_each("_fd_applications".apis) api,
--                 json_each(api.value, '$.config.queries') q
--           WHERE json_type(q.value, '$.sql') IS NOT NULL);

-- ---------------------------------------------------------------------------
-- 7. Postgres: models' datasets become named datasets (2026-09-29).
-- ---------------------------------------------------------------------------
--
-- A model's dataset used to be written on the model, as `{ table, columns,
-- filter, order }` in `_fd_models.dataset` (and the same inside each entry of
-- `related`). It is now a named dataset of its own, in `_fd_datasets`, and the
-- model holds `{ "dataset_id": … }` (analytics TODO A1.8). This creates one
-- dataset per old-style dataset — a Calculated column per column, the Filter,
-- the Sort and a Select columns keeping exactly those columns, which reads the
-- same rows as before — names it after the model ("House prices — data", or
-- "Radon — counties" for a related one), and points the model at it.
--
-- Run after section 1, with the server stopped. Re-running it is a no-op: a
-- model whose dataset is already a reference is left alone. Fits made before it
-- keep working; they only cannot say whether the dataset has changed since.

BEGIN;

CREATE TABLE IF NOT EXISTS "_fd_datasets" (
    "id" uuid NOT NULL PRIMARY KEY,
    "name" text NOT NULL UNIQUE,
    "description" text,
    "base" jsonb NOT NULL,
    "operations" jsonb NOT NULL,
    "attributes" jsonb NOT NULL
);

-- The operations an old-style dataset is.
CREATE OR REPLACE FUNCTION pg_temp.fd_dataset_operations(ds jsonb) RETURNS jsonb
LANGUAGE sql IMMUTABLE AS $fn$
    SELECT COALESCE(jsonb_agg(op ORDER BY ord), '[]'::jsonb) FROM (
        SELECT c.i AS ord,
               jsonb_build_object('id', 'c' || c.i, 'enabled', true, 'kind', 'calculated',
                   'params', jsonb_build_object('name', c.v->>'name', 'formula', c.v->>'expr')) AS op
          FROM jsonb_array_elements(COALESCE(ds->'columns', '[]'::jsonb)) WITH ORDINALITY AS c(v, i)
        UNION ALL
        SELECT 1000000,
               jsonb_build_object('id', 'filter', 'enabled', true, 'kind', 'filter',
                   'params', jsonb_build_object('formula', ds->>'filter'))
         WHERE COALESCE(ds->>'filter', '') <> ''
        UNION ALL
        SELECT 1000001,
               jsonb_build_object('id', 'sort', 'enabled', true, 'kind', 'sort',
                   'params', jsonb_build_object('keys', (
                       SELECT jsonb_agg(jsonb_build_object(
                                  'formula', o.v->>'expr',
                                  'descending', COALESCE((o.v->>'descending')::boolean, false))
                              ORDER BY o.i)
                         FROM jsonb_array_elements(ds->'order') WITH ORDINALITY AS o(v, i))))
         WHERE jsonb_typeof(ds->'order') = 'array' AND jsonb_array_length(ds->'order') > 0
        UNION ALL
        SELECT 1000002,
               jsonb_build_object('id', 'select', 'enabled', true, 'kind', 'select',
                   'params', jsonb_build_object('columns', (
                       SELECT jsonb_agg(jsonb_build_object('column', c.v->>'name') ORDER BY c.i)
                         FROM jsonb_array_elements(ds->'columns') WITH ORDINALITY AS c(v, i))))
         WHERE jsonb_array_length(COALESCE(ds->'columns', '[]'::jsonb)) > 0
    ) ops
$fn$;

-- The main datasets.
WITH legacy AS (
    SELECT m.id AS model_id, m.name AS model_name, m.dataset AS ds,
           gen_random_uuid() AS dataset_id
      FROM "_fd_models" m
     WHERE m.dataset ? 'table'
), made AS (
    INSERT INTO "_fd_datasets" (id, name, description, base, operations, attributes)
    SELECT dataset_id, model_name || ' — data', '',
           jsonb_build_object('kind', 'table', 'table', ds->>'table'),
           pg_temp.fd_dataset_operations(ds), '{}'::jsonb
      FROM legacy
    RETURNING id
)
UPDATE "_fd_models" m
   SET dataset = jsonb_build_object('dataset_id', l.dataset_id)
  FROM legacy l
 WHERE m.id = l.model_id;

-- The related datasets.
WITH legacy AS (
    SELECT m.id AS model_id, m.name AS model_name, r.v AS item, r.i AS pos,
           gen_random_uuid() AS dataset_id
      FROM "_fd_models" m,
           jsonb_array_elements(m.related) WITH ORDINALITY AS r(v, i)
     WHERE jsonb_typeof(m.related) = 'array' AND r.v->'dataset' ? 'table'
), made AS (
    INSERT INTO "_fd_datasets" (id, name, description, base, operations, attributes)
    SELECT dataset_id, model_name || ' — ' || (item->>'name'), '',
           jsonb_build_object('kind', 'table', 'table', item->'dataset'->>'table'),
           pg_temp.fd_dataset_operations(item->'dataset'), '{}'::jsonb
      FROM legacy
    RETURNING id
)
UPDATE "_fd_models" m
   SET related = (
       SELECT jsonb_agg(
                  CASE WHEN l.dataset_id IS NULL THEN e.v
                       ELSE jsonb_build_object('name', e.v->>'name', 'dataset_id', l.dataset_id,
                                               'label', e.v->'label') END
                  ORDER BY e.i)
         FROM jsonb_array_elements(m.related) WITH ORDINALITY AS e(v, i)
         LEFT JOIN legacy l ON l.model_id = m.id AND l.pos = e.i)
 WHERE m.id IN (SELECT model_id FROM legacy);

COMMIT;

-- Check: this should return 0.
--
--   SELECT count(*) FROM "_fd_models"
--    WHERE dataset ? 'table'
--       OR (jsonb_typeof(related) = 'array'
--           AND EXISTS (SELECT 1 FROM jsonb_array_elements(related) r
--                        WHERE r->'dataset' ? 'table'));

-- ---------------------------------------------------------------------------
-- 8. SQLite primary: the same upgrade as section 7.
-- ---------------------------------------------------------------------------
--
-- SQLite stores JSON and UUIDs as text and has no statement that inserts and
-- updates at once, so the new ids go through a work table first. Run after
-- section 3, with the server stopped, as one transaction. Re-running it is a
-- no-op.
--
--   CREATE TABLE IF NOT EXISTS "_fd_datasets" (
--       "id" uuid NOT NULL PRIMARY KEY,
--       "name" text NOT NULL UNIQUE,
--       "description" text,
--       "base" jsonb NOT NULL,
--       "operations" jsonb NOT NULL,
--       "attributes" jsonb NOT NULL
--   );
--
--   CREATE TABLE "_fd_legacy_datasets" AS
--   SELECT m.id AS model_id, NULL AS pos, m.name || ' — data' AS name,
--          json(m.dataset) AS ds,
--          lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' ||
--                substr(hex(randomblob(2)), 2) || '-8' || substr(hex(randomblob(2)), 2) ||
--                '-' || hex(randomblob(6))) AS dataset_id
--     FROM "_fd_models" m
--    WHERE json_type(m.dataset, '$.table') IS NOT NULL
--   UNION ALL
--   SELECT m.id, r.key, m.name || ' — ' || json_extract(r.value, '$.name'),
--          json_extract(r.value, '$.dataset'),
--          lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' ||
--                substr(hex(randomblob(2)), 2) || '-8' || substr(hex(randomblob(2)), 2) ||
--                '-' || hex(randomblob(6)))
--     FROM "_fd_models" m, json_each(m.related) r
--    WHERE json_type(m.related) = 'array'
--      AND json_type(r.value, '$.dataset.table') IS NOT NULL;
--
--   INSERT INTO "_fd_datasets" (id, name, description, base, operations, attributes)
--   SELECT l.dataset_id, l.name, '',
--          json_object('kind', 'table', 'table', json_extract(l.ds, '$.table')),
--          (SELECT json_group_array(json(op) ORDER BY ord) FROM (
--               SELECT c.key AS ord,
--                      json_object('id', 'c' || (c.key + 1), 'enabled', json('true'),
--                          'kind', 'calculated',
--                          'params', json_object('name', json_extract(c.value, '$.name'),
--                                                'formula', json_extract(c.value, '$.expr'))) AS op
--                 FROM json_each(l.ds, '$.columns') c
--               UNION ALL
--               SELECT 1000000,
--                      json_object('id', 'filter', 'enabled', json('true'), 'kind', 'filter',
--                          'params', json_object('formula', json_extract(l.ds, '$.filter')))
--                WHERE COALESCE(json_extract(l.ds, '$.filter'), '') <> ''
--               UNION ALL
--               SELECT 1000001,
--                      json_object('id', 'sort', 'enabled', json('true'), 'kind', 'sort',
--                          'params', json_object('keys', (
--                              SELECT json_group_array(json_object(
--                                         'formula', json_extract(o.value, '$.expr'),
--                                         'descending',
--                                         json(CASE WHEN json_extract(o.value, '$.descending')
--                                                   THEN 'true' ELSE 'false' END))
--                                     ORDER BY o.key)
--                                FROM json_each(l.ds, '$.order') o)))
--                WHERE json_array_length(l.ds, '$.order') > 0
--               UNION ALL
--               SELECT 1000002,
--                      json_object('id', 'select', 'enabled', json('true'), 'kind', 'select',
--                          'params', json_object('columns', (
--                              SELECT json_group_array(
--                                         json_object('column', json_extract(c.value, '$.name'))
--                                     ORDER BY c.key)
--                                FROM json_each(l.ds, '$.columns') c)))
--                WHERE json_array_length(l.ds, '$.columns') > 0)),
--          '{}'
--     FROM "_fd_legacy_datasets" l;
--
--   UPDATE "_fd_models"
--      SET dataset = json_object('dataset_id',
--                        (SELECT l.dataset_id FROM "_fd_legacy_datasets" l
--                          WHERE l.model_id = "_fd_models".id AND l.pos IS NULL))
--    WHERE id IN (SELECT model_id FROM "_fd_legacy_datasets" WHERE pos IS NULL);
--
--   UPDATE "_fd_models"
--      SET related = (
--          SELECT json_group_array(json(
--                   CASE WHEN l.dataset_id IS NULL THEN r.value
--                        ELSE json_object('name', json_extract(r.value, '$.name'),
--                                         'dataset_id', l.dataset_id,
--                                         'label', json_extract(r.value, '$.label')) END)
--                   ORDER BY r.key)
--            FROM json_each("_fd_models".related) r
--            LEFT JOIN "_fd_legacy_datasets" l
--              ON l.model_id = "_fd_models".id AND l.pos = r.key)
--    WHERE id IN (SELECT model_id FROM "_fd_legacy_datasets" WHERE pos IS NOT NULL);
--
--   DROP TABLE "_fd_legacy_datasets";

-- ---------------------------------------------------------------------------
-- 9. Postgres and SQLite: the Dataset editor is no longer a workspace
--    (2026-10-01).
-- ---------------------------------------------------------------------------
--
-- The Analytics UI's front page lists the datasets beside the workspaces, and
-- a dataset opens in the Dataset editor on its own; the `dataset_editor`
-- workspace kind is gone. A stored workspace of that kind no longer reads, and
-- would make the whole list fail, so delete them. The datasets they edited are
-- in `_fd_datasets` and are untouched. Re-running it is a no-op; skip it where
-- `_fd_workspaces` does not exist.

DELETE FROM "_fd_workspaces" WHERE kind = 'dataset_editor';
