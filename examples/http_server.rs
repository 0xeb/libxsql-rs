// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interactive HTTP-server demo for libxsql-rs.
//!
//! Serves a small store/CRM database — made of index, cached, and generator
//! virtual tables plus real tables and views — over the thinclient HTTP server
//! (a hardened blocking stack: `tiny_http` server + `ureq` client), so it can be
//! driven with `curl`:
//!
//! ```text
//! cargo run --example http_server --all-features
//! curl -s localhost:8077/query -d "SELECT * FROM order_details"
//! curl -s "localhost:8077/query?format=csv" -d "SELECT * FROM product_revenue"
//! ```
//!
//! On startup it runs an in-process [`ThinClient`](libxsql::thinclient::ThinClient)
//! self-check (a `/status` + one query round-trip) to show the client side, then
//! blocks serving `curl` until `POST /shutdown`.
//!
//! Architecture note: `libxsql::Database` is `!Send` (it holds `Rc`s), but the HTTP
//! query handler must be `Send + Sync`. So a dedicated *DB-owner thread* holds the
//! single `Database` and the HTTP handler forwards SQL to it over a channel,
//! receiving back a JSON result envelope.

#[cfg(all(feature = "thinclient", feature = "serde"))]
fn main() -> libxsql::Result<()> {
    demo::run()
}

#[cfg(not(all(feature = "thinclient", feature = "serde")))]
fn main() {
    eprintln!("run with --all-features (this example needs the thinclient + serde features)");
}

#[cfg(all(feature = "thinclient", feature = "serde"))]
mod demo {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use libxsql::thinclient::{ClientConfig, HttpQueryServer, HttpQueryServerConfig, ThinClient};
    use libxsql::{Database, Generator, ScriptExecutionMode};
    use std::time::Duration;

    const PORT: u16 = 8077;

    // ---- backing data types -------------------------------------------------

    #[derive(Clone)]
    struct Product {
        id: i32,
        name: String,
        category: String,
        price: f64,
        stock: i32,
    }

    #[derive(Clone)]
    struct Customer {
        id: i64,
        name: String,
        region_id: i64,
        tier: String,
    }

    #[derive(Clone)]
    struct Order {
        id: i64,
        customer_id: i64,
        product_id: i64,
        qty: i32,
        total: f64,
    }

    #[derive(Clone)]
    struct AuditEntry {
        seq: i64,
        action: String,
        detail: String,
    }

    // ---- generators ----------------------------------------------------------

    /// Streams a fixed list of audit entries one at a time (so `LIMIT` stops work
    /// early).
    struct AuditGenerator {
        rows: Rc<Vec<AuditEntry>>,
        pos: Option<usize>,
    }

    impl Generator<AuditEntry> for AuditGenerator {
        fn next(&mut self) -> libxsql::Result<bool> {
            let next = self.pos.map_or(0, |p| p + 1);
            if next < self.rows.len() {
                self.pos = Some(next);
                Ok(true)
            } else {
                self.pos = None;
                Ok(false)
            }
        }
        fn current(&self) -> &AuditEntry {
            &self.rows[self.pos.expect("audit current row is valid")]
        }
        fn rowid(&self) -> i64 {
            self.current().seq
        }
    }

    /// A `generate_series`-style table-valued function: yields `start..=stop`.
    struct SeriesGenerator {
        next_val: i64,
        stop: i64,
        value: i64,
    }

    impl SeriesGenerator {
        fn new(start: i64, stop: i64) -> Self {
            Self {
                next_val: start,
                stop,
                value: 0,
            }
        }
    }

    impl Generator<i64> for SeriesGenerator {
        fn next(&mut self) -> libxsql::Result<bool> {
            if self.next_val > self.stop {
                return Ok(false);
            }
            self.value = self.next_val;
            self.next_val += 1;
            Ok(true)
        }
        fn current(&self) -> &i64 {
            &self.value
        }
        fn rowid(&self) -> i64 {
            self.value
        }
    }

    // ---- database construction (runs entirely on the DB-owner thread) --------

