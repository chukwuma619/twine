use crate::{Order, Phase, Trade, i64_from_shannons, shannons_from_i64};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

const TRADE_COLUMNS: &str = "id, order_id, taker_nostr, taker_fiber, fiat_amount, shannons, state, \
     hold_payment_hash, hold_invoice, payout_invoice, payout_payment_hash, hold_received_at";

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
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS orders (
                id TEXT PRIMARY KEY,
                maker_nostr TEXT NOT NULL,
                maker_fiber TEXT NOT NULL,
                available_shannons INTEGER NOT NULL,
                fiat_currency TEXT NOT NULL,
                price_per_ckb TEXT NOT NULL,
                min TEXT NOT NULL,
                max TEXT NOT NULL,
                payment_method TEXT NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS trades (
                id TEXT PRIMARY KEY,
                order_id TEXT NOT NULL,
                taker_nostr TEXT NOT NULL,
                taker_fiber TEXT NOT NULL,
                fiat_amount TEXT NOT NULL,
                shannons INTEGER NOT NULL,
                state TEXT NOT NULL,
                hold_payment_hash TEXT,
                hold_invoice TEXT,
                payout_invoice TEXT,
                payout_payment_hash TEXT,
                hold_received_at INTEGER,
                FOREIGN KEY(order_id) REFERENCES orders(id)
            );
            CREATE TABLE IF NOT EXISTS preimages (
                payment_hash TEXT PRIMARY KEY,
                preimage TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS processed_events (
                event_id TEXT PRIMARY KEY
            );
            ",
        )?;
        // Databases created before hold_received_at existed.
        if let Err(error) =
            conn.execute("ALTER TABLE trades ADD COLUMN hold_received_at INTEGER", [])
        {
            if !error.to_string().contains("duplicate column") {
                return Err(error.into());
            }
        }
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
        conn.execute(
            "INSERT INTO orders (
                id, maker_nostr, maker_fiber, available_shannons, fiat_currency,
                price_per_ckb, min, max, payment_method, status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                order.id,
                order.maker_nostr,
                order.maker_fiber,
                i64_from_shannons(order.available_shannons)?,
                order.fiat_currency_code,
                order.price_per_ckb,
                order.min,
                order.max,
                order.payment_method,
                order.status,
            ],
        )?;
        Ok(())
    }

    pub fn get_order(&self, id: &str) -> Result<Option<Order>> {
        let conn = self.conn.lock().expect("db");
        let mut stmt = conn.prepare(
            "SELECT id, maker_nostr, maker_fiber, available_shannons, fiat_currency,
                    price_per_ckb, min, max, payment_method, status
             FROM orders WHERE id = ?1",
        )?;
        let order = stmt
            .query_row(params![id], |row| {
                Ok(Order {
                    id: row.get(0)?,
                    maker_nostr: row.get(1)?,
                    maker_fiber: row.get(2)?,
                    available_shannons: shannons_from_i64(row.get(3)?),
                    fiat_currency_code: row.get(4)?,
                    price_per_ckb: row.get(5)?,
                    min: row.get(6)?,
                    max: row.get(7)?,
                    payment_method: row.get(8)?,
                    status: row.get(9)?,
                })
            })
            .optional()?;
        Ok(order)
    }

    pub fn set_available(&self, id: &str, available: u128) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE orders SET available_shannons = ?1 WHERE id = ?2",
            params![i64_from_shannons(available)?, id],
        )?;
        Ok(())
    }

    pub fn set_order_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "UPDATE orders SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn insert_trade(&self, trade: &Trade) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute(
            "INSERT INTO trades (
                id, order_id, taker_nostr, taker_fiber, fiat_amount, shannons, state,
                hold_payment_hash, hold_invoice, payout_invoice, payout_payment_hash,
                hold_received_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                trade.id,
                trade.order_id,
                trade.taker_nostr,
                trade.taker_fiber,
                trade.fiat_amount,
                i64_from_shannons(trade.shannons)?,
                trade.state,
                trade.hold_payment_hash,
                trade.hold_invoice,
                trade.payout_invoice,
                trade.payout_payment_hash,
                trade.hold_received_at,
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

    pub fn delete_trade(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db");
        conn.execute("DELETE FROM trades WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn row_to_trade(row: &rusqlite::Row<'_>) -> rusqlite::Result<Trade> {
    Ok(Trade {
        id: row.get(0)?,
        order_id: row.get(1)?,
        taker_nostr: row.get(2)?,
        taker_fiber: row.get(3)?,
        fiat_amount: row.get(4)?,
        shannons: shannons_from_i64(row.get(5)?),
        state: row.get(6)?,
        hold_payment_hash: row.get(7)?,
        hold_invoice: row.get(8)?,
        payout_invoice: row.get(9)?,
        payout_payment_hash: row.get(10)?,
        hold_received_at: row.get(11)?,
    })
}
