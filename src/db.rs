use crate::{Order, OrderStatus, Phase, Trade, i64_from_shannons};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

const TRADE_COLUMNS: &str = "id, order_id, seller_nostr, seller_fiber, buyer_nostr, buyer_fiber, \
     fiat_amount, shannons, state, hold_payment_hash, hold_invoice, payout_invoice, \
     payout_payment_hash, hold_received_at, hold_created_at, buyer_fiber_addr, \
     hold_expires_at_ms, fiat_acknowledged, fiat_sent_at, chat_key_shared, release_prompted, \
     outcome";

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
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
                hold_created_at INTEGER,
                buyer_fiber_addr TEXT,
                hold_expires_at_ms INTEGER,
                fiat_acknowledged INTEGER NOT NULL DEFAULT 0,
                fiat_sent_at INTEGER,
                chat_key_shared INTEGER NOT NULL DEFAULT 0,
                release_prompted INTEGER NOT NULL DEFAULT 0,
                outcome TEXT,
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
                pubkey TEXT NOT NULL,
                addresses TEXT NOT NULL DEFAULT '[]'
            );
            ",
        )?;
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN hold_created_at INTEGER", []);
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN buyer_fiber_addr TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN hold_expires_at_ms INTEGER",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN fiat_acknowledged INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN fiat_sent_at INTEGER", []);
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN chat_key_shared INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN release_prompted INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN outcome TEXT", []);
        let _ = conn.execute("ALTER TABLE trades ADD COLUMN solver_evidence TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE trades ADD COLUMN evidence_pending INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("DROP TABLE IF EXISTS payment_methods", []);
        let _ = conn.execute("DROP TABLE IF EXISTS supported_payment_methods", []);
        for column in [
            "payment_method_id",
            "payment_kind",
            "payment_label",
            "payment_currency",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE trades DROP COLUMN {column}"), []);
        }
        let _ = conn.execute(
            "ALTER TABLE fiber_node ADD COLUMN addresses TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        Ok(())
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
                    status: row.get(9)?,
                    hold_secs: read_u64(row.get(10)?, 10)?,
                    reserved_shannons: 0,
                })
            })
            .optional()?;
        let Some(mut order) = order else {
            return Ok(None);
        };
        order.reserved_shannons = reserved_shannons_for(&conn, id)?;
        Ok(Some(order))
    }

    pub fn orders(&self) -> Result<Vec<Order>> {
        let ids = {
            let conn = self.conn.lock().expect("db");
            let mut stmt = conn.prepare("SELECT id FROM orders ORDER BY id")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        };
        ids.iter()
            .map(|id| {
                self.get_order(id)?
                    .with_context(|| format!("order {id} disappeared"))
            })
            .collect()
    }

    /// Debit the order and insert the trade. The hold hash may still be empty.
    /// The buyer's preimage is never stored here.
    pub fn commit_take(&self, trade: &Trade) -> Result<()> {
        let shannons = i64_from_shannons(trade.shannons)?;
        let conn = self.conn.lock().expect("db");
        let tx = conn.unchecked_transaction()?;
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
                payout_invoice, payout_payment_hash, hold_received_at, hold_created_at,
                buyer_fiber_addr, hold_expires_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                trade.hold_payment_hash,
                trade.hold_invoice,
                trade.payout_invoice,
                trade.payout_payment_hash,
                trade.hold_received_at,
                trade.hold_created_at,
                trade.buyer_fiber_addr,
                trade.hold_expires_at_ms,
            ],
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
        let states = open_states();
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
            values.push(state);
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
                payout_invoice, payout_payment_hash, hold_received_at, hold_created_at,
                buyer_fiber_addr, hold_expires_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                trade.hold_created_at,
                trade.buyer_fiber_addr,
                trade.hold_expires_at_ms,
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
            .trades_in_states(Phase::open_phases())?
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

    pub fn set_fiat_acknowledged(&self, id: &str, acknowledged: bool) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET fiat_acknowledged = ?1 WHERE id = ?2",
            params![i64::from(acknowledged), id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn mark_fiat_sent(&self, id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET state = ?1, fiat_sent_at = ?2, release_prompted = 0 WHERE id = ?3",
            params![Phase::FiatSent.as_str(), at, id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn set_fiat_sent_at(&self, id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET fiat_sent_at = ?1 WHERE id = ?2",
            params![at, id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn save_solver_evidence(&self, id: &str, evidence: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET solver_evidence = ?1, chat_key_shared = 1, evidence_pending = 1 WHERE id = ?2",
            params![evidence, id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn solver_evidence(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db");
        let value = conn
            .query_row(
                "SELECT solver_evidence FROM trades WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    pub fn pending_solver_evidence(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("db");
        let mut stmt = conn.prepare(
            "SELECT id, solver_evidence FROM trades
             WHERE evidence_pending = 1 AND solver_evidence IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn evidence_is_pending(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("db");
        let pending: Option<i64> = conn
            .query_row(
                "SELECT evidence_pending FROM trades WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(pending == Some(1))
    }

    pub fn clear_evidence_pending(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET evidence_pending = 0 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Stores the public outcome the first time. A later write does not replace it.
    pub fn set_outcome_once(&self, id: &str, outcome: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET outcome = ?1 WHERE id = ?2 AND outcome IS NULL",
            params![outcome, id],
        )?;
        if updated == 1 {
            return Ok(true);
        }
        let found: Option<Option<String>> = conn
            .query_row(
                "SELECT outcome FROM trades WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if found.is_none() {
            bail!("trade {id} not found");
        }
        Ok(false)
    }

    pub fn set_release_prompted(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET release_prompted = 1 WHERE id = ?1",
            params![id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    /// Seller release, or a solver decision that the fiat arrived and the invoice can be paid.
    pub fn begin_release(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET state = ?1, fiat_acknowledged = 1 WHERE id = ?2",
            params![Phase::Releasing.as_str(), id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
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

    pub fn set_hold_expires_at_ms(&self, id: &str, expires_at_ms: i64) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET hold_expires_at_ms = ?1 WHERE id = ?2",
            params![expires_at_ms, id],
        )?;
        Ok(())
    }

    pub fn set_hold_received(
        &self,
        id: &str,
        state: &str,
        at: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let updated = conn.execute(
            "UPDATE trades SET state = ?1, hold_received_at = ?2, hold_expires_at_ms = ?3 WHERE id = ?4",
            params![state, at, expires_at_ms, id],
        )?;
        if updated != 1 {
            bail!("trade {id} not found");
        }
        Ok(())
    }

    pub fn set_hold(
        &self,
        id: &str,
        payment_hash: &str,
        invoice: &str,
        created_at: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE trades SET hold_payment_hash = ?1, hold_invoice = ?2, hold_created_at = ?3 WHERE id = ?4",
            params![payment_hash, invoice, created_at, id],
        )?;
        Ok(())
    }

    pub fn trades_for_party(&self, pubkey: &str) -> Result<Vec<Trade>> {
        let conn = self.conn.lock().expect("db");
        let mut stmt = conn.prepare(&format!(
            "SELECT {TRADE_COLUMNS} FROM trades
             WHERE lower(seller_nostr) = lower(?1) OR lower(buyer_nostr) = lower(?1)
             ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![pubkey], row_to_trade)?;
        let mut trades = Vec::new();
        for row in rows {
            trades.push(row?);
        }
        Ok(trades)
    }

    /// True when another trade already used this hold hash.
    /// A settled payment reveals the preimage, so the hash cannot be reused.
    pub fn hold_hash_used(&self, payment_hash: &str, except_trade: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("db");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM trades
             WHERE id != ?1 AND hold_payment_hash IS NOT NULL
             AND lower(hold_payment_hash) = lower(?2)",
            params![except_trade, payment_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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
            "INSERT OR REPLACE INTO preimages (payment_hash, preimage) VALUES (?1, ?2)",
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

    pub fn fiber_addresses(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("db");
        let raw: Option<String> = conn
            .query_row("SELECT addresses FROM fiber_node WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        decode_fiber_addresses(raw.as_deref())
    }

    /// An empty address list keeps the previous multiaddrs when the pubkey is unchanged.
    pub fn set_fiber_node(&self, pubkey: &str, addresses: &[String]) -> Result<()> {
        let same_node = self.fiber_pubkey()?.as_deref() == Some(pubkey);
        let kept = if addresses.is_empty() && same_node {
            self.fiber_addresses()?
        } else {
            addresses.to_vec()
        };
        let encoded = serde_json::to_string(&kept).context("fiber node multiaddrs")?;
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "INSERT INTO fiber_node (id, pubkey, addresses) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET pubkey = excluded.pubkey, addresses = excluded.addresses",
            params![pubkey, encoded],
        )?;
        Ok(())
    }

    /// Updates multiaddrs only when `pubkey` is the pinned node.
    /// An empty list keeps the addresses already stored.
    pub fn update_pinned_addresses(&self, pubkey: &str, addresses: &[String]) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        let Some((saved, _)) = fiber_node_row(&conn)? else {
            return Ok(());
        };
        if saved != pubkey || addresses.is_empty() {
            return Ok(());
        }
        write_fiber_node(&conn, &saved, addresses)
    }

    pub fn delete_trade(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute("DELETE FROM trades WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn fiber_node_row(conn: &Connection) -> Result<Option<(String, Vec<String>)>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT pubkey, addresses FROM fiber_node WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        Some((pubkey, raw)) => Ok(Some((pubkey, decode_fiber_addresses(Some(raw.as_str()))?))),
        None => Ok(None),
    }
}

fn write_fiber_node(conn: &Connection, pubkey: &str, addresses: &[String]) -> Result<()> {
    let encoded = serde_json::to_string(addresses).context("fiber node multiaddrs")?;
    conn.execute(
        "INSERT INTO fiber_node (id, pubkey, addresses) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET pubkey = excluded.pubkey, addresses = excluded.addresses",
        params![pubkey, encoded],
    )?;
    Ok(())
}

fn decode_fiber_addresses(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(raw).context("fiber node multiaddrs")
}

fn open_states() -> Vec<&'static str> {
    Phase::open_phases()
        .iter()
        .map(|phase| phase.as_str())
        .collect()
}

fn reserved_shannons_for(conn: &Connection, order_id: &str) -> Result<u128> {
    let states = open_states();
    let marks = vec!["?"; states.len()].join(",");
    let sql = format!(
        "SELECT COALESCE(SUM(shannons), 0) FROM trades WHERE order_id = ? AND state IN ({marks})"
    );
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&order_id];
    for state in &states {
        values.push(state);
    }
    let total: i64 = conn.query_row(
        &sql,
        rusqlite::params_from_iter(values.iter().copied()),
        |row| row.get(0),
    )?;
    Ok(read_shannons(total, 0)?)
}

fn open_trades_for(conn: &Connection, order_id: &str) -> Result<i64> {
    let states = open_states();
    let marks = vec!["?"; states.len()].join(",");
    let sql = format!("SELECT COUNT(*) FROM trades WHERE order_id = ? AND state IN ({marks})");
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&order_id];
    for state in &states {
        values.push(state);
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
        hold_created_at: row.get(14)?,
        buyer_fiber_addr: row.get(15)?,
        hold_expires_at_ms: row.get(16)?,
        fiat_acknowledged: row.get::<_, i64>(17)? != 0,
        fiat_sent_at: row.get(18)?,
        chat_key_shared: row.get::<_, i64>(19)? != 0,
        release_prompted: row.get::<_, i64>(20)? != 0,
        outcome: row.get(21)?,
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
            status: OrderStatus::Open.as_str().into(),
            hold_secs: 36 * 3_600,
            reserved_shannons: 0,
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
            hold_created_at: None,
            buyer_fiber_addr: None,
            hold_expires_at_ms: None,
            fiat_acknowledged: false,
            fiat_sent_at: None,
            chat_key_shared: false,
            release_prompted: false,
            outcome: None,
        }
    }

    #[test]
    fn fiber_node_keeps_multiaddrs_with_the_pubkey() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.fiber_pubkey().unwrap().is_none());
        assert!(db.fiber_addresses().unwrap().is_empty());

        let multiaddr = "/ip4/203.0.113.10/tcp/8230/p2p/Qmabc".to_string();
        db.set_fiber_node("02abc", std::slice::from_ref(&multiaddr))
            .unwrap();
        assert_eq!(db.fiber_pubkey().unwrap().as_deref(), Some("02abc"));
        assert_eq!(db.fiber_addresses().unwrap(), vec![multiaddr]);

        db.set_fiber_node("02abc", &[]).unwrap();
        assert_eq!(db.fiber_addresses().unwrap().len(), 1);

        db.set_fiber_node("02def", &[]).unwrap();
        assert!(db.fiber_addresses().unwrap().is_empty());
    }

    #[test]
    fn pinned_address_refresh_cannot_replace_the_pubkey() {
        let db = Db::open_in_memory().unwrap();
        let kept = "/ip4/203.0.113.10/tcp/8230/p2p/Qmabc".to_string();
        db.set_fiber_node("02abc", std::slice::from_ref(&kept))
            .unwrap();

        db.update_pinned_addresses("02def", &["/ip4/198.51.100.2/tcp/8230".into()])
            .unwrap();
        assert_eq!(db.fiber_pubkey().unwrap().as_deref(), Some("02abc"));
        assert_eq!(db.fiber_addresses().unwrap(), vec![kept]);

        db.update_pinned_addresses("02abc", &[]).unwrap();
        assert_eq!(db.fiber_addresses().unwrap().len(), 1);

        let refreshed = "/ip4/198.51.100.8/tcp/8230/p2p/Qmdef".to_string();
        db.update_pinned_addresses("02abc", std::slice::from_ref(&refreshed))
            .unwrap();
        assert_eq!(db.fiber_pubkey().unwrap().as_deref(), Some("02abc"));
        assert_eq!(db.fiber_addresses().unwrap(), vec![refreshed]);
    }

    #[test]
    fn orders_lists_what_was_inserted() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.orders().unwrap().is_empty());
        db.insert_order(&sample_order(100_000_000)).unwrap();
        let orders = db.orders().unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].id, "order");
        assert_eq!(orders[0].fiat_currency, "NGN");
    }

    #[test]
    fn take_debits_once_and_rollback_restores_the_slice() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(200_000_000)).unwrap();
        let trade = sample_trade(100_000_000);
        db.commit_take(&trade).unwrap();
        assert_eq!(
            db.get_order("order").unwrap().unwrap().available_shannons,
            100_000_000
        );
        assert!(db.get_preimage("0xhash").unwrap().is_none());
        assert!(db.commit_take(&trade).is_err());
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
    fn waiting_invoice_reserves_its_slice_and_leaves_the_rest_open() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(200_000_000)).unwrap();
        let mut trade = sample_trade(100_000_000);
        trade.state = Phase::WaitingInvoice.as_str().into();
        trade.hold_payment_hash = None;
        db.commit_take(&trade).unwrap();
        let order = db.get_order("order").unwrap().unwrap();
        assert_eq!(order.available_shannons, 100_000_000);
        assert_eq!(order.reserved_shannons, 100_000_000);
        assert!(db.open_trade_for_order("order").unwrap().is_some());
        assert!(db.cancel_order("order").is_err());
        trade.id = "trade-2".into();
        db.commit_take(&trade).unwrap();
        let order = db.get_order("order").unwrap().unwrap();
        assert_eq!(order.available_shannons, 0);
        assert_eq!(order.reserved_shannons, 200_000_000);
        trade.id = "trade-3".into();
        let rejected = db.commit_take(&trade).unwrap_err();
        assert!(rejected.to_string().contains("cannot fund"));
    }

    #[test]
    fn cancel_order_refuses_an_open_trade_and_clears_a_free_book() {
        let db = Db::open_in_memory().unwrap();
        db.insert_order(&sample_order(200_000_000)).unwrap();
        db.commit_take(&sample_trade(100_000_000)).unwrap();
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
