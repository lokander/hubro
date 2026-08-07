-- Demo schema + data for manual testing of hubro against SQL Server.
--
-- Applied by seed-sqlserver.sh with sqlcmd (GO batch separators are required —
-- CREATE SCHEMA and CREATE VIEW must each start a batch). Re-running drops and
-- recreates the whole `demo` database, so it is safe to run repeatedly.
--
-- Deliberately covers what the viewer has special handling for: multiple
-- schemas, a view, IDENTITY and computed columns, composite and text primary
-- keys, a keyless table, foreign keys (single and composite), a filtered
-- unique index, NULLs, unicode, oversized cells, types with no dedicated
-- decoder (sql_variant, hierarchyid), and a table big enough to page through.

USE master;
GO

IF DB_ID('demo') IS NOT NULL
BEGIN
    ALTER DATABASE demo SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE demo;
END
GO

CREATE DATABASE demo;
GO

USE demo;
GO

CREATE SCHEMA analytics;
GO

-- ---------------------------------------------------------------- customers
CREATE TABLE dbo.customers (
    id        int IDENTITY(1, 1) PRIMARY KEY,
    name      nvarchar(100) NOT NULL,
    email     varchar(120) NOT NULL UNIQUE,
    signed_up date NOT NULL CONSTRAINT df_customers_signed_up DEFAULT (CAST(SYSDATETIME() AS date)),
    is_active bit NOT NULL CONSTRAINT df_customers_active DEFAULT (1),
    balance   decimal(12, 2) NOT NULL CONSTRAINT df_customers_balance DEFAULT (0),
    prefs     nvarchar(max),   -- JSON as text, the usual SQL Server idiom
    notes     nvarchar(max),
    avatar    varbinary(max)
);
GO

SET IDENTITY_INSERT dbo.customers ON;
INSERT INTO dbo.customers (id, name, email, signed_up, is_active, balance, prefs, notes, avatar) VALUES
    (1,  N'Ada Lovelace',     'ada@example.com',   '2021-03-14', 1,  1250.00, N'{"theme":"dark","locale":"en"}',  N'First customer.',                      0x89504E470D0A1A0A),
    (2,  N'Ingrid Hågensen',  'ingrid@example.no', '2022-01-02', 1,     0.00, N'{"theme":"light"}',               NULL,                                    NULL),
    (3,  N'张伟',             'zhang@example.cn',  '2022-06-30', 1,   -42.50, NULL,                               N'Bills in CNY. 中文备注。',              NULL),
    (4,  N'Björn Öst',        'bjorn@example.se',  '2023-02-11', 0,   310.75, N'{"theme":"dark","digest":false}', N'Churned after the 2023 price change.', NULL),
    (5,  N'Yusuf Al-Amin',    'yusuf@example.ae',  '2023-04-19', 1,    88.20, N'{"locale":"ar"}',                 NULL,                                    0xFFD8FFE000104A46),
    (6,  N'Emoji Enjoyer 🦉', 'owl@example.com',   '2023-08-08', 1,     5.00, N'{"emoji":"🦉🎉"}',                N'Name and prefs both contain emoji 🎉',  NULL),
    (7,  N'Mara Silva',       'mara@example.br',   '2024-01-05', 1,  2400.00, N'{"theme":"system"}',              NULL,                                    NULL),
    (8,  N'Tom O''Brien',     'tom@example.ie',    '2024-03-22', 1,    19.99, NULL,                               N'Apostrophe in the name, on purpose.',  NULL),
    (9,  N'Nadia Petrova',    'nadia@example.ru',  '2024-07-17', 0,     0.00, N'{}',                              NULL,                                    NULL),
    (10, N'Kenji Watanabe',   'kenji@example.jp',  '2025-02-09', 1,   640.10, N'{"locale":"ja"}',                 REPLICATE(CAST(N'A very long note that should be truncated in the grid and readable in the cell editor. ' AS nvarchar(max)), 40), NULL),
    (11, N'Sofia Rossi',      'sofia@example.it',  '2025-05-30', 1,   120.00, N'{"digest":true}',                 NULL,                                    NULL),
    (12, N'No Contact',       'void@example.com',  '2025-11-11', 0,     0.00, NULL,                               NULL,                                    NULL);
