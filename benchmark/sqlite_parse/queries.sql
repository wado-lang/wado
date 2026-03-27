-- Realistic SQLite queries for parser benchmarking.
-- Mix of DDL, DML, expressions, subqueries, CTEs, joins, set operations.

CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT,
    is_active INTEGER DEFAULT 1,
    profile_json TEXT
);

CREATE TABLE posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    body TEXT,
    slug TEXT UNIQUE,
    status TEXT DEFAULT 'draft' CHECK(status IN ('draft', 'published', 'archived')),
    view_count INTEGER DEFAULT 0,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    published_at TEXT
);

CREATE TABLE comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    parent_id INTEGER REFERENCES comments(id),
    body TEXT NOT NULL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE post_tags (
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (post_id, tag_id)
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    ip_address TEXT NOT NULL,
    user_agent TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NOT NULL
);

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER REFERENCES users(id),
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id INTEGER,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_posts_user_id ON posts(user_id);
CREATE INDEX idx_posts_status ON posts(status) WHERE status = 'published';
CREATE INDEX idx_comments_post_id ON comments(post_id);
CREATE INDEX idx_posts_created ON posts(created_at DESC);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);
CREATE INDEX idx_audit_entity ON audit_log(entity_type, entity_id);

INSERT INTO users (username, email, password_hash) VALUES ('alice', 'alice@example.com', 'hash1');
INSERT INTO users (username, email, password_hash) VALUES ('bob', 'bob@example.com', 'hash2');
INSERT INTO users (username, email, password_hash) VALUES ('charlie', 'charlie@example.com', 'hash3');
INSERT INTO users (username, email, password_hash) VALUES ('diana', 'diana@example.com', 'hash4');
INSERT INTO users (username, email, password_hash) VALUES ('eve', 'eve@example.com', 'hash5');

INSERT INTO posts (user_id, title, body, slug, status, published_at)
VALUES (1, 'Hello World', 'This is my first post.', 'hello-world', 'published', '2024-01-15 10:00:00');

INSERT INTO posts (user_id, title, body, slug, status, published_at)
VALUES (1, 'SQLite Tips', 'SQLite is amazing for embedded databases.', 'sqlite-tips', 'published', '2024-02-01 12:00:00');

INSERT INTO posts (user_id, title, body, slug, status)
VALUES (2, 'Draft Post', 'Work in progress...', 'draft-post', 'draft');

INSERT INTO posts (user_id, title, body, slug, status, published_at)
VALUES (3, 'Database Design', 'Good schema design matters.', 'db-design', 'published', '2024-03-10 08:30:00');

INSERT INTO posts (user_id, title, body, slug, status)
VALUES (4, 'Archived Article', 'Old content.', 'archived-article', 'archived');

INSERT INTO tags (name) VALUES ('sqlite');
INSERT INTO tags (name) VALUES ('database');
INSERT INTO tags (name) VALUES ('tutorial');
INSERT INTO tags (name) VALUES ('programming');
INSERT INTO tags (name) VALUES ('performance');

INSERT INTO post_tags (post_id, tag_id) VALUES (1, 1);
INSERT INTO post_tags (post_id, tag_id) VALUES (1, 4);
INSERT INTO post_tags (post_id, tag_id) VALUES (2, 1);
INSERT INTO post_tags (post_id, tag_id) VALUES (2, 2);
INSERT INTO post_tags (post_id, tag_id) VALUES (2, 3);
INSERT INTO post_tags (post_id, tag_id) VALUES (4, 2);
INSERT INTO post_tags (post_id, tag_id) VALUES (4, 5);

INSERT INTO comments (post_id, user_id, body)
VALUES (1, 2, 'Great first post!');

INSERT INTO comments (post_id, user_id, parent_id, body)
VALUES (1, 1, 1, 'Thanks Bob!');

INSERT INTO comments (post_id, user_id, body)
VALUES (2, 3, 'Very helpful tips.');

INSERT INTO comments (post_id, user_id, body)
VALUES (2, 4, 'I learned a lot from this.');

INSERT INTO comments (post_id, user_id, body)
VALUES (4, 5, 'Nice overview of schema design.');

INSERT INTO sessions (id, user_id, ip_address, user_agent, expires_at)
VALUES ('sess_001', 1, '192.168.1.100', 'Mozilla/5.0', '2024-12-31 23:59:59');

INSERT INTO sessions (id, user_id, ip_address, user_agent, expires_at)
VALUES ('sess_002', 2, '10.0.0.50', 'Chrome/120', '2024-12-31 23:59:59');

INSERT INTO audit_log (user_id, action, entity_type, entity_id, new_value)
VALUES (1, 'create', 'post', 1, 'Hello World');

INSERT INTO audit_log (user_id, action, entity_type, entity_id, old_value, new_value)
VALUES (1, 'update', 'post', 1, 'draft', 'published');

SELECT * FROM users WHERE is_active = 1 ORDER BY created_at DESC;

SELECT p.id, p.title, p.slug, u.username AS author, p.view_count, p.published_at
FROM posts p
INNER JOIN users u ON p.user_id = u.id
WHERE p.status = 'published'
ORDER BY p.published_at DESC
LIMIT 10;

