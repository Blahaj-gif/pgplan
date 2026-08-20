//! A real Postgres per test, downloaded and started by `postgresql_embedded`.
//!
//! A plan is the database's opinion. Every assertion in this project is about
//! what a planner actually chose, so a model of a planner would be a model of
//! the thing under test — which is the one thing a test may not be.

#![allow(dead_code)]

use std::time::Duration;

use postgresql_embedded::blocking::PostgreSQL;
use postgresql_embedded::Settings;

/// The default is fifteen seconds, which is enough to start a server that is
/// already unpacked and nowhere near enough for the first run, where the
/// binaries are downloaded and `initdb` builds a cluster from nothing. The
/// symptom is every test failing at once with "deadline has elapsed", which
/// reads like a broken suite rather than a slow download.
const SETUP_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Db {
    server: PostgreSQL,
    url: String,
}

impl Db {
    pub fn start() -> Db {
        let settings = Settings {
            timeout: Some(SETUP_TIMEOUT),
            ..Settings::default()
        };
        let mut server = PostgreSQL::new(settings);
        server.setup().expect("postgres could not be set up");
        server.start().expect("postgres could not be started");
        let name = "plans";
        if !server.database_exists(name).unwrap_or(false) {
            server.create_database(name).expect("database");
        }
        let url = server.settings().url(name);
        Db { server, url }
    }

    pub fn client(&self) -> postgres::Client {
        postgres::Client::connect(&self.url, postgres::NoTls).expect("connect")
    }

    pub fn apply(&self, sql: &str) {
        self.client().batch_execute(sql).expect("ddl");
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = self.server.stop();
    }
}

/// A table big enough that a sequential scan of it is a real cost, with an
/// index the planner will genuinely prefer.
///
/// Fifty thousand rows rather than a token few: the entire question this
/// project asks is what the planner does at volume, and at two hundred rows it
/// correctly does the thing that would be a regression at two million.
pub const ORDERS: &str = "
    CREATE TABLE orders (
        id       int PRIMARY KEY,
        user_id  int NOT NULL,
        total    numeric(10,2) NOT NULL
    );
    INSERT INTO orders
    SELECT g, g % 5000, (g % 997)::numeric / 100
    FROM generate_series(1, 50000) g;
    CREATE INDEX orders_user_id_idx ON orders (user_id);
    ANALYZE orders;
";