SET IDENTITY_INSERT dbo.customers OFF;
GO

-- ----------------------------------------------------------------- products
CREATE TABLE dbo.products (
    sku             varchar(16) PRIMARY KEY,
    name            nvarchar(100) NOT NULL,
    price           decimal(10, 2) NOT NULL CHECK (price >= 0),
    weight_kg       real,
    in_stock        int NOT NULL CONSTRAINT df_products_stock DEFAULT (0),
    attributes      nvarchar(max) NOT NULL CONSTRAINT df_products_attrs DEFAULT (N'{}'),
    discontinued_at datetimeoffset
);
GO

INSERT INTO dbo.products (sku, name, price, weight_kg, in_stock, attributes, discontinued_at) VALUES
    ('KB-60',    N'Keyboard, 60%',        89.00, 0.55, 120, N'{"switches":"tactile","layout":"ansi"}', NULL),
    ('KB-TKL',   N'Keyboard, TKL',       119.00, 0.82,  45, N'{"switches":"linear","layout":"iso"}',   NULL),
    ('MS-ERG',   N'Mouse, ergonomic',     69.50, 0.11,   0, N'{"buttons":6}',                          NULL),
    ('MS-TRK',   N'Trackball',            94.00, 0.24,  12, N'{"buttons":5,"ball":"55mm"}',            NULL),
    ('MON-27',   N'Monitor 27" 4K',      549.00, 6.30,   8, N'{"panel":"IPS","hz":60}',                NULL),
    ('MON-34U',  N'Monitor 34" ultra',   899.00, 9.10,   3, N'{"panel":"OLED","hz":175}',              NULL),
    ('CAB-USBC', N'Cable, USB-C 2m',      19.90, 0.09, 500, N'{"length_m":2}',                         NULL),
    ('DOK-TB4',  N'Dock, Thunderbolt 4', 279.00, 0.65,  22, N'{"ports":11}',                           NULL),
    ('HP-OLD',   N'Headphones (EOL)',     59.00, 0.31,   0, N'{"eol":true}',                           '2024-09-01T12:00:00+02:00'),
    ('MAT-DSK',  N'Desk mat, felt',       34.00, 0.40,  77, N'{"colour":"grå","size":"90x40"}',        NULL);
GO

-- ------------------------------------------------------------------- orders
CREATE TABLE dbo.orders (
    id          bigint IDENTITY(1, 1) PRIMARY KEY,
    customer_id int NOT NULL REFERENCES dbo.customers (id) ON DELETE CASCADE,
    status      varchar(16) NOT NULL CONSTRAINT df_orders_status DEFAULT ('pending')
                CONSTRAINT ck_orders_status CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled', 'refunded')),
    placed_at   datetimeoffset NOT NULL CONSTRAINT df_orders_placed DEFAULT (SYSDATETIMEOFFSET()),
    ship_to     nvarchar(200),
    coupon      varchar(20)
);
GO

CREATE INDEX orders_customer_idx ON dbo.orders (customer_id);
-- Filtered unique index: only unique among pending orders, so row identity
-- must not treat it as a key.
CREATE UNIQUE INDEX orders_one_pending_per_customer
    ON dbo.orders (customer_id) WHERE status = 'pending';
GO

SET IDENTITY_INSERT dbo.orders ON;
INSERT INTO dbo.orders (id, customer_id, status, placed_at, ship_to, coupon)
SELECT i,
       1 + (i % 12),
       CASE i % 4 WHEN 0 THEN 'paid' WHEN 1 THEN 'shipped' WHEN 2 THEN 'cancelled' ELSE 'refunded' END,
       DATEADD(day, -i, SYSDATETIMEOFFSET()),
       CASE WHEN i % 5 = 0 THEN NULL ELSE CONCAT(N'Storgata ', i, N', Oslo') END,
       CASE WHEN i % 6 = 0 THEN 'SUMMER10' END