SELECT p.title, COUNT(c.id) AS comment_count
FROM posts p
LEFT JOIN comments c ON p.id = c.post_id
WHERE p.status = 'published'
GROUP BY p.id
ORDER BY comment_count DESC;

SELECT p.title, GROUP_CONCAT(t.name) AS tags
FROM posts p
INNER JOIN post_tags pt ON p.id = pt.post_id
INNER JOIN tags t ON pt.tag_id = t.id
GROUP BY p.id;

SELECT u.username,
       COUNT(DISTINCT p.id) AS post_count,
       COUNT(DISTINCT c.id) AS comment_count,
       COALESCE(SUM(p.view_count), 0) AS total_views
FROM users u
LEFT JOIN posts p ON u.id = p.user_id AND p.status = 'published'
LEFT JOIN comments c ON u.id = c.user_id
GROUP BY u.id
ORDER BY total_views DESC;

SELECT p.title, c.body AS comment, u.username AS commenter, c.created_at
FROM comments c
INNER JOIN posts p ON c.post_id = p.id
INNER JOIN users u ON c.user_id = u.id
WHERE c.parent_id IS NULL
ORDER BY c.created_at DESC
LIMIT 20 OFFSET 0;

WITH RECURSIVE comment_tree(id, post_id, user_id, parent_id, body, depth) AS (
    SELECT id, post_id, user_id, parent_id, body, 0
    FROM comments
    WHERE parent_id IS NULL AND post_id = 1
    UNION ALL
    SELECT c.id, c.post_id, c.user_id, c.parent_id, c.body, ct.depth + 1
    FROM comments c
    INNER JOIN comment_tree ct ON c.parent_id = ct.id
)
SELECT ct.depth, ct.body, u.username
FROM comment_tree ct
INNER JOIN users u ON ct.user_id = u.id
ORDER BY ct.id;

SELECT *
FROM posts
WHERE user_id IN (SELECT id FROM users WHERE is_active = 1)
  AND id NOT IN (SELECT post_id FROM post_tags WHERE tag_id = 3)
  AND status = 'published';

SELECT p.title,
       (SELECT COUNT(*) FROM comments c WHERE c.post_id = p.id) AS comment_count,
       (SELECT COUNT(*) FROM post_tags pt WHERE pt.post_id = p.id) AS tag_count
FROM posts p
WHERE p.status = 'published'
ORDER BY comment_count DESC;

SELECT title,
       CASE
           WHEN view_count > 1000 THEN 'viral'
           WHEN view_count > 100 THEN 'popular'
           WHEN view_count > 10 THEN 'moderate'
           ELSE 'low'
       END AS popularity,
       CASE status
           WHEN 'published' THEN 1
           WHEN 'draft' THEN 0
           ELSE -1
       END AS status_code
FROM posts;

UPDATE posts SET view_count = view_count + 1 WHERE id = 1;

UPDATE posts
SET status = 'archived'
WHERE status = 'published'
  AND published_at < datetime('now', '-1 year');

DELETE FROM comments WHERE user_id NOT IN (SELECT id FROM users WHERE is_active = 1);

DELETE FROM sessions WHERE expires_at < datetime('now');

SELECT 'users' AS table_name, COUNT(*) AS row_count FROM users
UNION ALL
SELECT 'posts', COUNT(*) FROM posts
UNION ALL
SELECT 'comments', COUNT(*) FROM comments
UNION ALL
SELECT 'tags', COUNT(*) FROM tags;

SELECT u.username, p.title
FROM users u
CROSS JOIN posts p
WHERE p.status = 'published'
INTERSECT
SELECT u.username, p.title
FROM users u
INNER JOIN comments c ON u.id = c.user_id
INNER JOIN posts p ON c.post_id = p.id;

WITH monthly_stats AS (
    SELECT strftime('%Y-%m', published_at) AS month,
           COUNT(*) AS post_count,
           SUM(view_count) AS total_views
    FROM posts
    WHERE status = 'published'
    GROUP BY strftime('%Y-%m', published_at)
)
SELECT ms.month, ms.post_count, ms.total_views
FROM monthly_stats ms
ORDER BY ms.month;

WITH user_activity AS (
    SELECT u.id, u.username,
           COUNT(DISTINCT p.id) AS posts,
           COUNT(DISTINCT c.id) AS comments
    FROM users u
    LEFT JOIN posts p ON u.id = p.user_id
    LEFT JOIN comments c ON u.id = c.user_id
    GROUP BY u.id
)
SELECT ua.username, ua.posts, ua.comments
FROM user_activity ua
ORDER BY ua.username;

CREATE VIEW published_posts_view AS
SELECT p.id, p.title, p.slug, p.body, p.view_count,
       u.username AS author,
       p.published_at,
       (SELECT COUNT(*) FROM comments c WHERE c.post_id = p.id) AS comment_count
FROM posts p
INNER JOIN users u ON p.user_id = u.id
WHERE p.status = 'published';

INSERT OR REPLACE INTO users (id, username, email, password_hash)
VALUES (1, 'alice', 'newalice@example.com', 'newhash1');

