-- Demo schema + data for manual testing of hubro against SQLite.
--
-- Applied by seed-sqlite.sh into a throwaway file; re-running recreates the
-- database from scratch.
--
-- Deliberately covers SQLite's own oddities alongside the ordinary stuff: an
-- INTEGER PRIMARY KEY (which *is* the rowid), a keyless table addressed by
-- rowid, a WITHOUT ROWID table, VIRTUAL and STORED generated columns, a table
-- with no declared column types, a partial unique index, quoted identifiers
-- with spaces, and a table big enough to page through.

PRAGMA foreign_keys = ON;

DROP VIEW IF EXISTS order_summary;
DROP TABLE IF EXISTS order_items;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS events;
DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS customers;
DROP TABLE IF EXISTS sensor_readings;
DROP TABLE IF EXISTS all_types;
DROP TABLE IF EXISTS untyped;

-- ---------------------------------------------------------------- customers
-- INTEGER PRIMARY KEY: an alias for the rowid, so row identity resolves to the
-- PK rather than the implicit rowid.
CREATE TABLE customers (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL,
    email     TEXT NOT NULL UNIQUE,
    signed_up TEXT NOT NULL DEFAULT (date('now')),
    is_active INTEGER NOT NULL DEFAULT 1,
    balance   NUMERIC NOT NULL DEFAULT 0,
    prefs     TEXT,        -- JSON kept as text, the usual SQLite idiom
    notes     TEXT,
    avatar    BLOB
);

INSERT INTO customers (id, name, email, signed_up, is_active, balance, prefs, notes, avatar) VALUES
    (1,  'Ada Lovelace',     'ada@example.com',   '2021-03-14', 1,  1250.00, '{"theme":"dark","locale":"en"}',  'First customer.',                      x'89504e470d0a1a0a'),
    (2,  'Ingrid Hågensen',  'ingrid@example.no', '2022-01-02', 1,     0.00, '{"theme":"light"}',               NULL,                                   NULL),
    (3,  '张伟',             'zhang@example.cn',  '2022-06-30', 1,   -42.50, NULL,                              'Bills in CNY. 中文备注。',              NULL),
    (4,  'Björn Öst',        'bjorn@example.se',  '2023-02-11', 0,   310.75, '{"theme":"dark","digest":false}', 'Churned after the 2023 price change.', NULL),
    (5,  'Yusuf Al-Amin',    'yusuf@example.ae',  '2023-04-19', 1,    88.20, '{"locale":"ar"}',                 NULL,                                   x'ffd8ffe000104a46'),
    (6,  'Emoji Enjoyer 🦉', 'owl@example.com',   '2023-08-08', 1,     5.00, '{"emoji":"🦉🎉"}',                'Name and prefs both contain emoji 🎉',  NULL),
    (7,  'Mara Silva',       'mara@example.br',   '2024-01-05', 1,  2400.00, '{"theme":"system"}',              NULL,                                   NULL),
    (8,  'Tom O''Brien',     'tom@example.ie',    '2024-03-22', 1,    19.99, NULL,                              'Apostrophe in the name, on purpose.',  NULL),
    (9,  'Nadia Petrova',    'nadia@example.ru',  '2024-07-17', 0,     0.00, '{}',                              NULL,                                   NULL),
    (10, 'Kenji Watanabe',   'kenji@example.jp',  '2025-02-09', 1,   640.10, '{"locale":"ja"}',                 replace(hex(zeroblob(700)), '00', 'long note. '), NULL),
    (11, 'Sofia Rossi',      'sofia@example.it',  '2025-05-30', 1,   120.00, '{"digest":true}',                 NULL,                                   NULL),
    (12, 'No Contact',       'void@example.com',  '2025-11-11', 0,     0.00, NULL,                              NULL,                                   NULL);

-- ----------------------------------------------------------------- products
-- WITHOUT ROWID with a text primary key.
CREATE TABLE products (
    sku             TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    price           REAL NOT NULL CHECK (price >= 0),
    weight_kg       REAL,
    in_stock        INTEGER NOT NULL DEFAULT 0,
    attributes      TEXT NOT NULL DEFAULT '{}',
    discontinued_at TEXT
) WITHOUT ROWID;

