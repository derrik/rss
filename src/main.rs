//! `rss` — a tiny, agent-friendly RSS/Atom reader. See README.md for the contract.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};

use rss::db::{self, AddStatus, Db, Filter, FullItem, StateField, SyncRecord};
use rss::error::{self, CliError, EXIT_ERROR, EXIT_FAILED, EXIT_PARTIAL};
use rss::feed::{self, ParsedFeed};
use rss::fetch::{self, Fetched};
use rss::{opml, time};

const SCHEMA_VERSION: u32 = 1;

#[derive(Parser)]
#[command(
    name = "rss",
    version,
    about = "A tiny, agent-friendly RSS/Atom reader",
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 sync partially failed, 4 sync failed entirely."
)]
struct Cli {
    /// Database path [default: $XDG_DATA_HOME/rss/rss.db]
    #[arg(long, env = "RSS_DB", global = true, value_name = "PATH")]
    db: Option<PathBuf>,
    /// Output one JSON object: schema_version, counts, metadata, items
    #[arg(long, global = true)]
    json: bool,
    /// Output JSON Lines: one record per line, nothing else
    #[arg(long, global = true)]
    jsonl: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Subscribe to feed URLs (`-` reads URLs from stdin, one per line)
    Add {
        #[arg(required = true, value_name = "URL")]
        urls: Vec<String>,
        /// Mark everything found by the feed's first successful fetch as seen
        #[arg(long)]
        baseline: bool,
    },
    /// List subscriptions
    Feeds {
        /// Include unsubscribed feeds
        #[arg(long)]
        all: bool,
    },
    /// Unsubscribe (by feed ID or URL); stored articles and their state are kept
    Remove {
        #[arg(required = true, value_name = "FEED")]
        feeds: Vec<String>,
    },
    /// Fetch all feeds, store every new entry, emit a bounded selection of them
    Sync {
        /// How many new entries to emit: a number or `all`
        #[arg(long, default_value = "5", value_name = "N|all", value_parser = parse_limit)]
        emit: Limit,
        /// Emit counts only (same as --emit 0)
        #[arg(long, conflicts_with = "emit")]
        no_emit: bool,
        /// Per-feed request timeout in seconds
        #[arg(long, default_value_t = 20, value_name = "SECS")]
        timeout: u64,
        /// Feeds fetched in parallel
        #[arg(long, default_value_t = 8, value_name = "N")]
        jobs: usize,
    },
    /// List stored entries, newest first. Never changes state.
    Items {
        /// Only entries not yet acknowledged
        #[arg(long, conflicts_with = "seen")]
        unseen: bool,
        /// Only acknowledged entries
        #[arg(long)]
        seen: bool,
        /// Only starred entries
        #[arg(long)]
        starred: bool,
        #[command(flatten)]
        filter: FilterArgs,
        /// Maximum entries to list: a number or `all`
        #[arg(long, default_value = "20", value_name = "N|all", value_parser = parse_limit)]
        limit: Limit,
        /// Skip this many matching entries
        #[arg(long, default_value_t = 0, value_name = "N")]
        offset: u64,
        /// Include bodies (same record as `get`)
        #[arg(long)]
        full: bool,
        /// Print bare item IDs, one per line
        #[arg(long)]
        ids: bool,
    },
    /// Show full stored records (`-` reads IDs from stdin)
    Get {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Counts, unseen date range, and the outcome of the last sync
    Status {
        /// Also count unseen entries older than this age (e.g. 3d)
        #[arg(long, value_name = "AGE", conflicts_with = "before")]
        older_than: Option<String>,
        /// Also count unseen entries before this time (RFC 3339 or YYYY-MM-DD)
        #[arg(long, value_name = "TIME")]
        before: Option<String>,
    },
    /// Acknowledge entries as presented: by ID (`-` for stdin), by filter, or --all
    Seen {
        #[arg(value_name = "ID")]
        ids: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Acknowledge every entry matching the filters (or every entry, if none)
        #[arg(long)]
        all: bool,
    },
    /// Clear acknowledgment on entries (`-` reads IDs from stdin)
    Unseen {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Star entries (`-` reads IDs from stdin)
    Star {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Unstar entries (`-` reads IDs from stdin)
    Unstar {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Subscribe to every feed in an OPML file (`-` for stdin)
    Import {
        #[arg(value_name = "FILE")]
        file: String,
        /// Baseline each newly added feed (see `add --baseline`)
        #[arg(long)]
        baseline: bool,
    },
    /// Write subscriptions as OPML to stdout
    Export,
    /// Parse a feed document from a file or stdin without touching the database
    Parse {
        #[arg(value_name = "FILE", default_value = "-")]
        file: String,
    },
}

#[derive(Args, Default)]
struct FilterArgs {
    /// Only entries first discovered by this sync
    #[arg(long, value_name = "SYNC_ID")]
    sync: Option<i64>,
    /// Only entries from this feed (ID or URL)
    #[arg(long, value_name = "FEED")]
    feed: Option<String>,
    /// Effective date older than this age (e.g. 12h, 3d, 1w)
    #[arg(long, value_name = "AGE", conflicts_with = "before")]
    older_than: Option<String>,
    /// Effective date strictly before this time (RFC 3339 or YYYY-MM-DD)
    #[arg(long, value_name = "TIME")]
    before: Option<String>,
    /// Effective date within this age (e.g. 24h)
    #[arg(long, value_name = "AGE", conflicts_with = "after")]
    newer_than: Option<String>,
    /// Effective date at or after this time
    #[arg(long, value_name = "TIME")]
    after: Option<String>,
}

impl FilterArgs {
    fn is_empty(&self) -> bool {
        self.sync.is_none()
            && self.feed.is_none()
            && self.older_than.is_none()
            && self.before.is_none()
            && self.newer_than.is_none()
            && self.after.is_none()
    }

    fn resolve(&self, db: &Db, now: DateTime<Utc>) -> Result<Filter> {
        Ok(Filter {
            sync_id: self.sync,
            feed_id: self
                .feed
                .as_deref()
                .map(|f| db.resolve_feed(f))
                .transpose()?,
            before: cutoff(self.older_than.as_deref(), self.before.as_deref(), now)?,
            after: cutoff(self.newer_than.as_deref(), self.after.as_deref(), now)?,
            ..Filter::default()
        })
    }
}

/// Resolve a relative age or absolute time into an absolute cutoff.
fn cutoff(
    age: Option<&str>,
    at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    if let Some(a) = age {
        let d = time::parse_duration(a).map_err(|e| error::usage(e.to_string()))?;
        return Ok(Some(now - d));
    }
    at.map(|t| time::parse_timestamp(t).map_err(|e| error::usage(e.to_string())))
        .transpose()
}

#[derive(Clone, Copy)]
struct Limit(Option<u64>);

fn parse_limit(s: &str) -> Result<Limit, String> {
    if s == "all" {
        return Ok(Limit(None));
    }
    s.parse()
        .map(|n| Limit(Some(n)))
        .map_err(|_| format!("expected a number or `all`, got {s:?}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Human,
    Json,
    Jsonl,
    Ids,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            if is_broken_pipe(&e) {
                return ExitCode::SUCCESS;
            }
            let (exit, code) = e
                .downcast_ref::<CliError>()
                .map(|c| (c.exit, c.code))
                .unwrap_or((EXIT_ERROR, "error"));
            let message = format!("{e:#}");
            eprintln!("rss: {message}");
            if json {
                let _ = emit_json(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "error": { "code": code, "message": message },
                }));
            }
            ExitCode::from(exit as u8)
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn default_db_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rss").join("rss.db")
}

fn run(cli: Cli) -> Result<i32> {
    if cli.json && cli.jsonl {
        return Err(error::usage("--json and --jsonl are mutually exclusive"));
    }
    let mode = if cli.json {
        Mode::Json
    } else if cli.jsonl {
        Mode::Jsonl
    } else {
        Mode::Human
    };
    let db_path = cli.db.clone().unwrap_or_else(default_db_path);
    let open = || Db::open(&db_path);

    match cli.cmd {
        Cmd::Parse { file } => cmd_parse(mode, &file).map(|_| 0),
        Cmd::Add { urls, baseline } => cmd_add(&mut open()?, mode, &urls, baseline).map(|_| 0),
        Cmd::Feeds { all } => cmd_feeds(&open()?, mode, all).map(|_| 0),
        Cmd::Remove { feeds } => cmd_remove(&mut open()?, mode, &feeds).map(|_| 0),
        Cmd::Sync {
            emit,
            no_emit,
            timeout,
            jobs,
        } => {
            let emit = if no_emit { Some(0) } else { emit.0 };
            cmd_sync(
                &mut open()?,
                mode,
                emit,
                Duration::from_secs(timeout.max(1)),
                jobs.max(1),
            )
        }
        Cmd::Items {
            unseen,
            seen,
            starred,
            filter,
            limit,
            offset,
            full,
            ids,
        } => {
            let mode = if ids {
                if mode != Mode::Human {
                    return Err(error::usage(
                        "--ids cannot be combined with --json or --jsonl",
                    ));
                }
                Mode::Ids
            } else {
                mode
            };
            let db = open()?;
            let mut f = filter.resolve(&db, time::now())?;
            f.unseen = unseen;
            f.seen = seen;
            f.starred = starred;
            cmd_items(&db, mode, &f, limit.0, offset, full).map(|_| 0)
        }
        Cmd::Get { ids } => {
            let ids = read_ids(&ids)?;
            cmd_get(&open()?, mode, &ids).map(|_| 0)
        }
        Cmd::Status { older_than, before } => {
            let now = time::now();
            let stale = cutoff(older_than.as_deref(), before.as_deref(), now)?;
            cmd_status(&open()?, mode, now, stale).map(|_| 0)
        }
        Cmd::Seen { ids, filter, all } => {
            cmd_seen(&mut open()?, mode, &ids, &filter, all).map(|_| 0)
        }
        Cmd::Unseen { ids } => {
            cmd_mark(&mut open()?, mode, &ids, StateField::Seen, false).map(|_| 0)
        }
        Cmd::Star { ids } => {
            cmd_mark(&mut open()?, mode, &ids, StateField::Starred, true).map(|_| 0)
        }
        Cmd::Unstar { ids } => {
            cmd_mark(&mut open()?, mode, &ids, StateField::Starred, false).map(|_| 0)
        }
        Cmd::Import { file, baseline } => {
            cmd_import(&mut open()?, mode, &file, baseline).map(|_| 0)
        }
        Cmd::Export => cmd_export(&open()?).map(|_| 0),
    }
}

// ---- output helpers -------------------------------------------------------

fn emit_json(v: &impl Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, v)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn emit_jsonl<T: Serialize>(records: impl IntoIterator<Item = T>) -> Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    for r in records {
        serde_json::to_writer(&mut out, &r)?;
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

fn emit_lines(lines: impl IntoIterator<Item = String>) -> Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    for l in lines {
        writeln!(out, "{l}")?;
    }
    out.flush()?;
    Ok(())
}

fn human_item(i: &db::Item) -> String {
    let date = i
        .effective_at
        .get(..16)
        .unwrap_or(&i.effective_at)
        .replace('T', " ");
    let mut flags = String::new();
    if i.starred_at.is_some() {
        flags.push_str(" [starred]");
    }
    if i.seen_at.is_some() {
        flags.push_str(" [seen]");
    }
    format!(
        "{:>6}  {date}  {}: {}{flags}",
        i.id,
        i.source,
        i.title.as_deref().unwrap_or("(untitled)")
    )
}

fn compact(items: &[FullItem]) -> Vec<&db::Item> {
    items.iter().map(|i| &i.item).collect()
}

/// Item records for structured output: compact unless `full`.
fn records(items: &[FullItem], full: bool) -> Value {
    if full {
        json!(items)
    } else {
        json!(compact(items))
    }
}

// ---- stdin ID parsing -----------------------------------------------------

/// Expand ID arguments; `-` reads whitespace-separated IDs or JSONL records with an `id`.
fn read_ids(args: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for a in args {
        if a == "-" {
            let mut s = String::new();
            io::stdin()
                .read_to_string(&mut s)
                .context("reading IDs from stdin")?;
            ids.extend(parse_id_stream(&s)?);
        } else {
            ids.push(parse_id(a)?);
        }
    }
    Ok(ids)
}

fn parse_id(s: &str) -> Result<i64> {
    s.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| error::invalid(format!("invalid item ID {s:?}")))
}

fn parse_id_stream(s: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for line in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with('{') {
            let v: Value = serde_json::from_str(line)
                .map_err(|e| error::invalid(format!("invalid JSON record on stdin: {e}")))?;
            let id = v.get("id").and_then(Value::as_i64).filter(|n| *n > 0);
            ids.push(
                id.ok_or_else(|| error::invalid("JSON record on stdin has no numeric \"id\""))?,
            );
        } else {
            for tok in line.split_whitespace() {
                ids.push(parse_id(tok)?);
            }
        }
    }
    Ok(ids)
}

fn read_input(file: &str) -> Result<Vec<u8>> {
    if file == "-" {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf).context("reading stdin")?;
        Ok(buf)
    } else {
        std::fs::read(file).with_context(|| format!("reading {file}"))
    }
}

// ---- commands -------------------------------------------------------------

fn validate_url(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://"))
        && !url.contains(char::is_whitespace)
    {
        Ok(())
    } else {
        Err(error::invalid(format!("not an http(s) URL: {url:?}")))
    }
}

fn cmd_add(db: &mut Db, mode: Mode, args: &[String], baseline: bool) -> Result<()> {
    let mut urls = Vec::new();
    for a in args {
        if a == "-" {
            let input = String::from_utf8(read_input("-")?).context("stdin is not UTF-8")?;
            urls.extend(
                input
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from),
            );
        } else {
            urls.push(a.trim().to_string());
        }
    }
    // Validate everything before writing anything.
    for u in &urls {
        validate_url(u)?;
    }
    let mut results = Vec::new();
    for u in &urls {
        let (id, status) = db.add_feed(u, None, baseline)?;
        if status == AddStatus::Exists && mode == Mode::Human {
            eprintln!("rss: already subscribed: {u}");
        }
        results.push(json!({ "id": id, "url": u, "status": status }));
    }
    match mode {
        Mode::Json => emit_json(&json!({ "schema_version": SCHEMA_VERSION, "feeds": results })),
        Mode::Jsonl => emit_jsonl(results),
        _ => Ok(()),
    }
}