SELECT p.title,
       SUBSTR(p.body, 1, 100) || '...' AS excerpt,
       LENGTH(p.body) AS body_length,
       REPLACE(LOWER(p.title), ' ', '-') AS generated_slug,
       UPPER(u.username) AS author_upper,
       TRIM(p.body) AS trimmed_body,
       INSTR(p.body, 'SQLite') AS sqlite_pos,
       ABS(p.view_count - 100) AS distance_from_100,
       TYPEOF(p.view_count) AS view_type,
       HEX(RANDOMBLOB(8)) AS request_id
FROM posts p
INNER JOIN users u ON p.user_id = u.id
WHERE p.body IS NOT NULL
  AND LENGTH(p.body) > 0
  AND p.title LIKE '%SQL%' ESCAPE '\'
  AND p.published_at BETWEEN '2024-01-01' AND '2024-12-31'
  AND p.view_count >= 0
  AND (p.status = 'published' OR p.status = 'archived')
ORDER BY p.published_at DESC
LIMIT 50;

SELECT p.id, p.title, p.status, p.view_count,
       u.username, u.email,
       (SELECT COUNT(*) FROM comments WHERE post_id = p.id) AS num_comments,
       EXISTS(SELECT 1 FROM post_tags WHERE post_id = p.id) AS has_tags
FROM posts p
INNER JOIN users u ON p.user_id = u.id
WHERE p.status != 'archived'
ORDER BY p.created_at DESC
LIMIT 25;

SELECT DISTINCT u.username
FROM users u
WHERE EXISTS (
    SELECT 1 FROM posts p
    WHERE p.user_id = u.id AND p.status = 'published'
)
AND NOT EXISTS (
    SELECT 1 FROM comments c
    WHERE c.user_id = u.id
)
ORDER BY u.username;

SELECT p.title,
       NULLIF(p.view_count, 0) AS non_zero_views,
       IFNULL(p.published_at, 'unpublished') AS pub_date,
       COALESCE(p.body, p.title, 'no content') AS content
FROM posts p
ORDER BY p.id;

SELECT u.username, s.ip_address, s.created_at AS session_start
FROM users u
INNER JOIN sessions s ON u.id = s.user_id
WHERE s.expires_at > datetime('now')
ORDER BY s.created_at DESC;

SELECT a.action, a.entity_type, a.entity_id, u.username, a.created_at
FROM audit_log a
LEFT JOIN users u ON a.user_id = u.id
WHERE a.entity_type = 'post'
ORDER BY a.created_at DESC
LIMIT 100;

SELECT u.username, COUNT(a.id) AS action_count
FROM users u
LEFT JOIN audit_log a ON u.id = a.user_id
GROUP BY u.id
ORDER BY action_count DESC;

SELECT p.title, p.view_count,
       (SELECT AVG(view_count) FROM posts WHERE status = 'published') AS avg_views,
       p.view_count - (SELECT AVG(view_count) FROM posts WHERE status = 'published') AS diff_from_avg
FROM posts p
WHERE p.status = 'published'
ORDER BY diff_from_avg DESC;

SELECT status, COUNT(*) AS cnt, SUM(view_count) AS total_views, AVG(view_count) AS avg_views
FROM posts
GROUP BY status
ORDER BY cnt DESC;

SELECT t.name AS tag_name, COUNT(pt.post_id) AS usage_count
FROM tags t
LEFT JOIN post_tags pt ON t.id = pt.tag_id
GROUP BY t.id
ORDER BY usage_count DESC;

SELECT p.title
FROM posts p
WHERE NOT EXISTS (
    SELECT 1 FROM comments c WHERE c.post_id = p.id
)
AND p.status = 'published';

SELECT u.username,
       (SELECT COUNT(*) FROM posts WHERE user_id = u.id) AS total_posts,
       (SELECT COUNT(*) FROM posts WHERE user_id = u.id AND status = 'published') AS published_posts,
       (SELECT COUNT(*) FROM posts WHERE user_id = u.id AND status = 'draft') AS draft_posts,
       (SELECT COUNT(*) FROM comments WHERE user_id = u.id) AS total_comments
FROM users u
WHERE u.is_active = 1
ORDER BY u.username;

REPLACE INTO tags (id, name) VALUES (1, 'sqlite3');

SELECT p.title, c.body, u.username
FROM posts p
LEFT JOIN comments c ON p.id = c.post_id
LEFT JOIN users u ON c.user_id = u.id
WHERE p.id IN (SELECT post_id FROM post_tags WHERE tag_id IN (SELECT id FROM tags WHERE name = 'sqlite'))
ORDER BY p.title, c.created_at;

SELECT CAST(COUNT(*) AS REAL) / (SELECT COUNT(*) FROM users) AS posts_per_user
FROM posts;

SELECT p.title, p.view_count,
       CASE WHEN p.view_count > 0 THEN 'viewed' ELSE 'unviewed' END AS view_status,
       p.created_at,
       JULIANDAY('now') - JULIANDAY(p.created_at) AS days_old
FROM posts p
ORDER BY days_old;