INSERT INTO products (sku, name, price, weight_kg, in_stock, attributes, discontinued_at) VALUES
    ('KB-60',    'Keyboard, 60%',        89.00, 0.55, 120, '{"switches":"tactile","layout":"ansi"}', NULL),
    ('KB-TKL',   'Keyboard, TKL',       119.00, 0.82,  45, '{"switches":"linear","layout":"iso"}',   NULL),
    ('MS-ERG',   'Mouse, ergonomic',     69.50, 0.11,   0, '{"buttons":6}',                          NULL),
    ('MS-TRK',   'Trackball',            94.00, 0.24,  12, '{"buttons":5,"ball":"55mm"}',            NULL),
    ('MON-27',   'Monitor 27" 4K',      549.00, 6.30,   8, '{"panel":"IPS","hz":60}',                NULL),
    ('MON-34U',  'Monitor 34" ultra',   899.00, 9.10,   3, '{"panel":"OLED","hz":175}',              NULL),
    ('CAB-USBC', 'Cable, USB-C 2m',      19.90, 0.09, 500, '{"length_m":2}',                         NULL),
    ('DOK-TB4',  'Dock, Thunderbolt 4', 279.00, 0.65,  22, '{"ports":11}',                           NULL),
    ('HP-OLD',   'Headphones (EOL)',     59.00, 0.31,   0, '{"eol":true}',                           '2024-09-01T12:00:00Z'),
    ('MAT-DSK',  'Desk mat, felt',       34.00, 0.40,  77, '{"colour":"grå","size":"90x40"}',        NULL);

-- ------------------------------------------------------------------- orders
CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers (id) ON DELETE CASCADE,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled', 'refunded')),
    placed_at   TEXT NOT NULL DEFAULT (datetime('now')),
    ship_to     TEXT,
    coupon      TEXT
);

CREATE INDEX orders_customer_idx ON orders (customer_id);
-- Partial unique index: only guarantees uniqueness among pending orders, so
-- row identity must not treat it as a key.
CREATE UNIQUE INDEX orders_one_pending_per_customer
    ON orders (customer_id) WHERE status = 'pending';

INSERT INTO orders (id, customer_id, status, placed_at, ship_to, coupon)
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 40)
SELECT i,
       1 + (i % 12),
       CASE i % 4 WHEN 0 THEN 'paid' WHEN 1 THEN 'shipped' WHEN 2 THEN 'cancelled' ELSE 'refunded' END,
       datetime('now', '-' || i || ' days'),
       CASE WHEN i % 5 = 0 THEN NULL ELSE 'Storgata ' || i || ', Oslo' END,
       CASE WHEN i % 6 = 0 THEN 'SUMMER10' END
FROM seq;

INSERT INTO orders (id, customer_id, status, placed_at, ship_to) VALUES
    (41, 1, 'pending', datetime('now', '-2 hours'),    'Storgata 1, Oslo'),
    (42, 2, 'pending', datetime('now', '-20 minutes'), NULL),
    (43, 3, 'pending', datetime('now'),                'Bryggen 4, Bergen');

-- -------------------------------------------------------------- order_items
-- Composite primary key, two foreign keys, and both flavours of generated
-- column (STORED is browsable and read-only; VIRTUAL is computed per read).
CREATE TABLE order_items (
    order_id   INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    line_no    INTEGER NOT NULL,
    sku        TEXT NOT NULL REFERENCES products (sku),
    quantity   INTEGER NOT NULL DEFAULT 1 CHECK (quantity > 0),
    unit_price REAL NOT NULL,
    line_total REAL GENERATED ALWAYS AS (quantity * unit_price) STORED,
    discounted INTEGER GENERATED ALWAYS AS (quantity >= 3) VIRTUAL,
    PRIMARY KEY (order_id, line_no)
);

INSERT INTO order_items (order_id, line_no, sku, quantity, unit_price)
WITH RECURSIVE
    line(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM line WHERE n < 3),
    numbered AS (SELECT sku, price, row_number() OVER (ORDER BY sku) - 1 AS idx FROM products)
SELECT o.id, line.n, p.sku, 1 + ((o.id + line.n) % 4), p.price
FROM orders o
JOIN line
JOIN numbered p ON p.idx = (o.id * 3 + line.n) % (SELECT count(*) FROM products)
WHERE (o.id + line.n) % 3 <> 0;

-- ------------------------------------------------------------------- events
-- No primary key and no unique index, so rows are addressed by rowid.
CREATE TABLE events (
    occurred_at TEXT NOT NULL,
    kind        TEXT NOT NULL,
    customer_id INTEGER REFERENCES customers (id),
    payload     TEXT,
    duration_ms INTEGER
);