fn cmd_feeds(db: &Db, mode: Mode, all: bool) -> Result<()> {
    let feeds = db.feeds(all)?;
    match mode {
        Mode::Json => emit_json(&json!({ "schema_version": SCHEMA_VERSION, "feeds": feeds })),
        Mode::Jsonl => emit_jsonl(&feeds),
        _ => emit_lines(feeds.iter().map(|f| {
            let status = if !f.subscribed {
                "removed".to_string()
            } else {
                f.last_status
                    .clone()
                    .unwrap_or_else(|| "never fetched".into())
            };
            let error = f
                .last_error
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            format!(
                "{:>4}  {}  [{status}{error}]  {} unseen / {}  {}",
                f.id,
                f.title.as_deref().unwrap_or("(untitled)"),
                f.unseen,
                f.items,
                f.url
            )
        })),
    }
}

fn cmd_remove(db: &mut Db, mode: Mode, specs: &[String]) -> Result<()> {
    // Resolve everything first so an unknown feed changes nothing.
    let ids = specs
        .iter()
        .map(|s| db.resolve_feed(s))
        .collect::<Result<Vec<_>>>()?;
    let mut results = Vec::new();
    for id in ids {
        let changed = db.remove_feed(id)?;
        results.push(json!({ "id": id, "removed": changed }));
    }
    match mode {
        Mode::Json => emit_json(&json!({ "schema_version": SCHEMA_VERSION, "feeds": results })),
        Mode::Jsonl => emit_jsonl(results),
        _ => Ok(()),
    }
}

