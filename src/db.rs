//! SQLite storage: schema, migrations, and every query the CLI needs.
//!
//! Concurrency: WAL mode plus a generous busy timeout; every write that must be
//! atomic with a read (dedup, baselining, bulk acknowledgment) runs inside a
//! `BEGIN IMMEDIATE` transaction, so concurrent processes serialize on writes.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};
use serde::Serialize;

use crate::error::not_found;
use crate::feed::{self, ParsedEntry, ParsedFeed};
use crate::time;

const SCHEMA_V1: &str = r#"
CREATE TABLE feeds (
    id                   INTEGER PRIMARY KEY,
    url                  TEXT NOT NULL UNIQUE,
    title                TEXT,
    site_url             TEXT,
    subscribed           INTEGER NOT NULL DEFAULT 1,
    baseline_pending     INTEGER NOT NULL DEFAULT 0,
    added_at             TEXT NOT NULL,
    removed_at           TEXT,
    etag                 TEXT,
    last_modified        TEXT,
    last_attempt_at      TEXT,
    last_success_at      TEXT,
    last_status          TEXT,
    last_error           TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE syncs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at      TEXT NOT NULL,
    finished_at     TEXT,
    status          TEXT NOT NULL,
    feeds_checked   INTEGER NOT NULL DEFAULT 0,
    feeds_failed    INTEGER NOT NULL DEFAULT 0,
    new_count       INTEGER NOT NULL DEFAULT 0,
    baselined_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_id       INTEGER NOT NULL REFERENCES feeds(id),
    identity      TEXT NOT NULL,
    guid          TEXT,
    url           TEXT,
    title         TEXT,
    author        TEXT,
    summary       TEXT,
    summary_type  TEXT,
    content       TEXT,
    content_type  TEXT,
    published_at  TEXT,
    updated_at    TEXT,
    first_seen_at TEXT NOT NULL,
    effective_at  TEXT NOT NULL,
    date_basis    TEXT NOT NULL,
    sync_id       INTEGER NOT NULL REFERENCES syncs(id),
    baselined     INTEGER NOT NULL DEFAULT 0,
    seen_at       TEXT,
    starred_at    TEXT,
    UNIQUE (feed_id, identity)
);
CREATE INDEX items_effective ON items (effective_at DESC, id DESC);
CREATE INDEX items_unseen ON items (effective_at) WHERE seen_at IS NULL;
CREATE INDEX items_starred ON items (starred_at) WHERE starred_at IS NOT NULL;
CREATE INDEX items_sync ON items (sync_id);
"#;

/// Compact, body-free item record.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub feed_id: i64,
    pub source: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub published_at: Option<String>,
    pub effective_at: String,
    pub date_basis: String,
    pub first_seen_at: String,
    pub sync_id: i64,
    pub seen_at: Option<String>,
    pub starred_at: Option<String>,
}

/// Full locally stored record: compact fields plus raw and plain-text bodies.
#[derive(Debug, Clone, Serialize)]
pub struct FullItem {
    #[serde(flatten)]
    pub item: Item,
    pub feed_url: String,
    pub guid: Option<String>,
    pub author: Option<String>,
    pub updated_at: Option<String>,
    pub summary: Option<String>,
    pub summary_text: Option<String>,
    pub content: Option<String>,
    pub content_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub subscribed: bool,
    pub baseline_pending: bool,
    pub added_at: String,
    pub removed_at: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: i64,
    pub items: i64,
    pub unseen: i64,
}

/// (feed ID, URL, ETag, Last-Modified) for one subscribed feed.
pub type FetchTarget = (i64, String, Option<String>, Option<String>);

#[derive(Debug, Clone, Serialize)]
pub struct SyncRecord {
    pub sync_id: i64,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub feeds_checked: i64,
    pub feeds_failed: i64,
    pub new: i64,
    pub baselined: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatedItem {
    pub id: i64,
    pub effective_at: String,
    pub date_basis: String,
}

/// Selection over items. Every set field narrows the result (logical AND).
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub unseen: bool,
    pub seen: bool,
    pub starred: bool,
    pub sync_id: Option<i64>,
    pub feed_id: Option<i64>,
    /// `effective_at < before`
    pub before: Option<DateTime<Utc>>,
    /// `effective_at >= after`
    pub after: Option<DateTime<Utc>>,
    pub exclude_baselined: bool,
    /// Only entries whose date came from the publisher (`date_basis` != `discovered`).
    pub dated: bool,
}

