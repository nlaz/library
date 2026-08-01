-- Initial metadata schema.
--
-- What used to be data/titles.json, data/collections.json,
-- data/notes/cards.json, data/status/<doc>.json and the app's
-- settings.json. Legacy data/annotations/<doc>.json is deliberately absent:
-- those sidecars are read-only migration input (see annots.rs), never a
-- live table.
--
-- Doc ids are the filename-stem ids in use today; the roots/files identity
-- work re-keys them to minted ids in a later migration.

CREATE TABLE docs (
    id          TEXT PRIMARY KEY,
    -- user-set display title; NULL means "derive one from the id"
    title       TEXT,
    -- ingest state machine, absorbed from data/status/<doc>.json
    state       TEXT,
    stage       TEXT,
    done        INTEGER NOT NULL DEFAULT 0,
    total       INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    -- per-stage timings and legibility, opaque to SQL
    metrics     TEXT,
    updated_at  INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX docs_state ON docs (state);

-- A doc's membership in a shelf. Retired once folders become shelves; until
-- then this is data/collections.json with one row per membership instead of
-- one file per library.
CREATE TABLE collections (
    name        TEXT NOT NULL,
    doc         TEXT NOT NULL,
    ord         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (name, doc)
) STRICT;

CREATE INDEX collections_doc ON collections (doc);

-- Note cards. `evidence` and `links` stay JSON: both are serde-tagged
-- enums whose shape is owned by notes.rs, and normalizing them into columns
-- would freeze that shape in the schema.
CREATE TABLE cards (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    body         TEXT NOT NULL DEFAULT '',
    created      INTEGER NOT NULL,
    modified     INTEGER NOT NULL,
    filed        INTEGER NOT NULL DEFAULT 0,
    split_hinted INTEGER NOT NULL DEFAULT 0,
    evidence     TEXT NOT NULL DEFAULT '[]',
    links        TEXT NOT NULL DEFAULT '[]',
    -- explicit insertion order: `created` is one-second resolution, and the
    -- annotation migration mints many cards inside one second in an order
    -- that is meaningful (reading order within a document)
    ord          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
) STRICT;