    fn build_database() -> libxsql::Result<Database> {
        let mut db = Database::open_memory()?;

        // 1) products — INDEX table (writable: restock/rename/delete/insert).
        let products = Rc::new(RefCell::new(vec![
            product(1, "Widget", "Hardware", 9.99, 120),
            product(2, "Gadget", "Hardware", 19.99, 8),
            product(3, "Gizmo", "Hardware", 14.50, 0),
            product(4, "Doohickey", "Accessories", 4.25, 200),
            product(5, "Sprocket", "Parts", 2.10, 35),
            product(6, "Cog", "Parts", 1.75, 5),
        ]));
        let products_def = libxsql::table("products")
            .count({
                let products = products.clone();
                move || products.borrow().len()
            })
            .column_int("id", {
                let products = products.clone();
                move |i| products.borrow()[i].id
            })
            .column_text_rw(
                "name",
                {
                    let products = products.clone();
                    move |i| products.borrow()[i].name.clone()
                },
                {
                    let products = products.clone();
                    move |i, value| {
                        products.borrow_mut()[i].name = value.to_string();
                        true
                    }
                },
            )
            .column_text("category", {
                let products = products.clone();
                move |i| products.borrow()[i].category.clone()
            })
            .column_double("price", {
                let products = products.clone();
                move |i| products.borrow()[i].price
            })
            .column_int_rw(
                "stock",
                {
                    let products = products.clone();
                    move |i| products.borrow()[i].stock
                },
                {
                    let products = products.clone();
                    move |i, value| {
                        products.borrow_mut()[i].stock = value;
                        true
                    }
                },
            )
            .deletable({
                let products = products.clone();
                move |i| {
                    products.borrow_mut().remove(i);
                    true
                }
            })
            .insertable({
                let products = products.clone();
                move |values| {
                    products.borrow_mut().push(Product {
                        id: values[0].as_i32(),
                        name: values[1].as_text(),
                        category: values[2].as_text(),
                        price: values[3].as_f64(),
                        stock: values[4].as_i32(),
                    });
                    true
                }
            })
            .build()?;
        db.register_and_create_table(&products_def)?;

        // 2) customers — CACHED table with a hash index on region_id.
        let customers = Rc::new(vec![
            customer(1, "Acme Corp", 1, "gold"),
            customer(2, "Globex", 2, "silver"),
            customer(3, "Initech", 1, "bronze"),
            customer(4, "Umbrella", 3, "gold"),
        ]);
        let customers_def = libxsql::cached_table::<Customer>("customers")
            .estimate_rows({
                let customers = customers.clone();
                move || customers.len()
            })
            .cache_builder({
                let customers = customers.clone();
                move |cache| cache.extend(customers.iter().cloned())
            })
            .column_i64("id", |c| c.id)
            .column_text("name", |c| c.name.clone())
            .column_i64("region_id", |c| c.region_id)
            .column_text("tier", |c| c.tier.clone())
            .index_on("region_id", |c| c.region_id)
            .build()?;
        db.register_and_create_cached_table(&customers_def)?;

        // 3) orders — CACHED table with two hash indexes.
        let orders = Rc::new(vec![
            order(1, 1, 1, 10, 99.90),
            order(2, 1, 4, 100, 425.00),
            order(3, 2, 2, 2, 39.98),
            order(4, 2, 5, 50, 105.00),
            order(5, 3, 1, 5, 49.95),
            order(6, 3, 6, 3, 5.25),
            order(7, 4, 2, 1, 19.99),
            order(8, 4, 3, 4, 58.00),
            order(9, 1, 5, 20, 42.00),
            order(10, 2, 4, 10, 42.50),
        ]);
        let orders_def = libxsql::cached_table::<Order>("orders")
            .estimate_rows({
                let orders = orders.clone();
                move || orders.len()
            })
            .cache_builder({
                let orders = orders.clone();
                move |cache| cache.extend(orders.iter().cloned())
            })
            .column_i64("id", |o| o.id)
            .column_i64("customer_id", |o| o.customer_id)
            .column_i64("product_id", |o| o.product_id)
            .column_int("qty", |o| o.qty)
            .column_double("total", |o| o.total)
            .index_on("customer_id", |o| o.customer_id)
            .index_on("product_id", |o| o.product_id)
            .build()?;
        db.register_and_create_cached_table(&orders_def)?;

        // 4) audit_log — streaming GENERATOR.
        let audit = Rc::new(vec![
            audit_entry(1, "login", "acme signed in"),
            audit_entry(2, "order", "#1 placed by acme"),
            audit_entry(3, "restock", "widget +120"),
            audit_entry(4, "login", "globex signed in"),
            audit_entry(5, "order", "#3 placed by globex"),
            audit_entry(6, "price", "gizmo repriced"),
            audit_entry(7, "order", "#7 placed by umbrella"),
            audit_entry(8, "logout", "acme signed out"),
        ]);
        let audit_def = libxsql::generator_table::<AuditEntry>("audit_log")
            .estimate_rows({
                let audit = audit.clone();
                move || audit.len()
            })
            .generator({
                let audit = audit.clone();
                move || {
                    Box::new(AuditGenerator {
                        rows: audit.clone(),
                        pos: None,
                    }) as Box<dyn Generator<AuditEntry>>
                }
            })
            .column_i64("seq", |e| e.seq)
            .column_text("action", |e| e.action.clone())
            .column_text("detail", |e| e.detail.clone())
            .build()?;
        db.register_and_create_generator_table(&audit_def)?;

        // 5) series — GENERATOR table-valued function driven by hidden columns.
        let series_def = libxsql::generator_table::<i64>("series")
            .estimate_rows(|| 0)
            // Full-scan generator is never used (full_scan_error rejects scans
            // without start/stop); provide an empty one to satisfy the builder.
            .generator(|| Box::new(SeriesGenerator::new(1, 0)) as Box<dyn Generator<i64>>)
            .column_i64("n", |n| *n)
            .hidden_column_i64("start")
            .hidden_column_i64("stop")
            .parametric_filter(
                ["start", "stop"],
                |args| {
                    Box::new(SeriesGenerator::new(args[0].as_i64(), args[1].as_i64()))
                        as Box<dyn Generator<i64>>
                },
                1.0,
                10.0,
            )
            .full_scan_error("series requires start and stop, e.g. WHERE start = 1 AND stop = 10")
            .build()?;
        db.register_and_create_generator_table(&series_def)?;

        // Custom scalar functions.
        db.register_function("money", 1, |ctx, args| {
            if args[0].is_null() {
                ctx.result_null();
            } else {
                ctx.result_text(format!("${:.2}", args[0].as_f64()));
            }
        })?;
        db.register_function("initials", 1, |ctx, args| {
            let initials: String = args[0]
                .as_text()
                .split_whitespace()
                .filter_map(|word| word.chars().next())
                .collect::<String>()
                .to_uppercase();
            ctx.result_text(&initials);
        })?;

        // Real table + views over the virtual tables.
        db.exec(
            "CREATE TABLE notes(entity TEXT, note TEXT);
             INSERT INTO notes(entity, note) VALUES
                 ('Widget', 'best seller'),
                 ('Globex', 'net-30 terms');

             CREATE VIEW order_details AS
                 SELECT o.id AS order_id, c.name AS customer, p.name AS product,
                        o.qty, o.total
                 FROM orders o
                 JOIN customers c ON c.id = o.customer_id
                 JOIN products  p ON p.id = o.product_id;

             CREATE VIEW customer_spend AS
                 SELECT c.name AS customer, SUM(o.total) AS spend
                 FROM orders o JOIN customers c ON c.id = o.customer_id
                 GROUP BY c.id ORDER BY spend DESC;

             CREATE VIEW product_revenue AS
                 SELECT p.name AS product, money(SUM(o.total)) AS revenue
                 FROM orders o JOIN products p ON p.id = o.product_id
                 GROUP BY p.id ORDER BY SUM(o.total) DESC;

             CREATE VIEW low_stock AS
                 SELECT name, stock FROM products WHERE stock < 10 ORDER BY stock;",
        )?;

        Ok(db)
    }

