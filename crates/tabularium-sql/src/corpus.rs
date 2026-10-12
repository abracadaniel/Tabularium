//! Realistic SQL corpus shared by the formatter and inspection tests.

use tabularium_core::Dialect;

pub(crate) const CORPUS: &[(&str, Dialect, &str)] = &[
    (
        "simple join",
        Dialect::Postgres,
        "select u.id, u.name, o.total from users u join orders o on o.user_id = u.id where o.total > 100 and u.active = true order by o.total desc limit 10;",
    ),
    (
        "left join group by having",
        Dialect::Postgres,
        "SELECT c.name, COUNT(o.id) AS order_count, SUM(o.amount) AS revenue\nFROM customers c\nLEFT OUTER JOIN orders o ON o.customer_id = c.id AND o.status <> 'cancelled'\nWHERE c.created_at >= DATE '2024-01-01'\nGROUP BY c.name\nHAVING SUM(o.amount) > 1000 OR COUNT(o.id) > 5\nORDER BY revenue DESC NULLS LAST;",
    ),
    (
        "cte recursive",
        Dialect::Postgres,
        "WITH RECURSIVE tree AS (\n  SELECT id, parent_id, name, 1 AS depth FROM categories WHERE parent_id IS NULL\n  UNION ALL\n  SELECT c.id, c.parent_id, c.name, t.depth + 1 FROM categories c JOIN tree t ON c.parent_id = t.id\n), leaves AS (SELECT * FROM tree WHERE depth > 2)\nSELECT * FROM leaves ORDER BY depth, name;",
    ),
    (
        "subqueries",
        Dialect::Postgres,
        "select * from (select user_id, max(created_at) as last_seen from events group by user_id) e where e.user_id in (select id from users where email like '%@example.com') and exists (select 1 from bans b where b.user_id = e.user_id);",
    ),
    (
        "case expression",
        Dialect::Generic,
        "SELECT id, CASE WHEN score >= 90 THEN 'A' WHEN score >= 80 THEN 'B' ELSE CASE WHEN score IS NULL THEN 'n/a' ELSE 'C' END END AS grade, -score AS neg FROM results WHERE score BETWEEN 0 AND 100;",
    ),
    (
        "window functions",
        Dialect::Postgres,
        "SELECT department, employee, salary, ROW_NUMBER() OVER (PARTITION BY department ORDER BY salary DESC) AS rn, AVG(salary) OVER (PARTITION BY department) AS avg_sal, LAG(salary, 1) OVER w AS prev FROM staff WINDOW w AS (ORDER BY hired_at) ORDER BY department, rn;",
    ),
    (
        "insert select",
        Dialect::Postgres,
        "INSERT INTO archive.orders (id, customer_id, total, archived_at) SELECT id, customer_id, total, now() FROM orders WHERE created_at < now() - INTERVAL '1 year' RETURNING id;",
    ),
    (
        "insert values upsert",
        Dialect::Postgres,
        "insert into settings (key, value) values ('theme', 'dark'), ('lang', 'en'), ('tz', 'UTC') on conflict (key) do update set value = excluded.value, updated_at = now();",
    ),
    (
        "update delete",
        Dialect::Postgres,
        "UPDATE accounts SET balance = balance - 100.50, updated_at = CURRENT_TIMESTAMP WHERE id = 42 AND balance >= 100.50;\nDELETE FROM sessions WHERE expires_at < now() - interval '30 days';",
    ),
    (
        "create table postgres",
        Dialect::Postgres,
        "CREATE TABLE IF NOT EXISTS public.invoices (\n  id bigserial PRIMARY KEY,\n  customer_id bigint NOT NULL REFERENCES customers (id) ON DELETE CASCADE,\n  amount numeric(12, 2) NOT NULL DEFAULT 0 CHECK (amount >= 0),\n  issued_at timestamptz NOT NULL DEFAULT now(),\n  note text,\n  UNIQUE (customer_id, issued_at)\n);\nCREATE INDEX idx_invoices_customer ON public.invoices (customer_id);",
    ),
    (
        "postgres function dollar body",
        Dialect::Postgres,
        "CREATE OR REPLACE FUNCTION add_points(p_user integer, p_points integer)\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nDECLARE\n  total integer;\nBEGIN\n  UPDATE users SET points = points + p_points WHERE id = p_user RETURNING points INTO total;\n  RETURN total;\nEND;\n$$;\nSELECT add_points(1, 10);",
    ),
    (
        "postgres do block and casts",
        Dialect::Postgres,
        "DO $body$ BEGIN RAISE NOTICE 'hello'; END $body$;\nSELECT '2024-01-01'::date, data->>'name' AS name, data #>> '{a,b}' AS ab, tags @> ARRAY['x']::text[] FROM docs WHERE id = $1;",
    ),
    (
        "mysql procedure delimiter",
        Dialect::MySql,
        "DELIMITER $$\nCREATE PROCEDURE add_order(IN p_customer INT, IN p_total DECIMAL(10,2))\nBEGIN\n  DECLARE v_id INT;\n  INSERT INTO orders (customer_id, total) VALUES (p_customer, p_total);\n  SET v_id = LAST_INSERT_ID();\n  IF p_total > 1000 THEN\n    UPDATE customers SET vip = 1 WHERE id = p_customer;\n  END IF;\n  SELECT v_id;\nEND$$\nDELIMITER ;\nCALL add_order(1, 99.90);",
    ),
    (
        "mysql queries",
        Dialect::MySql,
        "SELECT `o`.`id`, GROUP_CONCAT(DISTINCT p.name ORDER BY p.name SEPARATOR ', ') AS products # list\nFROM `orders` o\nINNER JOIN order_items i ON i.order_id = o.id\nJOIN products p ON p.id = i.product_id\nWHERE o.created_at > NOW() - INTERVAL 7 DAY AND o.note <> \"x\"\nGROUP BY o.id\nLIMIT 20, 10;\nINSERT INTO stats (day, hits) VALUES (CURDATE(), 1) ON DUPLICATE KEY UPDATE hits = hits + 1;",
    ),
    (
        "sqlite trigger",
        Dialect::Sqlite,
        "CREATE TABLE IF NOT EXISTS log (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT NOT NULL, at TEXT DEFAULT CURRENT_TIMESTAMP);\nCREATE TRIGGER trg_users_insert AFTER INSERT ON users\nBEGIN\n  INSERT INTO log (msg) VALUES ('user ' || NEW.id || ' created');\n  UPDATE counters SET n = n + 1 WHERE name = 'users';\nEND;\nSELECT * FROM log ORDER BY id DESC LIMIT 5;",
    ),
    (
        "sqlite queries",
        Dialect::Sqlite,
        "SELECT name, strftime('%Y', created) AS yr, ifnull(email, '-') FROM [users] WHERE id IN (SELECT user_id FROM orders GROUP BY user_id HAVING count(*) > 2) AND name LIKE ?;\nINSERT OR REPLACE INTO kv (k, v) VALUES (:key, :value);",
    ),
    (
        "mssql batch",
        Dialect::MsSql,
        "SELECT TOP 10 [o].[Id], [c].[Name] FROM [dbo].[Orders] AS [o] INNER JOIN dbo.Customers c ON c.Id = o.CustomerId WHERE o.Total > @minTotal ORDER BY o.Total DESC;\nGO\nCREATE PROCEDURE dbo.GetOrders @CustomerId INT\nAS\nBEGIN\n  SET NOCOUNT ON;\n  SELECT * FROM dbo.Orders WHERE CustomerId = @CustomerId;\nEND\nGO",
    ),
    (
        "comments everywhere",
        Dialect::Postgres,
        "-- report query\n/* multi\n   line */\nSELECT a, -- first\n  b /* inline */ , c\nFROM t -- table\nWHERE /* cond */ a = 1\n-- trailing\n;\nSELECT 2; -- after",
    ),
    (
        "union and set ops",
        Dialect::Generic,
        "SELECT id, 'a' AS src FROM a UNION ALL SELECT id, 'b' FROM b EXCEPT SELECT id, 'c' FROM c ORDER BY 1;",
    ),
    (
        "unicode and quoting",
        Dialect::Postgres,
        "SELECT \"Prénom\", 'Grüße 😀', имя AS \"Имя\" FROM \"Ünïcode Table\" t WHERE t.\"Prénom\" = 'Zoë' AND t.score::numeric(5,2) > -1.5e3;",
    ),
    (
        "transactions and misc",
        Dialect::Postgres,
        "BEGIN;\nLOCK TABLE accounts IN EXCLUSIVE MODE;\nUPDATE accounts SET locked = true WHERE id = ANY($1);\nCOMMIT;\nEXPLAIN ANALYZE SELECT * FROM accounts WHERE id = 1;\nSET search_path TO public, archive;",
    ),
    (
        "lateral and distinct on",
        Dialect::Postgres,
        "SELECT DISTINCT ON (u.id) u.id, x.v FROM users u CROSS JOIN LATERAL (SELECT v FROM vals WHERE vals.uid = u.id ORDER BY v DESC LIMIT 1) x ORDER BY u.id, x.v;",
    ),
    (
        "alter and drop",
        Dialect::Postgres,
        "ALTER TABLE users ADD COLUMN last_login timestamptz;\nALTER TABLE users ALTER COLUMN name SET NOT NULL;\nDROP TABLE IF EXISTS tmp_import CASCADE;\nCREATE VIEW active_users AS SELECT id, name FROM users WHERE active;",
    ),
];