impl Filter {
    fn sql(&self) -> (String, Vec<Value>) {
        let mut clauses = vec!["1=1".to_string()];
        let mut args: Vec<Value> = Vec::new();
        if self.unseen {
            clauses.push("i.seen_at IS NULL".into());
        }
        if self.seen {
            clauses.push("i.seen_at IS NOT NULL".into());
        }
        if self.starred {
            clauses.push("i.starred_at IS NOT NULL".into());
        }
        if self.exclude_baselined {
            clauses.push("i.baselined = 0".into());
        }
        if self.dated {
            clauses.push("i.date_basis != 'discovered'".into());
        }
        if let Some(id) = self.sync_id {
            clauses.push("i.sync_id = ?".into());
            args.push(id.into());
        }
        if let Some(id) = self.feed_id {
            clauses.push("i.feed_id = ?".into());
            args.push(id.into());
        }
        if let Some(t) = self.before {
            clauses.push("i.effective_at < ?".into());
            args.push(time::fmt(t).into());
        }
        if let Some(t) = self.after {
            clauses.push("i.effective_at >= ?".into());
            args.push(time::fmt(t).into());
        }
        (clauses.join(" AND "), args)
    }
}

const ITEM_COLUMNS: &str = "i.id, i.feed_id, COALESCE(f.title, f.url), i.title, i.url, i.published_at, \
     i.effective_at, i.date_basis, i.first_seen_at, i.sync_id, i.seen_at, i.starred_at, \
     f.url, i.guid, i.author, i.updated_at, i.summary, i.summary_type, i.content, i.content_type";
const ORDER: &str = "ORDER BY i.effective_at DESC, i.id DESC";

fn full_item(r: &Row) -> rusqlite::Result<FullItem> {
    let summary: Option<String> = r.get(16)?;
    let summary_type: Option<String> = r.get(17)?;
    let content: Option<String> = r.get(18)?;
    let content_type: Option<String> = r.get(19)?;
    Ok(FullItem {
        item: Item {
            id: r.get(0)?,
            feed_id: r.get(1)?,
            source: r.get(2)?,
            title: r.get(3)?,
            url: r.get(4)?,
            published_at: r.get(5)?,
            effective_at: r.get(6)?,
            date_basis: r.get(7)?,
            first_seen_at: r.get(8)?,
            sync_id: r.get(9)?,
            seen_at: r.get(10)?,
            starred_at: r.get(11)?,
        },
        feed_url: r.get(12)?,
        guid: r.get(13)?,
        author: r.get(14)?,
        updated_at: r.get(15)?,
        summary_text: feed::summary_text(
            summary.as_deref(),
            summary_type.as_deref(),
            content.as_deref(),
            content_type.as_deref(),
        ),
        content_text: feed::render_opt(content.as_deref(), content_type.as_deref()),
        summary,
        content,
    })
}

pub struct Db {
    conn: Connection,
}