enum Outcome {
    NotModified,
    Parsed {
        feed: ParsedFeed,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

type Target = db::FetchTarget;

/// Fetch and parse every target on a small thread pool. Results are streamed back
/// so the caller can persist each feed as soon as it arrives.
fn fetch_all(
    targets: &[Target],
    timeout: Duration,
    jobs: usize,
    mut handle: impl FnMut(&Target, Result<Outcome, String>) -> Result<()>,
) -> Result<()> {
    let agent = fetch::agent(timeout);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(targets.len()) {
            let tx = tx.clone();
            let (agent, next) = (&agent, &next);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((_, url, etag, lm)) = targets.get(i) else {
                        break;
                    };
                    let result =
                        fetch::fetch(agent, url, etag.as_deref(), lm.as_deref()).and_then(|f| {
                            match f {
                                Fetched::NotModified => Ok(Outcome::NotModified),
                                Fetched::Body {
                                    bytes,
                                    etag,
                                    last_modified,
                                } => feed::parse(&bytes, Some(url))
                                    .map(|feed| Outcome::Parsed {
                                        feed,
                                        etag,
                                        last_modified,
                                    })
                                    .map_err(|e| e.to_string()),
                            }
                        });
                    if tx.send((i, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, result) in rx {
            handle(&targets[i], result)?;
        }
        Ok(())
    })
}

fn cmd_sync(
    db: &mut Db,
    mode: Mode,
    emit: Option<u64>,
    timeout: Duration,
    jobs: usize,
) -> Result<i32> {
    let started = time::now();
    let targets = db.fetch_targets()?;
    let sync_id = db.begin_sync(started)?;
    let (mut new, mut baselined) = (0i64, 0i64);
    let mut failures = Vec::new();

    fetch_all(&targets, timeout, jobs, |(feed_id, url, _, _), result| {
        match result {
            Ok(Outcome::NotModified) => db.record_not_modified(*feed_id)?,
            Ok(Outcome::Parsed {
                feed,
                etag,
                last_modified,
            }) => {
                let (n, b) = db.store_feed(
                    *feed_id,
                    sync_id,
                    &feed,
                    etag.as_deref(),
                    last_modified.as_deref(),
                )?;
                new += n;
                baselined += b;
            }
            Err(e) => {
                db.record_failure(*feed_id, &e)?;
                eprintln!("rss: feed {feed_id} ({url}): {e}");
                failures.push(json!({ "feed_id": feed_id, "url": url, "error": e }));
            }
        }
        Ok(())
    })?;

    let checked = targets.len() as i64;
    let failed = failures.len() as i64;
    let status = if failed == 0 {
        "ok"
    } else if failed == checked {
        "failed"
    } else {
        "partial"
    };
    let rec = SyncRecord {
        sync_id,
        status: status.into(),
        started_at: time::fmt(started),
        finished_at: Some(time::fmt(time::now())),
        feeds_checked: checked,
        feeds_failed: failed,
        new,
        baselined,
    };
    db.finish_sync(&rec)?;

    let emitted = if emit == Some(0) {
        Vec::new()
    } else {
        let f = Filter {
            sync_id: Some(sync_id),
            exclude_baselined: true,
            ..Filter::default()
        };
        db.items(&f, emit, 0)?
    };
    let remaining = new - emitted.len() as i64;
    let unseen_total = db.count(&Filter {
        unseen: true,
        ..Filter::default()
    })?;

    match mode {
        Mode::Json => emit_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "sync_id": sync_id,
            "status": status,
            "started_at": rec.started_at,
            "finished_at": rec.finished_at,
            "feeds_checked": checked,
            "feeds_failed": failed,
            "new": new,
            "baselined": baselined,
            "emitted": emitted.len(),
            "remaining": remaining,
            "unseen_total": unseen_total,
            "failures": failures,
            "items": compact(&emitted),
        }))?,
        Mode::Jsonl => emit_jsonl(compact(&emitted))?,
        _ => {
            if checked == 0 {
                eprintln!("rss: no subscriptions; add one with `rss add <url>`");
            }
            let mut header = format!("sync {sync_id}: {new} new, {unseen_total} unseen total");
            if baselined > 0 {
                header.push_str(&format!(", {baselined} baselined"));
            }
            if failed > 0 {
                header.push_str(&format!(" ({failed} of {checked} feeds failed)"));
            }
            let mut lines = vec![header];
            lines.extend(emitted.iter().map(|i| human_item(&i.item)));
            if remaining > 0 {
                lines.push(format!(
                    "… {remaining} more: rss items --sync {sync_id} --unseen"
                ));
            }
            emit_lines(lines)?;
        }
    }
    Ok(match status {
        "ok" => 0,
        "partial" => EXIT_PARTIAL,
        _ => EXIT_FAILED,
    })
}