FROM (SELECT TOP (40) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects) AS n;

INSERT INTO dbo.orders (id, customer_id, status, placed_at, ship_to) VALUES
    (41, 1, 'pending', DATEADD(hour, -2, SYSDATETIMEOFFSET()),    N'Storgata 1, Oslo'),
    (42, 2, 'pending', DATEADD(minute, -20, SYSDATETIMEOFFSET()), NULL),
    (43, 3, 'pending', SYSDATETIMEOFFSET(),                       N'Bryggen 4, Bergen');
SET IDENTITY_INSERT dbo.orders OFF;
GO

-- -------------------------------------------------------------- order_items
-- Composite primary key, two foreign keys, and a persisted computed column
-- (database-assigned, so the editor must treat it as read-only).
CREATE TABLE dbo.order_items (
    order_id   bigint NOT NULL REFERENCES dbo.orders (id) ON DELETE CASCADE,
    line_no    smallint NOT NULL,
    sku        varchar(16) NOT NULL REFERENCES dbo.products (sku),
    quantity   int NOT NULL CONSTRAINT df_items_qty DEFAULT (1) CHECK (quantity > 0),
    unit_price decimal(10, 2) NOT NULL,
    line_total AS (quantity * unit_price) PERSISTED,
    CONSTRAINT pk_order_items PRIMARY KEY (order_id, line_no)
);
GO

INSERT INTO dbo.order_items (order_id, line_no, sku, quantity, unit_price)
SELECT picked.order_id, picked.line_no, picked.sku, picked.quantity, p.price
FROM (
    SELECT o.id AS order_id,
           CAST(n.i AS smallint) AS line_no,
           (SELECT sku FROM dbo.products ORDER BY sku
            OFFSET ((o.id * 3 + n.i) % 10) ROWS FETCH NEXT 1 ROWS ONLY) AS sku,
           1 + ((o.id + n.i) % 4) AS quantity
    FROM dbo.orders o
    CROSS JOIN (SELECT TOP (3) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects) AS n
    WHERE (o.id + n.i) % 3 <> 0
) AS picked
JOIN dbo.products p ON p.sku = picked.sku;
GO

-- ------------------------------------------------------------------- events
-- No primary key and no unique index — and big enough to page, sort and
-- filter through.
CREATE TABLE dbo.events (
    occurred_at datetime2 NOT NULL,
    kind        varchar(20) NOT NULL,
    customer_id int NULL REFERENCES dbo.customers (id),
    payload     nvarchar(max),
    duration_ms int
);
GO

INSERT INTO dbo.events (occurred_at, kind, customer_id, payload, duration_ms)
SELECT DATEADD(minute, -i, SYSDATETIME()),
       CASE i % 5 WHEN 0 THEN 'page_view' WHEN 1 THEN 'click' WHEN 2 THEN 'signup'
                  WHEN 3 THEN 'purchase' ELSE 'error' END,
       CASE WHEN i % 7 = 0 THEN NULL ELSE 1 + (i % 12) END,
       CONCAT(N'{"seq":', i, N',"ok":', CASE WHEN i % 3 <> 0 THEN N'true' ELSE N'false' END,
              N',"path":"/p/', i % 50, N'"}'),
       CASE WHEN i % 11 = 0 THEN NULL ELSE (i * 7) % 900 END
