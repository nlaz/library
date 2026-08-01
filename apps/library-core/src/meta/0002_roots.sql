-- Roots and the file identity index.
--
-- A root is a folder the user pointed us at: the default `~/The Library` or
-- one they linked in place. Files are never moved between them and never
-- reorganized — we reflect what is there.
--
-- `files` is the identity index. A document's id is minted once and never
-- derived from its path, so renaming a file in Finder keeps the document,
-- its page renders, and its notes. What identifies a file across a rename
-- is the inode; what identifies it across a copy is the content hash.

CREATE TABLE roots (
    id            TEXT PRIMARY KEY,
    path          TEXT NOT NULL UNIQUE,
    -- exactly one root is the drop target for files added through the app
    is_default    INTEGER NOT NULL DEFAULT 0,
    -- watching | unavailable (unmounted volume, unreadable, gone)
    state         TEXT NOT NULL DEFAULT 'watching',
    added_at      INTEGER NOT NULL DEFAULT 0,
    last_scan_at  INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE files (
    id            TEXT PRIMARY KEY,
    root_id       TEXT NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    -- path relative to the root, '/'-separated; also the shelf's source
    relpath       TEXT NOT NULL,
    -- (inode, size, mtime) is the cheap change probe: matching all three
    -- means we can skip hashing the file entirely
    inode         INTEGER NOT NULL DEFAULT 0,
    size          INTEGER NOT NULL DEFAULT 0,
    mtime         INTEGER NOT NULL DEFAULT 0,
    -- xxh3-128 of the contents; a dedup and change signal, never the key
    content_hash  TEXT,
    doc_id        TEXT NOT NULL,
    -- present | missing | dataless (iCloud/Dropbox stub: there, not readable)
    state         TEXT NOT NULL DEFAULT 'present',
    first_seen_at INTEGER NOT NULL DEFAULT 0,
    last_seen_at  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (root_id, relpath)
) STRICT;

CREATE INDEX files_doc ON files (doc_id);
CREATE INDEX files_inode ON files (root_id, inode);
CREATE INDEX files_hash ON files (content_hash);

-- Documents gain the facts that used to be implicit in the doc id: what
-- kind of file it is, and which shelf its folder puts it on.
ALTER TABLE docs ADD COLUMN kind TEXT;
ALTER TABLE docs ADD COLUMN shelf TEXT;