fn cmd_items(
    db: &Db,
    mode: Mode,
    f: &Filter,
    limit: Option<u64>,
    offset: u64,
    full: bool,
) -> Result<()> {
    let total = db.count(f)?;
    let items = db.items(f, limit, offset)?;
    match mode {
        Mode::Json => {
            let mut env = json!({
                "schema_version": SCHEMA_VERSION,
                "total": total,
                "offset": offset,
                "limit": limit,
                "count": items.len(),
            });
            if let Some(b) = f.before {
                env["before"] = json!(time::fmt(b));
            }
            if let Some(a) = f.after {
                env["after"] = json!(time::fmt(a));
            }
            env["items"] = records(&items, full);
            emit_json(&env)
        }
        Mode::Jsonl if full => emit_jsonl(&items),
        Mode::Jsonl => emit_jsonl(compact(&items)),
        Mode::Ids => emit_lines(items.iter().map(|i| i.item.id.to_string())),
        Mode::Human => {
            emit_lines(items.iter().map(|i| human_item(&i.item)))?;
            let shown_to = offset as i64 + items.len() as i64;
            if shown_to < total {
                eprintln!(
                    "rss: showing {} of {total} (use --limit/--offset)",
                    items.len()
                );
            }
            Ok(())
        }
    }
}

