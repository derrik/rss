# rss

A tiny RSS/Atom reader for shells, scripts, and AI agents. It fetches feeds,
normalizes articles, remembers what has arrived, and exposes that through a
predictable CLI with stable JSON. It has no server, no daemon, and no opinions about
what matters.

> The tool knows what arrived. The consumer decides what matters.

This README covers usage, the output schema, exit codes, and design notes.

## Build and install

```sh
cargo build --release          # binary at target/release/rss
cargo install --path .         # or install into ~/.cargo/bin
cargo test                     # unit + end-to-end tests; no network needed
```

Requires a recent stable Rust (2024 edition). SQLite is bundled; there are no system
dependencies.

The database lives at `$XDG_DATA_HOME/rss/rss.db` (usually `~/.local/share/rss/rss.db`).
Override it with `--db PATH` or `RSS_DB=PATH`.

## Quick start

```sh
rss add https://blog.rust-lang.org/feed.xml
rss add --baseline https://hnrss.org/frontpage   # don't flood me with its backlog
rss sync                                          # fetch; show up to 5 new
rss items --unseen                                # what haven't I seen?
rss get 42                                        # full article record
rss seen 42                                       # I've seen it
rss star 42                                       # save for later
rss status                                        # counts and last sync outcome
```

## Chat-assistant workflow

The CLI has no conversational logic. These calls make the conversation easy:

```sh
# "What's in the news?"
rss sync --json
#  → {"sync_id":12,"status":"ok","new":17,"emitted":5,"remaining":12,"unseen_total":26,"items":[…5…]}
#  Assistant: "17 new stories. Want the first five?"

# The assistant reads five headlines, then acknowledges exactly those five:
rss seen 412 411 410 409 408

# "Hear the next five?" — the rest of *this* sync's stories, not older backlog:
rss items --sync 12 --unseen --limit 5 --json

# Status check with a staleness question:
rss status --older-than 4d --json
#  → {"unseen":12,"stale":{"before":"2026-11-13T15:00:00Z","unseen":12},"last_sync":{"status":"ok",…},…}
#  Assistant: "The remaining 12 unread stories are all more than four days old. Hear them, or mark them seen?"

# User says "mark them seen". Pass back the *same* absolute cutoff the assistant described,
# so nothing that aged past four days in the meantime gets swept up:
rss seen --before 2026-11-13T15:00:00Z --json
#  → {"matched":12,"changed":12,"before":"2026-11-13T15:00:00Z"}
```

Distinguishing the "no news" cases needs only one sync response:

| `new` | `unseen_total` | Meaning |
|---|---|---|
| 0 | 0 | Nothing new, all caught up |
| 0 | > 0 | Nothing new, but a backlog remains |
| > 0 | > `new` | New stories plus an older backlog |

Always check `status` (or the exit code) too. **"No new stories" from a `partial` or
`failed` sync is not a clean bill of health.**

### Undated feeds and big imports

Some feeds publish no dates. Their entries fall back to `date_basis: "discovered"`,
so a newly subscribed archive sorts as if it all arrived just now. Two tools help:

- `--dated` on `sync` and `items` leaves those entries out of headlines. They are
  still stored, counted, and reachable without the flag.
- `import --baseline` (or `add --baseline`) marks each feed's first batch seen, so
  importing dozens of subscriptions doesn't produce a 2,000-story "new" pile.

## Commands