FROM (
    SELECT TOP (5000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i
    FROM sys.all_objects a CROSS JOIN sys.all_objects b
) AS n;
GO

CREATE INDEX events_occurred_idx ON dbo.events (occurred_at);
GO

-- ---------------------------------------------------------- sensor_readings
-- Keyless, but with a full unique index — row identity should use that index.
CREATE TABLE dbo.sensor_readings (
    site       nvarchar(20) NOT NULL,
    sensor     varchar(20) NOT NULL,
    reading_at datetime2 NOT NULL,
    celsius    float,
    humidity   float
);
GO

CREATE UNIQUE INDEX sensor_readings_key ON dbo.sensor_readings (site, sensor, reading_at);
GO

INSERT INTO dbo.sensor_readings (site, sensor, reading_at, celsius, humidity)
SELECT s.site,
       CONCAT('probe-', p.p),
       DATEADD(hour, h.h, CAST('2026-01-01T00:00:00' AS datetime2)),
       ROUND(15 + 10 * SIN(h.h / 3.0) + p.p, 2),
       CASE WHEN h.h % 9 = 0 THEN NULL ELSE ROUND(40 + 20 * COS(h.h / 5.0), 2) END
FROM (VALUES (N'oslo'), (N'bergen'), (N'tromsø')) AS s (site)
CROSS JOIN (SELECT TOP (3) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS p FROM sys.all_objects) AS p
CROSS JOIN (SELECT TOP (200) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) - 1 AS h FROM sys.all_objects) AS h;
GO

-- ---------------------------------------------------------------- all_types
-- One row of ordinary values, one all-NULL row, one row of extremes. Includes
-- rowversion (always database-assigned) and types with no dedicated decoder
-- (sql_variant, hierarchyid) so the display fallbacks get exercised too.
CREATE TABLE dbo.all_types (
    id             int IDENTITY(1, 1) PRIMARY KEY,
    label          varchar(20),
    c_bit          bit,
    c_tinyint      tinyint,
    c_smallint     smallint,
    c_int          int,
    c_bigint       bigint,
    c_decimal      decimal(20, 6),
    c_smallmoney   smallmoney,
    c_money        money,
    c_float        float,
    c_real         real,
    c_date         date,
    c_time         time(7),
    c_smalldatetime smalldatetime,
    c_datetime     datetime,
    c_datetime2    datetime2(7),
    c_datetimeoffset datetimeoffset(7),
    c_char         char(4),
    c_varchar      varchar(32),
    c_varchar_max  varchar(max),
    c_nchar        nchar(4),
    c_nvarchar     nvarchar(32),
    c_nvarchar_max nvarchar(max),
    c_binary       binary(4),
    c_varbinary    varbinary(max),
    c_uuid         uniqueidentifier,
    c_xml          xml,
    c_variant      sql_variant,
    c_hierarchy    hierarchyid,
    c_rowversion   rowversion
);
GO

-- Three separate INSERTs, not one multi-row VALUES: a table constructor
-- derives a single type per column across all its rows, which the deliberately
-- mixed sql_variant values would fail.
INSERT INTO dbo.all_types (label, c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal,
                           c_smallmoney, c_money, c_float, c_real, c_date, c_time,
                           c_smalldatetime, c_datetime, c_datetime2, c_datetimeoffset,
                           c_char, c_varchar, c_varchar_max, c_nchar, c_nvarchar, c_nvarchar_max,
                           c_binary, c_varbinary, c_uuid, c_xml, c_variant, c_hierarchy)
VALUES
    ('typical', 1, 200, 32000, 123456, 9007199254740993, 1234.567890,
     214.75, 1234.56, 2.718281828459045, 3.5, '2026-02-14', '13:45:00.1234567',
     '2026-02-14T13:45:00', '2026-02-14T13:45:00.123', '2026-02-14T13:45:00.1234567',
     '2026-02-14T13:45:00.1234567+01:00',
     'abcd', 'varchar value', 'plain text', N'wxyz', N'nvarchar value', N'plain unicode text',
     0xDEADBEEF, 0xDEADBEEF, '3F333DF6-90A4-4FDA-8DD3-9485D27CEE36',
     '<note>hi</note>', CAST(42 AS int), hierarchyid::Parse('/1/2/'));

INSERT INTO dbo.all_types (label, c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal,
                           c_smallmoney, c_money, c_float, c_real, c_date, c_time,
                           c_smalldatetime, c_datetime, c_datetime2, c_datetimeoffset,
                           c_char, c_varchar, c_varchar_max, c_nchar, c_nvarchar, c_nvarchar_max,
                           c_binary, c_varbinary, c_uuid, c_xml, c_variant, c_hierarchy)