fn cmd_get(db: &Db, mode: Mode, ids: &[i64]) -> Result<()> {
    let items = db.get(ids)?;
    match mode {
        Mode::Json => emit_json(&json!({ "schema_version": SCHEMA_VERSION, "items": items })),
        Mode::Jsonl => emit_jsonl(&items),
        _ => {
            let mut lines = Vec::new();
            for (n, f) in items.iter().enumerate() {
                let i = &f.item;
                if n > 0 {
                    lines.push(String::new());
                }
                lines.push(i.title.clone().unwrap_or_else(|| "(untitled)".into()));
                lines.push(format!("id:      {}", i.id));
                lines.push(format!("source:  {}", i.source));
                if let Some(a) = &f.author {
                    lines.push(format!("author:  {a}"));
                }
                if let Some(u) = &i.url {
                    lines.push(format!("url:     {u}"));
                }
                lines.push(format!("date:    {} ({})", i.effective_at, i.date_basis));
                let mut state = Vec::new();
                if let Some(s) = &i.seen_at {
                    state.push(format!("seen {s}"));
                }
                if let Some(s) = &i.starred_at {
                    state.push(format!("starred {s}"));
                }
                lines.push(format!(
                    "state:   {}",
                    if state.is_empty() {
                        "unseen".into()
                    } else {
                        state.join(", ")
                    }
                ));
                if let Some(t) = &f.summary_text {
                    lines.push(String::new());
                    lines.push(t.clone());
                }
            }
            emit_lines(lines)
        }
    }
}

