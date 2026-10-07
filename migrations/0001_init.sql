CREATE TABLE pages (
    id           INTEGER PRIMARY KEY,
    slug         TEXT    NOT NULL UNIQUE,
    title        TEXT    NOT NULL,
    body         TEXT    NOT NULL,
    status       TEXT    NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published')),
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL,
    published_at TEXT
);

CREATE TABLE page_revisions (
    id         INTEGER PRIMARY KEY,
    page_id    INTEGER NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    revision   INTEGER NOT NULL,
    title      TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    created_at TEXT    NOT NULL,
    UNIQUE (page_id, revision)
);

CREATE TABLE media (
    id            INTEGER PRIMARY KEY,
    filename      TEXT    NOT NULL UNIQUE,
    original_name TEXT    NOT NULL,
    content_type  TEXT    NOT NULL,
    size          INTEGER NOT NULL,
    created_at    TEXT    NOT NULL
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