| Command | Purpose |
|---|---|
| `add URL... [--baseline]` | Subscribe. `-` reads URLs from stdin (one per line; `#` comments OK). |
| `feeds [--all]` | List subscriptions with fetch status and counts. `--all` includes removed feeds. |
| `remove FEED...` | Unsubscribe by feed ID or URL. Articles and their state are kept. |
| `sync [--emit N\|all] [--no-emit] [--dated] [--timeout S] [--jobs N]` | Fetch all feeds, store everything new, and emit up to N (default 5). `--dated` emits only entries with a publisher date. |
| `items [filters] [--limit N\|all] [--offset N] [--full] [--ids]` | List stored entries, newest first (default limit 20). Never changes state. |
| `get ID...` | Full records, including raw and plain-text bodies. |
| `status [--older-than AGE \| --before TIME]` | Counts, unseen date range, optional stale count, last sync outcome. |
| `seen ID...` / `seen [filters]` / `seen --all` | Acknowledge entries. Filters narrow; an unfiltered bulk call needs `--all`. |
| `unseen ID...` | Undo an acknowledgment. |
| `star ID...` / `unstar ID...` | Save for later, independent of seen state. |
| `import FILE [--baseline]` / `export` | OPML in and out. Folders are flattened on import. |
| `parse [FILE]` | Parse a feed from a file or stdin without touching the database. |

Every ID-taking command accepts `-` to read IDs from stdin. Input can be bare
integers or JSONL item records, so `rss items --jsonl | jq -c 'select(…)' | rss seen -`
works.

**Filters** (on `items` and `seen`): `--unseen`, `--seen`, `--starred` and `--dated` (items only),
`--sync ID`, `--feed FEED`, `--older-than AGE` / `--before TIME`, and `--newer-than AGE` /
`--after TIME`. Filters combine with AND.
- Ages look like `30m`, `12h`, `3d`, `1w`.
- Times are RFC 3339 or `YYYY-MM-DD` (UTC midnight).
- Age filters compare against `effective_at`.

**Global flags**: `--db PATH`, `--json`, `--jsonl`.

## Output

- **Human (default)**: concise text. Mutating commands are silent on success.
- **`--json`**: one JSON object per invocation, always with `schema_version: 1`.
  Envelope keys appear in the order documented below.
- **`--jsonl`**: homogeneous JSON Lines, one record per line and nothing else. Use
  `--json` when you need counts.
- **`--ids`** (`items` only): bare IDs, one per line.

Stdout is always valid for the chosen mode. Warnings, per-feed failures, and hints
go to stderr.

### Item records

**Compact** records come from `sync`, `items`, and `--jsonl` streams. They are body-free:

```json
{"id":412,"feed_id":3,"source":"Engineering Blog","title":"Example headline",
 "url":"https://example.org/story","published_at":"2026-11-17T12:00:00Z",
 "effective_at":"2026-11-17T12:00:00Z","date_basis":"published",
 "first_seen_at":"2026-11-17T15:00:00Z","sync_id":12,"seen_at":null,"starred_at":null}
```

**Full** records come from `get` and `items --full`. They add:

```json
{"feed_url":"…","guid":"…","author":"…","updated_at":null,
 "summary":"<p>raw HTML as supplied</p>","summary_text":"plain text",
 "content":"<raw/>","content_text":"plain text"}
```

| Field | Meaning |
|---|---|
| `id` | Compact local ID, never reused. Pass it back to `get`/`seen`/`star`. |
| `source` | Feed title (falls back to the feed URL). |
| `title` | Plain text; HTML in feed titles is stripped. |
| `effective_at`, `date_basis` | Story date used for ordering and age filters. The basis is `published`, `updated` (no publish date given), or `discovered` (date missing, unparseable, more than 1h in the future, or before 1995). |
| `first_seen_at`, `sync_id` | When, and in which sync, this database first saw the entry. |
| `seen_at`, `starred_at` | `null` or the time of the explicit action. |
| `summary_text` | Sanitized plain text: tags stripped, script/style dropped, entities decoded, whitespace collapsed. Falls back to the content when the feed has no summary. Never truncated. |

All timestamps are UTC RFC 3339 with second precision, so they sort as strings.

### Envelopes