VALUES
    ('all null', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
     NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

INSERT INTO dbo.all_types (label, c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal,
                           c_smallmoney, c_money, c_float, c_real, c_date, c_time,
                           c_smalldatetime, c_datetime, c_datetime2, c_datetimeoffset,
                           c_char, c_varchar, c_varchar_max, c_nchar, c_nvarchar, c_nvarchar_max,
                           c_binary, c_varbinary, c_uuid, c_xml, c_variant, c_hierarchy)
VALUES
    ('extremes', 0, 0, -32768, -2147483648, -9223372036854775808, -99999999999999.999999,
     -214748.3648, -922337203685477.5808, 1.7e308, -3.4e38, '9999-12-31', '00:00:00',
     '1900-01-01T00:00:00', '1753-01-01T00:00:00', '0001-01-01T00:00:00',
     -- +14:00, not -14:00: a negative offset here would push the UTC instant
     -- past datetimeoffset's own upper bound.
     '9999-12-31T23:59:59.9999999+14:00',
     '  x ', 'ünïcödé', REPLICATE(CAST('long cell. ' AS varchar(max)), 900),
     N'🦉  ', N'ünïcödé — 中文 — 🦉', REPLICATE(CAST(N'长文本 ' AS nvarchar(max)), 900),
     0x00000000, CAST(REPLICATE(CAST(0x00FF AS varbinary(max)), 4000) AS varbinary(max)),
     '00000000-0000-0000-0000-000000000000',
     '<empty/>', CAST(N'a variant holding text' AS nvarchar(50)), hierarchyid::Parse('/'));
GO

-- --------------------------------------------------------------------- view
CREATE VIEW dbo.order_summary AS
SELECT o.id AS order_id,
       c.name AS customer,
       o.status,
       o.placed_at,
       COUNT(i.line_no) AS lines,
       COALESCE(SUM(i.line_total), 0) AS total
FROM dbo.orders o
JOIN dbo.customers c ON c.id = o.customer_id
LEFT JOIN dbo.order_items i ON i.order_id = o.id
GROUP BY o.id, c.name, o.status, o.placed_at;
GO

-- ------------------------------------------------------- analytics (schema)
CREATE TABLE analytics.daily_sales (
    day     date NOT NULL,
    sku     varchar(16) NOT NULL REFERENCES dbo.products (sku),
    units   int NOT NULL,
    revenue decimal(12, 2) NOT NULL,
    CONSTRAINT pk_daily_sales PRIMARY KEY (day, sku)
);
GO

INSERT INTO analytics.daily_sales (day, sku, units, revenue)
SELECT d.day, p.sku, u.units, ROUND(u.units * p.price, 2)
FROM (
    SELECT TOP (90) DATEADD(day, ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) - 1,
                            CAST('2026-01-01' AS date)) AS day
    FROM sys.all_objects
) AS d
CROSS JOIN dbo.products p
CROSS APPLY (SELECT (DAY(d.day) * LEN(p.sku)) % 17 AS units) AS u
WHERE p.discontinued_at IS NULL;
GO

-- Composite foreign key into sensor_readings' unique index, to exercise
-- multi-column foreign-key navigation.
CREATE TABLE analytics.sensor_alerts (
    id         int IDENTITY(1, 1) PRIMARY KEY,
    site       nvarchar(20) NOT NULL,
    sensor     varchar(20) NOT NULL,
    reading_at datetime2 NOT NULL,
    severity   varchar(10) NOT NULL,
    CONSTRAINT fk_sensor_alerts_reading FOREIGN KEY (site, sensor, reading_at)
        REFERENCES dbo.sensor_readings (site, sensor, reading_at)
);
GO

INSERT INTO analytics.sensor_alerts (site, sensor, reading_at, severity)
SELECT TOP (60) site, sensor, reading_at,
       CASE WHEN celsius > 24 THEN 'critical' ELSE 'warning' END
FROM dbo.sensor_readings
WHERE celsius > 23
ORDER BY reading_at;
GO
