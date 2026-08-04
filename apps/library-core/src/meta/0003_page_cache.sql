-- When a document was last read, so page renders can be evicted least-read
-- first.
--
-- data/pages is becoming a bounded cache rather than permanent storage, and
-- an LRU needs a recency signal. The two signals already lying around were
-- both rejected:
--
--   the page JPEG's mtime — free to read, but writing it on every serve
--   costs ~25 utimensat per search burst, and it destroys the only record
--   of *when a page was rendered*, which is what diagnoses a width change.
--
--   atime — literally free, already updated by the read. Rejected because
--   any backup, Spotlight reindex, antivirus or `du` flattens the whole
--   signal to "everything was read at once", and the failure is invisible:
--   the cache quietly evicts the wrong things and nobody can tell.
--
-- A column costs one throttled UPDATE per document per minute, is orderable
-- with one indexed query instead of stat-ing ten thousand files, survives a
-- restart as a queryable fact, and is multi-process — so library-server can
-- participate in the same LRU the app maintains.
--
-- Recency is per *document*, not per page: a document is what a person
-- reads, and per-page tracking would be ~40x the writes to answer a
-- question nobody asks. Eviction picks victims per document too; only the
-- re-render is per page, so an evicted book comes back incrementally
-- instead of paying 900 renders to open page 3.
ALTER TABLE docs ADD COLUMN last_read_at INTEGER NOT NULL DEFAULT 0;

-- 0 sorts first, so a document nobody has opened is the first to go.
CREATE INDEX docs_last_read ON docs (last_read_at);
