CREATE TABLE galleries (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE gallery_photos (
    gallery_id TEXT NOT NULL REFERENCES galleries(id) ON DELETE CASCADE,
    photo_id TEXT NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (gallery_id, photo_id)
);

CREATE UNIQUE INDEX idx_gallery_photos_position ON gallery_photos(gallery_id, position);
CREATE INDEX idx_gallery_photos_photo ON gallery_photos(photo_id);

-- Galleries get their own tag namespace/tree, entirely separate from photo
-- tags (see `tags`/`photo_tags`), so a gallery like "One Piece" can be tagged
-- "ongoing" without that tag polluting or being polluted by per-photo tags.
CREATE TABLE gallery_tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    parent_id INTEGER NOT NULL DEFAULT 0 REFERENCES gallery_tags(id) ON DELETE CASCADE,
    UNIQUE(parent_id, name)
);

-- sentinel root tag; all top-level gallery tags have parent_id = 0
INSERT INTO gallery_tags (id, name, parent_id) VALUES (0, '', 0);

CREATE INDEX idx_gallery_tags_parent ON gallery_tags(parent_id);

CREATE TABLE gallery_tag_links (
    gallery_id TEXT NOT NULL REFERENCES galleries(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES gallery_tags(id) ON DELETE CASCADE,
    PRIMARY KEY (gallery_id, tag_id)
);

CREATE INDEX idx_gallery_tag_links_tag ON gallery_tag_links(tag_id);
