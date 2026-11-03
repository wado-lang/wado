CREATE TABLE users (
    id INTEGER PRIMARY KEY ASC,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT,
    is_active INTEGER DEFAULT 1,
    profile_json TEXT
);

CREATE INDEX idx_posts_status ON posts(status) WHERE status = 'published';

INSERT INTO users (username, email, password_hash) VALUES ('charlie', 'charlie@example.com', 'hash3');

INSERT INTO tags (name) VALUES ('sqlite');

INSERT INTO post_tags (post_id, tag_id) VALUES (2, 2);

INSERT INTO comments (post_id, user_id, body)
VALUES (4, 5, 'Nice overview of schema design.');

SELECT p.title, GROUP_CONCAT(t.name) AS tags
FROM posts p
INNER JOIN post_tags pt ON p.id = pt.post_id
INNER JOIN tags t ON pt.tag_id = t.id
GROUP BY p.id;

UPDATE posts
SET status = 'archived'
WHERE status = 'published'
  AND published_at < datetime('now', '-1 year');

REPLACE INTO users (id, username, email, password_hash, created_at)
VALUES (1, 'alice', 'newalice@example.com', 'newhash1', '2024-01-01');

SELECT p.title, p.view_count,
       (SELECT AVG(view_count) FROM posts WHERE status = 'published') AS avg_views,
       p.view_count - (SELECT AVG(view_count) FROM posts WHERE status = 'published') AS diff_from_avg
FROM posts p
WHERE p.status = 'published'
ORDER BY diff_from_avg DESC;

SELECT p.title, p.view_count,
       CASE WHEN p.view_count > 0 THEN 'viewed' ELSE 'unviewed' END AS view_status,
       p.created_at,
       JULIANDAY('now') - JULIANDAY(p.created_at) AS days_old
FROM posts p
ORDER BY days_old;