    fn product(id: i32, name: &str, category: &str, price: f64, stock: i32) -> Product {
        Product {
            id,
            name: name.to_string(),
            category: category.to_string(),
            price,
            stock,
        }
    }
    fn customer(id: i64, name: &str, region_id: i64, tier: &str) -> Customer {
        Customer {
            id,
            name: name.to_string(),
            region_id,
            tier: tier.to_string(),
        }
    }
    fn order(id: i64, customer_id: i64, product_id: i64, qty: i32, total: f64) -> Order {
        Order {
            id,
            customer_id,
            product_id,
            qty,
            total,
        }
    }
    fn audit_entry(seq: i64, action: &str, detail: &str) -> AuditEntry {
        AuditEntry {
            seq,
            action: action.to_string(),
            detail: detail.to_string(),
        }
    }

    // ---- request handling ----------------------------------------------------

    /// Run a SQL request (possibly multi-statement) and return a JSON envelope.
    fn run_and_format(db: &mut Database, sql: &str) -> String {
        match db.run_script(sql, ScriptExecutionMode::ContinueOnError) {
            Ok(result) => libxsql::script_result_to_json(&result)
                .unwrap_or_else(|err| error_json(&err.to_string())),
            Err(err) => error_json(&err.to_string()),
        }
    }