fn cmd_status(db: &Db, mode: Mode, now: DateTime<Utc>, stale: Option<DateTime<Utc>>) -> Result<()> {
    let unseen = db.count(&Filter {
        unseen: true,
        ..Filter::default()
    })?;
    let starred = db.count(&Filter {
        starred: true,
        ..Filter::default()
    })?;
    let total = db.count(&Filter::default())?;
    let feeds = db.feeds(false)?.len();
    let (oldest, newest) = db.unseen_extremes()?;
    let stale = stale
        .map(|b| -> Result<_> {
            let n = db.count(&Filter {
                unseen: true,
                before: Some(b),
                ..Filter::default()
            })?;
            Ok((time::fmt(b), n))
        })
        .transpose()?;
    let last = db.last_sync()?;

    match mode {
        Mode::Json | Mode::Jsonl => {
            let mut env = json!({
                "schema_version": SCHEMA_VERSION,
                "now": time::fmt(now),
                "feeds": feeds,
                "items": total,
                "unseen": unseen,
                "starred": starred,
                "oldest_unseen": oldest,
                "newest_unseen": newest,
                "last_sync": last,
            });
            if let Some((before, n)) = &stale {
                env["stale"] = json!({ "before": before, "unseen": n });
            }
            emit_json(&env)
        }
        _ => {
            let mut lines = vec![
                format!("feeds:     {feeds}"),
                format!("unseen:    {unseen} of {total}"),
                format!("starred:   {starred}"),
            ];
            if let (Some(o), Some(n)) = (&oldest, &newest) {
                lines.push(format!(
                    "unseen from {} to {}",
                    o.effective_at, n.effective_at
                ));
            }
            if let Some((before, n)) = &stale {
                lines.push(format!("unseen before {before}: {n}"));
            }
            lines.push(match &last {
                Some(s) => format!(
                    "last sync: #{} {} at {} ({} feeds, {} failed, {} new)",
                    s.sync_id,
                    s.status,
                    s.finished_at.as_deref().unwrap_or("?"),
                    s.feeds_checked,
                    s.feeds_failed,
                    s.new
                ),
                None => "last sync: never".into(),
            });
            emit_lines(lines)
        }
    }
}