/// Outcome of `add_feed`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddStatus {
    Added,
    Exists,
    Resubscribed,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating database directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", 1)?;
        } else if version > 1 {
            anyhow::bail!("database schema version {version} is newer than this rss supports (1)");
        }
        tx.commit()?;
        Ok(())
    }

    // ---- feeds -------------------------------------------------------------

    pub fn add_feed(
        &mut self,
        url: &str,
        title: Option<&str>,
        baseline: bool,
    ) -> Result<(i64, AddStatus)> {
        let now = time::fmt(time::now());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, bool)> = tx
            .query_row(
                "SELECT id, subscribed FROM feeds WHERE url = ?",
                [url],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let result = match existing {
            Some((id, true)) => (id, AddStatus::Exists),
            Some((id, false)) => {
                tx.execute(
                    "UPDATE feeds SET subscribed = 1, removed_at = NULL, baseline_pending = ?, \
                     title = COALESCE(title, ?) WHERE id = ?",
                    params![baseline, title, id],
                )?;
                (id, AddStatus::Resubscribed)
            }
            None => {
                tx.execute(
                    "INSERT INTO feeds (url, title, baseline_pending, added_at) VALUES (?, ?, ?, ?)",
                    params![url, title, baseline, now],
                )?;
                (tx.last_insert_rowid(), AddStatus::Added)
            }
        };
        tx.commit()?;
        Ok(result)
    }

    /// Resolve a feed by numeric ID or exact URL (subscribed or not).
    pub fn resolve_feed(&self, spec: &str) -> Result<i64> {
        let found: Option<i64> = if let Ok(id) = spec.parse::<i64>() {
            self.conn
                .query_row("SELECT id FROM feeds WHERE id = ?", [id], |r| r.get(0))
                .optional()?
        } else {
            self.conn
                .query_row("SELECT id FROM feeds WHERE url = ?", [spec], |r| r.get(0))
                .optional()?
        };
        found.ok_or_else(|| not_found(format!("no such feed: {spec}")))
    }

    /// Unsubscribe. Returns whether anything changed. Articles and their state are kept.
    pub fn remove_feed(&mut self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE feeds SET subscribed = 0, removed_at = ?, baseline_pending = 0 \
             WHERE id = ? AND subscribed = 1",
            params![time::fmt(time::now()), id],
        )?;
        Ok(n > 0)
    }

    pub fn feeds(&self, include_unsubscribed: bool) -> Result<Vec<Feed>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.url, f.title, f.site_url, f.subscribed, f.baseline_pending, f.added_at, \
                    f.removed_at, f.last_attempt_at, f.last_success_at, f.last_status, f.last_error, \
                    f.consecutive_failures, \
                    (SELECT COUNT(*) FROM items i WHERE i.feed_id = f.id), \
                    (SELECT COUNT(*) FROM items i WHERE i.feed_id = f.id AND i.seen_at IS NULL) \
             FROM feeds f WHERE f.subscribed = 1 OR ? ORDER BY f.id",
        )?;
        let rows = stmt.query_map([include_unsubscribed], |r| {
            Ok(Feed {
                id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
                site_url: r.get(3)?,
                subscribed: r.get(4)?,
                baseline_pending: r.get(5)?,
                added_at: r.get(6)?,
                removed_at: r.get(7)?,
                last_attempt_at: r.get(8)?,
                last_success_at: r.get(9)?,
                last_status: r.get(10)?,
                last_error: r.get(11)?,
                consecutive_failures: r.get(12)?,
                items: r.get(13)?,
                unseen: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What `sync` needs to fetch each subscribed feed.
    pub fn fetch_targets(&self) -> Result<Vec<FetchTarget>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, url, etag, last_modified FROM feeds WHERE subscribed = 1 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- sync --------------------------------------------------------------

    pub fn begin_sync(&mut self, started: DateTime<Utc>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO syncs (started_at, status) VALUES (?, 'running')",
            [time::fmt(started)],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_sync(&mut self, rec: &SyncRecord) -> Result<()> {
        self.conn.execute(
            "UPDATE syncs SET finished_at = ?, status = ?, feeds_checked = ?, feeds_failed = ?, \
             new_count = ?, baselined_count = ? WHERE id = ?",
            params![
                rec.finished_at,
                rec.status,
                rec.feeds_checked,
                rec.feeds_failed,
                rec.new,
                rec.baselined,
                rec.sync_id
            ],
        )?;
        Ok(())
    }

    pub fn last_sync(&self) -> Result<Option<SyncRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, status, started_at, finished_at, feeds_checked, feeds_failed, new_count, \
                 baselined_count FROM syncs WHERE status != 'running' ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(SyncRecord {
                        sync_id: r.get(0)?,
                        status: r.get(1)?,
                        started_at: r.get(2)?,
                        finished_at: r.get(3)?,
                        feeds_checked: r.get(4)?,
                        feeds_failed: r.get(5)?,
                        new: r.get(6)?,
                        baselined: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn record_failure(&mut self, feed_id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE feeds SET last_attempt_at = ?, last_status = 'error', last_error = ?, \
             consecutive_failures = consecutive_failures + 1 WHERE id = ?",
            params![time::fmt(time::now()), error, feed_id],
        )?;
        Ok(())
    }

    pub fn record_not_modified(&mut self, feed_id: i64) -> Result<()> {
        let now = time::fmt(time::now());
        self.conn.execute(
            "UPDATE feeds SET last_attempt_at = ?, last_success_at = ?, last_status = 'not_modified', \
             last_error = NULL, consecutive_failures = 0 WHERE id = ?",
            params![now, now, feed_id],
        )?;
        Ok(())
    }

    /// Persist a successfully parsed feed. Returns (new, baselined) counts.
    ///
    /// Runs in one IMMEDIATE transaction, so a concurrent sync cannot insert the
    /// same entry in between our lookup and insert: each entry is credited to
    /// exactly one sync, and baselining applies to exactly one fetch.
    pub fn store_feed(
        &mut self,
        feed_id: i64,
        sync_id: i64,
        parsed: &ParsedFeed,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<(i64, i64)> {
        let now = time::now();
        let now_s = time::fmt(now);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let baseline: bool = tx.query_row(
            "SELECT baseline_pending FROM feeds WHERE id = ?",
            [feed_id],
            |r| r.get(0),
        )?;
        let (mut new, mut baselined) = (0i64, 0i64);
        for e in &parsed.entries {
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, first_seen_at FROM items WHERE feed_id = ? AND identity = ?",
                    params![feed_id, e.identity],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match existing {
                Some((id, first_seen)) => {
                    let first_seen = time::parse_stored(&first_seen).unwrap_or(now);
                    update_entry(&tx, id, e, first_seen)?;
                }
                None => {
                    let (eff, basis) = time::effective(e.published, e.updated, now);
                    tx.execute(
                        "INSERT INTO items (feed_id, identity, guid, url, title, author, summary, \
                         summary_type, content, content_type, published_at, updated_at, first_seen_at, \
                         effective_at, date_basis, sync_id, baselined, seen_at) \
                         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                        params![
                            feed_id,
                            e.identity,
                            e.guid,
                            e.url,
                            e.title,
                            e.author,
                            e.summary,
                            e.summary_type,
                            e.content,
                            e.content_type,
                            e.published.map(time::fmt),
                            e.updated.map(time::fmt),
                            now_s,
                            time::fmt(eff),
                            basis,
                            sync_id,
                            baseline,
                            baseline.then_some(&now_s),
                        ],
                    )?;
                    if baseline {
                        baselined += 1;
                    } else {
                        new += 1;
                    }
                }
            }
        }
        tx.execute(
            "UPDATE feeds SET title = COALESCE(?, title), site_url = COALESCE(?, site_url), \
             etag = ?, last_modified = ?, last_attempt_at = ?, last_success_at = ?, \
             last_status = 'ok', last_error = NULL, consecutive_failures = 0, baseline_pending = 0 \
             WHERE id = ?",
            params![
                parsed.title,
                parsed.site_url,
                etag,
                last_modified,
                now_s,
                now_s,
                feed_id
            ],
        )?;
        tx.commit()?;
        Ok((new, baselined))
    }

    // ---- items -------------------------------------------------------------

    pub fn count(&self, filter: &Filter) -> Result<i64> {
        let (clause, args) = filter.sql();
        let sql = format!("SELECT COUNT(*) FROM items i WHERE {clause}");
        Ok(self
            .conn
            .query_row(&sql, params_from_iter(args), |r| r.get(0))?)
    }

    pub fn items(&self, filter: &Filter, limit: Option<u64>, offset: u64) -> Result<Vec<FullItem>> {
        let (clause, mut args) = filter.sql();
        let sql = format!(
            "SELECT {ITEM_COLUMNS} FROM items i JOIN feeds f ON f.id = i.feed_id \
             WHERE {clause} {ORDER} LIMIT ? OFFSET ?"
        );
        args.push(limit.map(|n| n as i64).unwrap_or(-1).into());
        args.push((offset as i64).into());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), full_item)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, ids: &[i64]) -> Result<Vec<FullItem>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM items i JOIN feeds f ON f.id = i.feed_id WHERE i.id = ?"
        ))?;
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            match stmt.query_row([id], full_item).optional()? {
                Some(item) => out.push(item),
                None => missing.push(id.to_string()),
            }
        }
        if !missing.is_empty() {
            return Err(not_found(format!("no such item: {}", missing.join(", "))));
        }
        Ok(out)
    }

    /// Oldest and newest unseen item by effective date.
    pub fn unseen_extremes(&self) -> Result<(Option<DatedItem>, Option<DatedItem>)> {
        let q = |order: &str| -> Result<Option<DatedItem>> {
            Ok(self
                .conn
                .query_row(
                    &format!(
                        "SELECT id, effective_at, date_basis FROM items WHERE seen_at IS NULL \
                         ORDER BY effective_at {order}, id {order} LIMIT 1"
                    ),
                    [],
                    |r| {
                        Ok(DatedItem {
                            id: r.get(0)?,
                            effective_at: r.get(1)?,
                            date_basis: r.get(2)?,
                        })
                    },
                )
                .optional()?)
        };
        Ok((q("ASC")?, q("DESC")?))
    }

    // ---- state mutations ---------------------------------------------------

    /// Set or clear `seen_at` / `starred_at` on exact IDs. All-or-nothing: an
    /// unknown ID fails the whole call. Returns (matched, changed).
    pub fn mark_ids(&mut self, ids: &[i64], field: StateField, on: bool) -> Result<(i64, i64)> {
        let col = field.column();
        let now = time::fmt(time::now());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut missing = Vec::new();
        for id in ids {
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM items WHERE id = ?)",
                [id],
                |r| r.get(0),
            )?;
            if !exists {
                missing.push(id.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(not_found(format!("no such item: {}", missing.join(", "))));
        }
        let mut changed = 0;
        for id in ids {
            changed += if on {
                tx.execute(
                    &format!("UPDATE items SET {col} = ? WHERE id = ? AND {col} IS NULL"),
                    params![now, id],
                )?
            } else {
                tx.execute(
                    &format!("UPDATE items SET {col} = NULL WHERE id = ? AND {col} IS NOT NULL"),
                    [id],
                )?
            };
        }
        tx.commit()?;
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        Ok((unique.len() as i64, changed as i64))
    }

    /// Mark every item matching `filter` seen. Returns (matched, changed).
    pub fn mark_seen_where(&mut self, filter: &Filter) -> Result<(i64, i64)> {
        let now = time::fmt(time::now());
        let (clause, args) = filter.sql();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let matched: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM items i WHERE {clause}"),
            params_from_iter(args.clone()),
            |r| r.get(0),
        )?;
        let mut update_args = vec![Value::from(now)];
        update_args.extend(args);
        let changed = tx.execute(
            &format!("UPDATE items AS i SET seen_at = ? WHERE {clause} AND i.seen_at IS NULL"),
            params_from_iter(update_args),
        )?;
        tx.commit()?;
        Ok((matched, changed as i64))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StateField {
    Seen,
    Starred,
}

