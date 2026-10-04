use crate::{
    Order, OrderStatus, PaymentKind, PaymentMethod, Phase, SUPPORTED_PAYMENT_METHODS,
    SupportedPaymentMethod, Trade, i64_from_shannons,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

const TRADE_COLUMNS: &str = "id, order_id, seller_nostr, seller_fiber, buyer_nostr, buyer_fiber, \
     fiat_amount, shannons, state, hold_payment_hash, hold_invoice, payout_invoice, \
     payout_payment_hash, hold_received_at, payment_method_id, payment_kind, payment_label, \
     payment_currency";

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .mode(0o600)
                .open(path)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.create()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        db.create()?;
        Ok(db)
    }

    fn create(&self) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS orders (
                id TEXT PRIMARY KEY,
                side TEXT NOT NULL,
                maker_nostr TEXT NOT NULL,
                maker_fiber TEXT NOT NULL,
                available_shannons INTEGER NOT NULL,
                fiat_currency TEXT NOT NULL,
                price_per_ckb TEXT NOT NULL,
                min TEXT NOT NULL,
                max TEXT NOT NULL,
                status TEXT NOT NULL,
                hold_secs INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS supported_payment_methods (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                currency TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS payment_methods (
                order_id TEXT NOT NULL,
                method_id TEXT NOT NULL,
                PRIMARY KEY (order_id, method_id),
                FOREIGN KEY(order_id) REFERENCES orders(id),
                FOREIGN KEY(method_id) REFERENCES supported_payment_methods(id)
            );
            CREATE TABLE IF NOT EXISTS trades (
                id TEXT PRIMARY KEY,
                order_id TEXT NOT NULL,
                seller_nostr TEXT NOT NULL,
                seller_fiber TEXT NOT NULL,
                buyer_nostr TEXT NOT NULL,
                buyer_fiber TEXT NOT NULL,
                fiat_amount TEXT NOT NULL,
                shannons INTEGER NOT NULL,
                state TEXT NOT NULL,
                hold_payment_hash TEXT,
                hold_invoice TEXT,
                payout_invoice TEXT,
                payout_payment_hash TEXT,
                hold_received_at INTEGER,
                payment_method_id TEXT NOT NULL,
                payment_kind TEXT NOT NULL,
                payment_label TEXT NOT NULL,
                payment_currency TEXT NOT NULL,
                FOREIGN KEY(order_id) REFERENCES orders(id)
            );
            CREATE TABLE IF NOT EXISTS preimages (
                payment_hash TEXT PRIMARY KEY,
                preimage TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS processed_events (
                event_id TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS fiber_node (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                pubkey TEXT NOT NULL
            );
            ",
        )?;
        sync_supported(&conn)?;
        Ok(())
    }

    pub fn supported_payment_methods(&self) -> Result<Vec<SupportedPaymentMethod>> {
        let conn = self.conn.lock().expect("db");
        load_supported(&conn)
    }

    pub fn mark_event(&self, event_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("db");
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO processed_events (event_id) VALUES (?1)",
            params![event_id],
        )?;
        Ok(inserted == 1)
    }

    pub fn insert_order(&self, order: &Order) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO orders (
                id, side, maker_nostr, maker_fiber,
                available_shannons, fiat_currency, price_per_ckb, min, max,
                status, hold_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                order.id,
                order.side,
                order.maker_nostr,
                order.maker_fiber,
                i64_from_shannons(order.available_shannons)?,
                order.fiat_currency,
                order.price_per_ckb,
                order.min,
                order.max,
                order.status,
                i64::try_from(order.hold_secs).context("hold does not fit sqlite integer")?,
            ],
        )?;
        for method in &order.payment_methods {
            insert_method(&tx, &order.id, method)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_order(&self, id: &str) -> Result<Option<Order>> {
        let conn = self.conn.lock().expect("db");
        let mut stmt = conn.prepare(
            "SELECT id, side, maker_nostr, maker_fiber,
                    available_shannons, fiat_currency, price_per_ckb, min, max,
                    status, hold_secs
             FROM orders WHERE id = ?1",
        )?;
        let order = stmt
            .query_row(params![id], |row| {
                Ok(Order {
                    id: row.get(0)?,
                    side: row.get(1)?,
                    maker_nostr: row.get(2)?,
                    maker_fiber: row.get(3)?,
                    available_shannons: read_shannons(row.get(4)?, 4)?,
                    fiat_currency: row.get(5)?,
                    price_per_ckb: row.get(6)?,
                    min: row.get(7)?,
                    max: row.get(8)?,
                    payment_methods: Vec::new(),
                    status: row.get(9)?,
                    hold_secs: read_u64(row.get(10)?, 10)?,
                })
            })
            .optional()?;
        let Some(mut order) = order else {
            return Ok(None);
        };
        order.payment_methods = load_methods(&conn, id)?;
        Ok(Some(order))
    }

    /// Debit the order, insert the trade, and store the hold preimage together.
    pub fn commit_take(&self, trade: &Trade, preimage: &str) -> Result<()> {
        let hash = trade
            .hold_payment_hash
            .as_deref()
            .context("take is missing a hold hash")?;
        let shannons = i64_from_shannons(trade.shannons)?;
        let conn = self.conn.lock().expect("db");
        let tx = conn.unchecked_transaction()?;
        if open_trades_for(&tx, &trade.order_id)? > 0 {
            bail!("order already has an open trade");
        }
        let updated = tx.execute(
            "UPDATE orders SET available_shannons = available_shannons - ?1
             WHERE id = ?2 AND status = ?3 AND available_shannons >= ?1",
            params![shannons, trade.order_id, OrderStatus::Open.as_str()],
        )?;
        if updated != 1 {
            bail!("order cannot fund this take");
        }
        tx.execute(
            "INSERT INTO trades (
                id, order_id, seller_nostr, seller_fiber, buyer_nostr, buyer_fiber,
                fiat_amount, shannons, state, hold_payment_hash, hold_invoice,
                payout_invoice, payout_payment_hash, hold_received_at,
                payment_method_id, payment_kind, payment_label, payment_currency
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                trade.id,
                trade.order_id,
                trade.seller_nostr,
                trade.seller_fiber,
                trade.buyer_nostr,
                trade.buyer_fiber,
                trade.fiat_amount,
                shannons,
                trade.state,
                hash,
                trade.hold_invoice,
                trade.payout_invoice,
                trade.payout_payment_hash,
                trade.hold_received_at,
                trade.payment_method_id,
                trade.payment_kind,
                trade.payment_label,
                trade.payment_currency,
            ],
        )?;
        tx.execute(
            "INSERT INTO preimages (payment_hash, preimage) VALUES (?1, ?2)",
            params![hash, preimage],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn rollback_take(
        &self,
        order_id: &str,
        trade_id: &str,
        payment_hash: &str,
        shannons: u128,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM preimages WHERE payment_hash = ?1",
            params![payment_hash],
        )?;
        tx.execute("DELETE FROM trades WHERE id = ?1", params![trade_id])?;
        add_available(&tx, order_id, shannons)?;
        tx.commit()?;
        Ok(())
    }

    /// Close a trade. When `restore` is set, add that slice back to the order.
    pub fn finish_trade(
        &self,
        order_id: &str,
        trade_id: &str,
        state: &str,
        restore: Option<u128>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let tx = conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE trades SET state = ?1 WHERE id = ?2",
            params![state, trade_id],
        )?;
        if updated != 1 {
            bail!("trade {trade_id} not found");
        }
        if let Some(shannons) = restore {
            add_available(&tx, order_id, shannons)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark an order canceled only while it has no open trade, and clear the book.
    pub fn cancel_order(&self, id: &str) -> Result<()> {
        let states = watched_states();
        let marks = vec!["?"; states.len()].join(",");
        let sql = format!(
            "UPDATE orders SET status = ?, available_shannons = 0
             WHERE id = ? AND status = ?
             AND NOT EXISTS (
                 SELECT 1 FROM trades
                 WHERE trades.order_id = orders.id AND trades.state IN ({marks})
             )"
        );
        let canceled = OrderStatus::Canceled.as_str();
        let open = OrderStatus::Open.as_str();
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&canceled, &id, &open];
        for state in &states {
            values.push(&*state);
        }
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(&sql, rusqlite::params_from_iter(values.iter().copied()))?;
        if updated == 1 {
            return Ok(());
        }
        if open_trades_for(&conn, id)? > 0 {
            bail!("order has an open trade");
        }
        bail!("order is not open");
    }

    pub fn insert_trade(&self, trade: &Trade) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "INSERT INTO trades (
                id, order_id, seller_nostr, seller_fiber, buyer_nostr, buyer_fiber,
                fiat_amount, shannons, state, hold_payment_hash, hold_invoice,
                payout_invoice, payout_payment_hash, hold_received_at,
                payment_method_id, payment_kind, payment_label, payment_currency
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                trade.id,
                trade.order_id,
                trade.seller_nostr,
                trade.seller_fiber,
                trade.buyer_nostr,
                trade.buyer_fiber,
                trade.fiat_amount,
                i64_from_shannons(trade.shannons)?,
                trade.state,
                trade.hold_payment_hash,
                trade.hold_invoice,
                trade.payout_invoice,
                trade.payout_payment_hash,
                trade.hold_received_at,
                trade.payment_method_id,
                trade.payment_kind,
                trade.payment_label,
                trade.payment_currency,
            ],
        )?;
        Ok(())
    }

    pub fn get_trade(&self, id: &str) -> Result<Option<Trade>> {
        let conn = self.conn.lock().expect("db");
        let mut stmt =
            conn.prepare(&format!("SELECT {TRADE_COLUMNS} FROM trades WHERE id = ?1"))?;
        let trade = stmt.query_row(params![id], row_to_trade).optional()?;
        Ok(trade)
    }

    pub fn open_trade_for_order(&self, order_id: &str) -> Result<Option<Trade>> {
        Ok(self
            .trades_in_states(Phase::watched_phases())?
            .into_iter()
            .find(|trade| trade.order_id == order_id))
    }

    pub fn trades_in_states(&self, phases: &[Phase]) -> Result<Vec<Trade>> {
        let states: Vec<&str> = phases.iter().map(|phase| phase.as_str()).collect();
        let conn = self.conn.lock().expect("db");
        let marks = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT {TRADE_COLUMNS} FROM trades WHERE state IN ({marks})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(states.iter()), row_to_trade)?;
        let mut trades = Vec::new();
        for row in rows {
            trades.push(row?);
        }
        Ok(trades)
    }

    pub fn set_trade_state(&self, id: &str, state: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET state = ?1 WHERE id = ?2",
            params![state, id],
        )?;
        Ok(())
    }

    pub fn set_hold_received_at(&self, id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET hold_received_at = ?1 WHERE id = ?2",
            params![at, id],
        )?;
        Ok(())
    }

    pub fn set_hold_received(&self, id: &str, state: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET state = ?1, hold_received_at = ?2 WHERE id = ?3",
            params![state, at, id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn set_hold(&self, id: &str, payment_hash: &str, invoice: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET hold_payment_hash = ?1, hold_invoice = ?2 WHERE id = ?3",
            params![payment_hash, invoice, id],
        )?;
        Ok(())
    }

    pub fn set_payout(&self, id: &str, invoice: &str, payment_hash: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET payout_invoice = ?1, payout_payment_hash = ?2 WHERE id = ?3",
            params![invoice, payment_hash, id],
        )?;
        Ok(())
    }

    pub fn insert_preimage(&self, payment_hash: &str, preimage: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "INSERT INTO preimages (payment_hash, preimage) VALUES (?1, ?2)",
            params![payment_hash, preimage],
        )?;
        Ok(())
    }

    pub fn get_preimage(&self, payment_hash: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db");
        let value = conn
            .query_row(
                "SELECT preimage FROM preimages WHERE payment_hash = ?1",
                params![payment_hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn delete_preimage(&self, payment_hash: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "DELETE FROM preimages WHERE payment_hash = ?1",
            params![payment_hash],
        )?;
        Ok(())
    }

    pub fn fiber_pubkey(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db");
        let value = conn
            .query_row("SELECT pubkey FROM fiber_node WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    pub fn set_fiber_pubkey(&self, pubkey: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "INSERT INTO fiber_node (id, pubkey) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET pubkey = excluded.pubkey",
            params![pubkey],
        )?;
        Ok(())
    }

    pub fn delete_trade(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute("DELETE FROM trades WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn sync_supported(conn: &Connection) -> Result<()> {
    for method in SUPPORTED_PAYMENT_METHODS {
        if PaymentKind::parse(method.kind).is_none() {
            bail!(
                "payment method {} has unknown kind {}",
                method.id,
                method.kind
            );
        }
        if !currency_code(method.currency) {
            bail!(
                "payment method {} has unknown currency {}",
                method.id,
                method.currency
            );
        }
        conn.execute(
            "INSERT INTO supported_payment_methods (id, kind, label, currency) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, label = excluded.label, currency = excluded.currency",
            params![method.id, method.kind, method.label, method.currency],
        )?;
    }
    let ids: Vec<&str> = SUPPORTED_PAYMENT_METHODS
        .iter()
        .map(|method| method.id)
        .collect();
    if ids.is_empty() {
        conn.execute("DELETE FROM supported_payment_methods", [])?;
        return Ok(());
    }
    let marks = vec!["?"; ids.len()].join(",");
    let sql = format!("DELETE FROM supported_payment_methods WHERE id NOT IN ({marks})");
    conn.execute(&sql, rusqlite::params_from_iter(ids.iter()))
        .context("cannot remove a payment method that an order still uses")?;
    Ok(())
}

fn currency_code(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn load_supported(conn: &Connection) -> Result<Vec<SupportedPaymentMethod>> {
    let mut stmt = conn
        .prepare("SELECT id, kind, label, currency FROM supported_payment_methods ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(SupportedPaymentMethod {
            id: row.get(0)?,
            kind: row.get(1)?,
            label: row.get(2)?,
            currency: row.get(3)?,
        })
    })?;
    let mut methods = Vec::new();
    for row in rows {
        methods.push(row?);
    }
    Ok(methods)
}

fn insert_method(conn: &Connection, order_id: &str, method: &PaymentMethod) -> Result<()> {
    conn.execute(
        "INSERT INTO payment_methods (order_id, method_id) VALUES (?1, ?2)",
        params![order_id, method.id],
    )?;
    Ok(())
}

fn load_methods(conn: &Connection, order_id: &str) -> Result<Vec<PaymentMethod>> {
    let mut stmt = conn.prepare(
        "SELECT supported.id, supported.kind, supported.label, supported.currency
         FROM payment_methods payment
         JOIN supported_payment_methods supported ON supported.id = payment.method_id
         WHERE payment.order_id = ?1
         ORDER BY supported.id",
    )?;
    let rows = stmt.query_map(params![order_id], |row| {
        Ok(PaymentMethod {
            id: row.get(0)?,
            kind: row.get(1)?,
            label: row.get(2)?,
            currency: row.get(3)?,
        })
    })?;
    let mut methods = Vec::new();
    for row in rows {
        methods.push(row?);
    }
    Ok(methods)
}

fn watched_states() -> Vec<&'static str> {
    Phase::watched_phases()
        .iter()
        .map(|phase| phase.as_str())
        .collect()
}