INSERT INTO events (occurred_at, kind, customer_id, payload, duration_ms)
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 5000)
SELECT datetime('now', '-' || i || ' minutes'),
       CASE i % 5 WHEN 0 THEN 'page_view' WHEN 1 THEN 'click' WHEN 2 THEN 'signup'
                  WHEN 3 THEN 'purchase' ELSE 'error' END,
       CASE WHEN i % 7 = 0 THEN NULL ELSE 1 + (i % 12) END,
       json_object('seq', i, 'ok', json(CASE WHEN i % 3 <> 0 THEN 'true' ELSE 'false' END),
                   'path', '/p/' || (i % 50)),
       CASE WHEN i % 11 = 0 THEN NULL ELSE (i * 7) % 900 END
FROM seq;

CREATE INDEX events_occurred_idx ON events (occurred_at);

-- ---------------------------------------------------------- sensor_readings
-- Keyless, but with a full unique index — row identity should use that index
-- rather than falling back to the rowid.
CREATE TABLE sensor_readings (
    site       TEXT NOT NULL,
    sensor     TEXT NOT NULL,
    reading_at TEXT NOT NULL,
    celsius    REAL,
    humidity   REAL
);

CREATE UNIQUE INDEX sensor_readings_key ON sensor_readings (site, sensor, reading_at);

INSERT INTO sensor_readings (site, sensor, reading_at, celsius, humidity)
WITH RECURSIVE
    hours(h) AS (SELECT 0 UNION ALL SELECT h + 1 FROM hours WHERE h < 199),
    probes(p) AS (SELECT 1 UNION ALL SELECT p + 1 FROM probes WHERE p < 3),
    sites(site) AS (VALUES ('oslo'), ('bergen'), ('tromsø'))
SELECT site, 'probe-' || p,
       datetime('2026-01-01 00:00', '+' || h || ' hours'),
       round(15 + 10 * sin(h / 3.0) + p, 2),
       CASE WHEN h % 9 = 0 THEN NULL ELSE round(40 + 20 * cos(h / 5.0), 2) END
FROM sites, probes, hours;

-- ---------------------------------------------------------------- all_types
-- SQLite stores whatever you give it regardless of the declared type, so this
-- mixes declared affinities with values that don't match them.
CREATE TABLE all_types (
    id            INTEGER PRIMARY KEY,
    label         TEXT,
    c_integer     INTEGER,
    c_bigint      BIGINT,
    c_real        REAL,
    c_numeric     NUMERIC,
    c_bool        BOOLEAN,
    c_text        TEXT,
    c_varchar     VARCHAR(32),
    c_blob        BLOB,
    c_date        DATE,
    c_datetime    DATETIME,
    c_json        JSON,
    "spaced name" TEXT,
    "select"      TEXT   -- a reserved word as a column name
);

INSERT INTO all_types (id, label, c_integer, c_bigint, c_real, c_numeric, c_bool, c_text,
                       c_varchar, c_blob, c_date, c_datetime, c_json, "spaced name", "select")
VALUES
    (1, 'typical', 42, 9007199254740993, 2.718281828459045, 1234.5678, 1, 'plain text',
     'varchar value', x'deadbeef', '2026-02-14', '2026-02-14 13:45:00.123',
     '{"a":[1,2,3]}', 'value in a spaced column', 'from'),
    (2, 'all null', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    (3, 'extremes', -9223372036854775808, 9223372036854775807, 9.0e307, -0.000001, 0,
     replace(hex(zeroblob(1000)), '00', 'long cell. '), 'ünïcödé — 中文 — 🦉',
     zeroblob(64000), 'not a date at all', '', '[]', '', ''),
    (4, 'wrong affinities', 'text in an INTEGER column', 3.5, 'text in a REAL column',
     x'0102', 'maybe', 12345, 6.7, 'text in a BLOB column', 8, 9.9, 'not json', NULL, NULL);

-- ------------------------------------------------------------------ untyped
-- No declared types at all: every column has BLOB (none) affinity, and the
-- viewer has nothing but the values to go on.
CREATE TABLE untyped (a, b, c);

INSERT INTO untyped VALUES
    (1, 'one', x'01'),
    (2.5, NULL, NULL),
    (NULL, 'three', x'0203'),
    ('4', '', zeroblob(8));

-- --------------------------------------------------------------------- view
CREATE VIEW order_summary AS
SELECT o.id AS order_id,
       c.name AS customer,
       o.status,
       o.placed_at,
       count(i.line_no) AS lines,
       coalesce(sum(i.line_total), 0) AS total
FROM orders o
JOIN customers c ON c.id = o.customer_id
LEFT JOIN order_items i ON i.order_id = o.id
GROUP BY o.id, c.name, o.status, o.placed_at;

ANALYZE;