impl StateField {
    fn column(self) -> &'static str {
        match self {
            StateField::Seen => "seen_at",
            StateField::Starred => "starred_at",
        }
    }
}

/// Refresh feed-supplied fields of an existing entry. Local state (ID, first_seen_at,
/// sync_id, seen/starred) is untouched; missing new values keep previous good data.
fn update_entry(
    tx: &rusqlite::Transaction,
    id: i64,
    e: &ParsedEntry,
    first_seen: DateTime<Utc>,
) -> Result<()> {
    let (old_pub, old_upd): (Option<String>, Option<String>) = tx.query_row(
        "SELECT published_at, updated_at FROM items WHERE id = ?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let published = e
        .published
        .or_else(|| old_pub.as_deref().and_then(time::parse_stored));
    let updated = e
        .updated
        .or_else(|| old_upd.as_deref().and_then(time::parse_stored));
    let (eff, basis) = time::effective(published, updated, first_seen);
    tx.execute(
        "UPDATE items SET guid = COALESCE(?, guid), url = COALESCE(?, url), title = COALESCE(?, title), \
         author = COALESCE(?, author), summary = COALESCE(?, summary), \
         summary_type = CASE WHEN ? IS NULL THEN summary_type ELSE ? END, \
         content = COALESCE(?, content), \
         content_type = CASE WHEN ? IS NULL THEN content_type ELSE ? END, \
         published_at = ?, updated_at = ?, effective_at = ?, date_basis = ? WHERE id = ?",
        params![
            e.guid,
            e.url,
            e.title,
            e.author,
            e.summary,
            e.summary,
            e.summary_type,
            e.content,
            e.content,
            e.content_type,
            published.map(time::fmt),
            updated.map(time::fmt),
            time::fmt(eff),
            basis,
            id
        ],
    )?;
    Ok(())
}