fn open_trades_for(conn: &Connection, order_id: &str) -> Result<i64> {
    let states = watched_states();
    let marks = vec!["?"; states.len()].join(",");
    let sql = format!("SELECT COUNT(*) FROM trades WHERE order_id = ? AND state IN ({marks})");
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&order_id];
    for state in &states {
        values.push(&*state);
    }
    let count = conn.query_row(
        &sql,
        rusqlite::params_from_iter(values.iter().copied()),
        |row| row.get(0),
    )?;
    Ok(count)
}

fn add_available(conn: &Connection, order_id: &str, shannons: u128) -> Result<()> {
    let shannons = i64_from_shannons(shannons)?;
    let current: i64 = conn
        .query_row(
            "SELECT available_shannons FROM orders WHERE id = ?1",
            params![order_id],
            |row| row.get(0),
        )
        .with_context(|| format!("order {order_id} not found"))?;
    let restored = current
        .checked_add(shannons)
        .context("order available balance overflow")?;
    conn.execute(
        "UPDATE orders SET available_shannons = ?1 WHERE id = ?2",
        params![restored, order_id],
    )?;
    Ok(())
}

fn read_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn read_shannons(value: i64, column: usize) -> rusqlite::Result<u128> {
    u128::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn row_to_trade(row: &rusqlite::Row<'_>) -> rusqlite::Result<Trade> {
    Ok(Trade {
        id: row.get(0)?,
        order_id: row.get(1)?,
        seller_nostr: row.get(2)?,
        seller_fiber: row.get(3)?,
        buyer_nostr: row.get(4)?,
        buyer_fiber: row.get(5)?,
        fiat_amount: row.get(6)?,
        shannons: read_shannons(row.get(7)?, 7)?,
        state: row.get(8)?,
        hold_payment_hash: row.get(9)?,
        hold_invoice: row.get(10)?,
        payout_invoice: row.get(11)?,
        payout_payment_hash: row.get(12)?,
        hold_received_at: row.get(13)?,
        payment_method_id: row.get(14)?,
        payment_kind: row.get(15)?,
        payment_label: row.get(16)?,
        payment_currency: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_order(available: u128) -> Order {
        Order {
            id: "order".into(),
            side: "sell".into(),
            maker_nostr: "seller".into(),
            maker_fiber: "fiber".into(),
            available_shannons: available,
            fiat_currency: "NGN".into(),
            price_per_ckb: "1000".into(),
            min: "1000".into(),
            max: "10000".into(),
            payment_methods: vec![PaymentMethod {
                id: "gtbank".into(),
                kind: "bank".into(),
                label: "GTBank".into(),
                currency: "NGN".into(),
            }],
            status: OrderStatus::Open.as_str().into(),
            hold_secs: 36 * 3_600,
        }
    }

    fn sample_trade(shannons: u128) -> Trade {
        Trade {
            id: "trade".into(),
            order_id: "order".into(),
            seller_nostr: "seller".into(),
            seller_fiber: "fiber-seller".into(),
            buyer_nostr: "buyer".into(),
            buyer_fiber: "fiber-buyer".into(),
            fiat_amount: "1000".into(),
            shannons,
            state: Phase::WaitingHold.as_str().into(),
            hold_payment_hash: Some("0xhash".into()),
            hold_invoice: None,
            payout_invoice: None,
            payout_payment_hash: None,
            hold_received_at: None,
            payment_method_id: "gtbank".into(),
            payment_kind: "bank".into(),
            payment_label: "GTBank".into(),
            payment_currency: "NGN".into(),
        }
    }

    #[test]
    fn the_catalog_is_the_two_configured_methods() {
        let db = Db::open_in_memory().unwrap();
        let methods = db.supported_payment_methods().unwrap();
        assert_eq!(
            methods
                .iter()
                .map(|method| method.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gtbank", "zelle"]
        );
        assert_eq!(methods[0].kind, "bank");
        assert_eq!(methods[0].currency, "NGN");
        assert_eq!(methods[1].kind, "wallet");
        assert_eq!(methods[1].currency, "USD");
    }

    #[test]
    fn take_debits_once_and_rollback_restores_the_slice() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(200_000_000)).unwrap();
        let trade = sample_trade(100_000_000);
        db.commit_take(&trade, "preimage").unwrap();
        assert_eq!(
            db.get_order("order").unwrap().unwrap().available_shannons,
            100_000_000
        );
        assert!(db.get_preimage("0xhash").unwrap().is_some());
        assert!(db.commit_take(&trade, "preimage").is_err());
        assert_eq!(
            db.get_order("order").unwrap().unwrap().available_shannons,
            100_000_000
        );
        db.rollback_take("order", "trade", "0xhash", 100_000_000)
            .unwrap();
        assert_eq!(
            db.get_order("order").unwrap().unwrap().available_shannons,
            200_000_000
        );
        assert!(db.get_trade("trade").unwrap().is_none());
        assert!(db.get_preimage("0xhash").unwrap().is_none());
    }

    #[test]
    fn cancel_order_refuses_an_open_trade_and_clears_a_free_book() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(200_000_000)).unwrap();
        db.commit_take(&sample_trade(100_000_000), "preimage")
            .unwrap();
        assert!(db.cancel_order("order").is_err());
        db.finish_trade("order", "trade", Phase::Settled.as_str(), None)
            .unwrap();
        db.cancel_order("order").unwrap();
        let order = db.get_order("order").unwrap().unwrap();
        assert_eq!(order.status, OrderStatus::Canceled.as_str());
        assert_eq!(order.available_shannons, 0);
    }

    #[test]
    fn a_negative_balance_is_not_read_as_zero() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(1)).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE orders SET available_shannons = -1 WHERE id = 'order'",
                [],
            )
            .unwrap();
        assert!(db.get_order("order").is_err());
    }
}