```text
sync    {schema_version, sync_id, status, started_at, finished_at, feeds_checked,
         feeds_failed, new, baselined, emitted, remaining, unseen_total,
         failures:[{feed_id,url,error}], items:[compact]}
items   {schema_version, total, offset, limit, count, before?, after?, items:[compact|full]}
get     {schema_version, items:[full]}
status  {schema_version, now, feeds, items, unseen, starred,
         oldest_unseen:{id,effective_at,date_basis}|null, newest_unseen:…|null,
         last_sync:{sync_id,status,started_at,finished_at,feeds_checked,feeds_failed,new,baselined}|null,
         stale?:{before,unseen}}
seen/unseen/star/unstar   {schema_version, matched, changed, before?}
feeds   {schema_version, feeds:[{id,url,title,site_url,subscribed,baseline_pending,added_at,
         removed_at,last_attempt_at,last_success_at,last_status,last_error,
         consecutive_failures,items,unseen}]}
add/remove/import   {schema_version, feeds:[{id,url,status}|{id,removed}], skipped? (import)}
parse   {schema_version, title, site_url, items:[{guid,url,title,author,published_at,
         updated_at,summary,summary_text,content,content_text}]}
error   {schema_version, error:{code,message}}     code ∈ not_found | invalid_input | usage | error
```

Field meanings:
- **`sync`**: `remaining` is `new − emitted` for *this* sync, not the unseen backlog.
  `baselined` entries (from `add --baseline`) are stored already seen and are not
  counted in `new`. `status` is `ok`, `partial`, or `failed`.
- **`items`**: `total` counts all matches before paging. When a relative age was given,
  `before`/`after` echo the absolute cutoff that was used.
- **`seen` and the other mutations**: `changed` excludes entries already in the target
  state, so a repeated call reports 0.
- **`feeds`**: `last_status` is `ok`, `not_modified`, or `error`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. For `sync`: every feed fetched OK or returned 304 Not Modified. Zero subscriptions also counts as success. |
| 1 | Error: unknown ID, invalid input, database or I/O failure. |
| 2 | Usage error. |
| 3 | `sync` partially failed: some feeds failed, the rest were stored. |
| 4 | `sync` failed: every feed failed. |

Codes 3 and 4 still write a complete envelope or stream to stdout. With `--json`,
errors with codes 1 and 2 also print an `error` envelope.

## Design notes

- **Storage.** One SQLite file in WAL mode with a 30-second busy timeout. Writes that
  must be atomic with a read run in `BEGIN IMMEDIATE` transactions: per-feed ingest
  (dedup plus baselining), bulk `seen`, and ID mutations. So concurrent `sync` runs
  are safe, and each entry is credited to exactly one sync. Network fetches happen
  outside transactions, on a small thread pool. Each feed is committed as soon as it
  arrives.
- **Identity.** Public IDs are SQLite integers, which are short and cheap for an agent
  to copy. Dedup uses a separate per-feed key: the feed GUID, else the item link, else
  a SHA-256 of title, summary, content, and date. An updated entry keeps its ID and
  state. Its fields are refreshed, and a field the feed omits keeps its previous value.
- **Unsubscribe is soft.** `remove` flips a flag. Re-adding the same URL resumes the
  same feed, so its old entries aren't rediscovered as new.
- **Raw plus derived text.** Raw summary and content are stored exactly as supplied.
  `summary_text` and `content_text` are derived when output is produced, so improving
  the sanitizer never needs a migration. Titles are the exception: they are stored as
  plain text.
- **Untrusted content.** Feed text is only ever data. It is never executed or
  interpreted, and markup is stripped, not rendered.
- **Redirects** are followed (up to 10), but the subscription URL isn't rewritten.
  Responses are capped at 16 MiB.
- **Crates.**
  - `feed-rs`: RSS 0.9x/1.0/2.0, Atom, and JSON Feed parsing.
  - `rusqlite` (bundled SQLite).
  - `ureq` (blocking HTTP with rustls; no async runtime).
  - `clap`, `serde_json`, `chrono`.
  - `scraper`: HTML to text.
  - `quick-xml`: OPML.

### Other notes

- **JSONL streams contain only records.** No summary record is mixed in. Every item
  record carries its `sync_id`; use `--json` when you need counts.
- **JSON Feed** parses too, via `feed-rs`, alongside RSS and Atom.
- **Binary size.** The release binary is about 9.5 MB, mostly rustls, html5ever, and
  bundled SQLite. That is the cost of having no system dependencies.