fn mutation_result(
    mode: Mode,
    matched: i64,
    changed: i64,
    before: Option<DateTime<Utc>>,
) -> Result<()> {
    if mode == Mode::Human {
        return Ok(());
    }
    let mut env =
        json!({ "schema_version": SCHEMA_VERSION, "matched": matched, "changed": changed });
    if let Some(b) = before {
        env["before"] = json!(time::fmt(b));
    }
    emit_json(&env)
}

fn cmd_seen(db: &mut Db, mode: Mode, ids: &[String], filter: &FilterArgs, all: bool) -> Result<()> {
    if !ids.is_empty() {
        if all || !filter.is_empty() {
            return Err(error::usage("give item IDs or filters/--all, not both"));
        }
        let ids = read_ids(ids)?;
        let (matched, changed) = db.mark_ids(&ids, StateField::Seen, true)?;
        return mutation_result(mode, matched, changed, None);
    }
    if filter.is_empty() && !all {
        return Err(error::usage(
            "seen needs item IDs, a filter (--before, --older-than, --feed, --sync, ...), or --all",
        ));
    }
    let f = filter.resolve(db, time::now())?;
    let (matched, changed) = db.mark_seen_where(&f)?;
    mutation_result(mode, matched, changed, f.before)
}

fn cmd_mark(db: &mut Db, mode: Mode, ids: &[String], field: StateField, on: bool) -> Result<()> {
    let ids = read_ids(ids)?;
    let (matched, changed) = db.mark_ids(&ids, field, on)?;
    mutation_result(mode, matched, changed, None)
}