    fn error_json(message: &str) -> String {
        // Reuse the library's own error-envelope formatter (matches server errors).
        libxsql::thinclient::make_error_json(message)
    }

    type Job = (String, Sender<String>);

    pub fn run() -> libxsql::Result<()> {
        // DB-owner thread: owns the (non-Send) Database and serves SQL jobs.
        let (tx, rx) = mpsc::channel::<Job>();
        let db_thread = thread::Builder::new()
            .name("xsql-db".to_string())
            .spawn(move || {
                let mut db = build_database().expect("failed to build demo database");
                for (sql, reply) in rx {
                    let _ = reply.send(run_and_format(&mut db, &sql));
                }
            })
            .expect("spawn db thread");

        // HTTP query handler (Send + Sync): forward SQL to the DB thread.
        let tx = Arc::new(Mutex::new(tx));
        let server = HttpQueryServer::new(
            HttpQueryServerConfig {
                tool_name: "storesql".to_string(),
                help_text: HELP.to_string(),
                bind_address: "127.0.0.1".to_string(),
                port: PORT,
                ..Default::default()
            },
            move |sql| {
                let (reply_tx, reply_rx) = mpsc::channel::<String>();
                tx.lock()
                    .expect("query channel lock")
                    .send((sql.to_string(), reply_tx))
                    .map_err(|_| libxsql::Error::Message("db thread is gone".to_string()))?;
                reply_rx
                    .recv()
                    .map_err(|_| libxsql::Error::Message("db thread dropped the reply".to_string()))
            },
        );

        let mut handle = server.start()?;
        self_check(handle.port());
        print_banner(&handle.url());

        // Block until POST /shutdown (then let the DB thread wind down).
        handle.join()?;
        drop(handle);
        let _ = db_thread.join();
        Ok(())
    }

    /// In-process round-trip over the ureq-backed [`ThinClient`]: confirm
    /// `/status` and run one query against the just-started server.
    fn self_check(port: u16) {
        let client = ThinClient::new(ClientConfig {
            host: "127.0.0.1".to_string(),
            port,
            timeout: Duration::from_secs(5),
            token: None,
        });
        match client.status() {
            Ok(status) => println!("self-check  GET /status -> {status}"),
            Err(err) => println!("self-check  GET /status failed: {err}"),
        }
        match client.query("SELECT name, price FROM products ORDER BY price DESC LIMIT 1") {
            Ok(body) => println!("self-check  POST /query -> {body}"),
            Err(err) => println!("self-check  POST /query failed: {err}"),
        }
        println!();
    }

    fn print_banner(url: &str) {
        println!("storesql demo server listening on {url}");
        println!();
        println!("Virtual tables:");
        println!("  products    (index, writable)   id,name,category,price,stock");
        println!("  customers   (cached + index)    id,name,region_id,tier");
        println!("  orders      (cached + 2 indexes) id,customer_id,product_id,qty,total");
        println!("  audit_log   (generator/stream)  seq,action,detail");
        println!("  series      (generator TVF)     n  [hidden: start, stop]");
        println!("Real table: notes(entity, note)");
        println!("Views: order_details, customer_spend, product_revenue, low_stock");
        println!("Functions: money(x), initials(name)");
        println!();
        println!("POST SQL to /query (body = SQL, response = JSON envelope). Examples:");
        println!("  curl -s localhost:{PORT}/query -d \"SELECT * FROM order_details\"");
        println!(
            "  curl -s localhost:{PORT}/query -d \"SELECT n FROM series WHERE start=1 AND stop=5\""
        );
        println!("  curl -s localhost:{PORT}/query -d \"SELECT * FROM audit_log LIMIT 3\"");
        println!("  curl -s localhost:{PORT}/query -d \"SELECT * FROM customer_spend\"");
        println!(
            "  curl -s \"localhost:{PORT}/query?format=csv\" -d \"SELECT * FROM product_revenue\""
        );
        println!("  curl -s -X POST localhost:{PORT}/shutdown   # stop the server");
    }

    const HELP: &str = "storesql demo. POST SQL to /query; tables: products, customers, \
orders, audit_log, series (WHERE start=.. AND stop=..); views: order_details, \
customer_spend, product_revenue, low_stock; functions: money(x), initials(name).";
}
