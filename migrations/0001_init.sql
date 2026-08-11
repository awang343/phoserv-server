CREATE TABLE tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    parent_id INTEGER NOT NULL DEFAULT 0 REFERENCES tags(id) ON DELETE CASCADE,
    UNIQUE(parent_id, name)
);

-- sentinel root tag; all top-level tags have parent_id = 0
INSERT INTO tags (id, name, parent_id) VALUES (0, '', 0);

CREATE INDEX idx_tags_parent ON tags(parent_id);

CREATE TABLE photos (
    id TEXT PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    original_filename TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    media_type TEXT NOT NULL CHECK(media_type IN ('image', 'video')),
    ext TEXT NOT NULL,
    file_size INTEGER NOT NULL,
    width INTEGER,
    height INTEGER,
    duration_seconds REAL,
    taken_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_photos_created_at ON photos(created_at);

CREATE TABLE photo_tags (
    photo_id TEXT NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (photo_id, tag_id)
);

CREATE INDEX idx_photo_tags_tag ON photo_tags(tag_id);