fn cmd_import(db: &mut Db, mode: Mode, file: &str, baseline: bool) -> Result<()> {
    let bytes = read_input(file)?;
    let xml = String::from_utf8(bytes).map_err(|_| error::invalid("OPML is not valid UTF-8"))?;
    let outlines = opml::parse(&xml).map_err(|e| error::invalid(format!("{e:#}")))?;
    let mut results = Vec::new();
    let mut skipped = 0;
    for o in &outlines {
        if let Err(e) = validate_url(&o.url) {
            eprintln!("rss: skipping: {e}");
            skipped += 1;
            continue;
        }
        let (id, status) = db.add_feed(&o.url, o.title.as_deref(), baseline)?;
        results.push(json!({ "id": id, "url": o.url, "status": status }));
    }
    match mode {
        Mode::Json => emit_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "skipped": skipped,
            "feeds": results,
        })),
        Mode::Jsonl => emit_jsonl(results),
        _ => Ok(()),
    }
}

fn cmd_export(db: &Db) -> Result<()> {
    let outlines: Vec<_> = db
        .feeds(false)?
        .into_iter()
        .map(|f| opml::Outline {
            url: f.url,
            title: f.title,
        })
        .collect();
    let mut out = io::stdout().lock();
    out.write_all(opml::render(&outlines).as_bytes())?;
    out.flush()?;
    Ok(())
}

#[derive(Serialize)]
struct ParsedRecord {
    guid: Option<String>,
    url: Option<String>,
    title: Option<String>,
    author: Option<String>,
    published_at: Option<String>,
    updated_at: Option<String>,
    summary: Option<String>,
    summary_text: Option<String>,
    content: Option<String>,
    content_text: Option<String>,
}

fn cmd_parse(mode: Mode, file: &str) -> Result<()> {
    let bytes = read_input(file)?;
    let parsed = feed::parse(&bytes, None).map_err(|e| error::invalid(format!("{e:#}")))?;
    let records: Vec<ParsedRecord> = parsed
        .entries
        .iter()
        .map(|e| ParsedRecord {
            guid: e.guid.clone(),
            url: e.url.clone(),
            title: e.title.clone(),
            author: e.author.clone(),
            published_at: e.published.map(time::fmt),
            updated_at: e.updated.map(time::fmt),
            summary_text: e.summary_text(),
            content_text: e.content_text(),
            summary: e.summary.clone(),
            content: e.content.clone(),
        })
        .collect();
    match mode {
        Mode::Json => emit_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "title": parsed.title,
            "site_url": parsed.site_url,
            "items": records,
        })),
        Mode::Jsonl => emit_jsonl(&records),
        _ => emit_lines(records.iter().map(|r| {
            let date = r
                .published_at
                .as_deref()
                .or(r.updated_at.as_deref())
                .unwrap_or("-");
            format!("{date}  {}", r.title.as_deref().unwrap_or("(untitled)"))
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_stream_accepts_numbers_and_jsonl() {
        let ids = parse_id_stream("1 2\n\n3\n{\"id\":4,\"title\":\"x\"}\n").unwrap();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert!(parse_id_stream("abc").is_err());
        assert!(parse_id_stream("0").is_err());
        assert!(parse_id_stream("{\"title\":\"no id\"}").is_err());
    }

    #[test]
    fn limits() {
        assert_eq!(parse_limit("all").unwrap().0, None);
        assert_eq!(parse_limit("5").unwrap().0, Some(5));
        assert!(parse_limit("-1").is_err());
    }
}
