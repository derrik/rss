//! End-to-end acceptance scenarios, run against the real binary and a
//! local HTTP server. No live network access.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as Age, Utc};
use serde_json::Value;
use tempfile::TempDir;

// ---- test HTTP server -----------------------------------------------------

#[derive(Clone)]
struct Route {
    status: u16,
    body: String,
    delay: Duration,
    etag: Option<String>,
}

#[derive(Clone)]
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

impl Server {
    fn start() -> Server {
        let http = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", http.server_addr().to_ip().unwrap());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let r = routes.clone();
        std::thread::spawn(move || {
            for req in http.incoming_requests() {
                let routes = r.clone();
                // One thread per request so a slow route can't stall the others.
                std::thread::spawn(move || {
                    let route = routes.lock().unwrap().get(req.url()).cloned();
                    let Some(route) = route else {
                        let _ = req.respond(tiny_http::Response::empty(404));
                        return;
                    };
                    std::thread::sleep(route.delay);
                    let inm = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("If-None-Match"))
                        .map(|h| h.value.to_string());
                    if route.etag.is_some() && inm == route.etag {
                        let _ = req.respond(tiny_http::Response::empty(304));
                        return;
                    }
                    let mut resp =
                        tiny_http::Response::from_string(route.body).with_status_code(route.status);
                    if let Some(e) = route.etag {
                        resp.add_header(tiny_http::Header::from_bytes("ETag", e).unwrap());
                    }
                    let _ = req.respond(resp);
                });
            }
        });
        Server { base, routes }
    }

    fn set(&self, path: &str, body: String) {
        self.put(
            path,
            Route {
                status: 200,
                body,
                delay: Duration::ZERO,
                etag: None,
            },
        );
    }

    fn put(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

// ---- feed fixtures --------------------------------------------------------

struct Entry {
    guid: String,
    title: String,
    published: Option<DateTime<Utc>>,
}

fn entry(guid: &str, age: Option<Age>) -> Entry {
    Entry {
        guid: guid.to_string(),
        title: format!("Headline {guid}"),
        published: age.map(|a| Utc::now() - a),
    }
}

/// `n` entries named `{prefix}{i}`, published `i` hours ago (+ `offset`).
fn entries(prefix: &str, n: usize, offset: Age) -> Vec<Entry> {
    (1..=n)
        .map(|i| entry(&format!("{prefix}{i}"), Some(offset + Age::hours(i as i64))))
        .collect()
}

fn rss(title: &str, entries: &[Entry]) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>{title}</title><link>https://example.org/</link>"
    );
    for e in entries {
        s.push_str(&format!(
            "<item><title>{}</title><guid isPermaLink=\"false\">{}</guid><link>https://example.org/{}</link>\
             <description>&lt;p&gt;About &lt;b&gt;{}&lt;/b&gt;&lt;/p&gt;</description>",
            e.title, e.guid, e.guid, e.guid
        ));
        if let Some(p) = e.published {
            s.push_str(&format!("<pubDate>{}</pubDate>", p.to_rfc2822()));
        }
        s.push_str("</item>");
    }
    s.push_str("</channel></rss>");
    s
}

// ---- CLI driver -----------------------------------------------------------

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

struct Rss {
    _dir: TempDir,
    db: String,
}

impl Rss {
    fn new() -> Rss {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("test.db").to_string_lossy().into_owned();
        Rss { _dir: dir, db }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rss"));
        c.arg("--db").arg(&self.db).args(args).env_remove("RSS_DB");
        c
    }

    fn run_in(&self, args: &[&str], stdin: &str) -> Out {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8(o.stdout).unwrap(),
            stderr: String::from_utf8(o.stderr).unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_in(args, "")
    }

    /// Run, assert exit code, parse stdout as JSON.
    fn json(&self, args: &[&str], code: i32) -> Value {
        let o = self.run(args);
        assert_eq!(
            o.code, code,
            "args {args:?}\nstdout: {}\nstderr: {}",
            o.stdout, o.stderr
        );
        // Human-mode mutations are silent; treat that as null rather than invalid JSON.
        if o.stdout.is_empty() {
            Value::Null
        } else {
            o.json()
        }
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.json(args, 0)
    }
}

fn ids(v: &Value) -> Vec<i64> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect()
}

// ---- 1, 3, 11: initial sync, overflow, sync addressing --------------------

#[test]
fn initial_sync_emits_five_and_persists_all() {
    let srv = Server::start();
    srv.set("/a", rss("Feed A", &entries("a", 17, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);

    let s = r.ok(&["sync", "--json"]);
    assert_eq!(s["schema_version"], 1);
    assert_eq!(s["status"], "ok");
    assert_eq!(s["feeds_checked"], 1);
    assert_eq!(s["feeds_failed"], 0);
    assert_eq!(s["new"], 17);
    assert_eq!(s["emitted"], 5);
    assert_eq!(s["remaining"], 12);
    assert_eq!(s["unseen_total"], 17);
    let items = s["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    // Newest first, compact (body-free).
    assert_eq!(items[0]["title"], "Headline a1");
    assert_eq!(items[4]["title"], "Headline a5");
    assert_eq!(items[0]["source"], "Feed A");
    assert_eq!(items[0]["date_basis"], "published");
    assert!(items[0].get("summary").is_none() && items[0].get("content").is_none());

    // The other 12 are queryable locally, by sync ID, without fetching again.
    let sync_id = s["sync_id"].as_i64().unwrap();
    for id in ids(&s) {
        r.ok(&["seen", &id.to_string()]);
    }
    let rest = r.ok(&[
        "items",
        "--sync",
        &sync_id.to_string(),
        "--unseen",
        "--limit",
        "all",
        "--json",
    ]);
    assert_eq!(rest["total"], 12);
    assert_eq!(rest["items"][0]["title"], "Headline a6");

    // Explicit emit limits.
    srv.set("/a", rss("Feed A", &entries("b", 3, Age::zero())));
    assert_eq!(r.ok(&["sync", "--emit", "all", "--json"])["emitted"], 3);
    srv.set("/a", rss("Feed A", &entries("c", 3, Age::zero())));
    let s = r.ok(&["sync", "--no-emit", "--json"]);
    assert_eq!(
        (
            s["new"].as_i64(),
            s["emitted"].as_i64(),
            s["remaining"].as_i64()
        ),
        (Some(3), Some(0), Some(3))
    );
    assert_eq!(s["items"].as_array().unwrap().len(), 0);
}

#[test]
fn items_sync_excludes_older_backlog_even_when_backdated() {
    let srv = Server::start();
    // Backlog: published within the last few hours.
    srv.set("/a", rss("A", &entries("old", 3, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);
    // New discoveries that are *backdated* behind the backlog.
    let mut all = entries("old", 3, Age::zero());
    all.extend(entries("new", 7, Age::days(2)));
    srv.set("/a", rss("A", &all));
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(s["new"], 7);
    assert_eq!(s["unseen_total"], 10);
    // A plain unseen query mixes in the backlog first...
    let unseen = r.ok(&["items", "--unseen", "--limit", "5", "--json"]);
    assert!(
        unseen["items"][0]["title"]
            .as_str()
            .unwrap()
            .contains("old")
    );
    // ...while --sync addresses exactly this sync's discoveries.
    let sid = s["sync_id"].to_string();
    let mine = r.ok(&["items", "--sync", &sid, "--limit", "all", "--json"]);
    assert_eq!(mine["total"], 7);
    assert!(
        mine["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["title"].as_str().unwrap().contains("new"))
    );
}

// ---- 2: repeat sync -------------------------------------------------------

#[test]
fn repeat_sync_has_no_duplicates_and_keeps_state() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 4, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    let first = r.ok(&["sync", "--json"]);
    let id = ids(&first)[0].to_string();
    r.ok(&["seen", &id]);
    r.ok(&["star", &id]);

    let again = r.ok(&["sync", "--json"]);
    assert_eq!(again["status"], "ok");
    assert_eq!(again["new"], 0);
    assert_eq!(again["unseen_total"], 3);
    assert_eq!(r.ok(&["items", "--json"])["total"], 4);
    let got = r.ok(&["get", &id, "--json"]);
    assert!(got["items"][0]["seen_at"].is_string());
    assert!(got["items"][0]["starred_at"].is_string());
}

#[test]
fn conditional_requests_and_updated_entries() {
    let srv = Server::start();
    let mut route = Route {
        status: 200,
        body: rss("A", &entries("a", 2, Age::zero())),
        delay: Duration::ZERO,
        etag: Some("\"v1\"".into()),
    };
    srv.put("/a", route.clone());
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    let first = r.ok(&["sync", "--json"]);
    let id = ids(&first)[0];
    r.ok(&["star", &id.to_string()]);

    // 304 Not Modified is a success with nothing new.
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (s["status"].as_str(), s["new"].as_i64()),
        (Some("ok"), Some(0))
    );
    let feeds = r.ok(&["feeds", "--json"]);
    assert_eq!(feeds["feeds"][0]["last_status"], "not_modified");

    // An updated entry keeps its local ID and state; it is not "new".
    let mut changed = entries("a", 2, Age::zero());
    changed[0].title = "Headline a1 (updated)".into();
    route.body = rss("A", &changed);
    route.etag = Some("\"v2\"".into());
    srv.put("/a", route);
    assert_eq!(r.ok(&["sync", "--json"])["new"], 0);
    let got = r.ok(&["get", &id.to_string(), "--json"]);
    assert_eq!(got["items"][0]["title"], "Headline a1 (updated)");
    assert!(got["items"][0]["starred_at"].is_string());
}

// ---- 4: acknowledgment ----------------------------------------------------

#[test]
fn reading_never_mutates_and_seen_is_idempotent() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 5, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--no-emit", "--json"]);

    let five = r.ok(&["items", "--unseen", "--limit", "5", "--json"]);
    r.ok(&["get", "1", "2", "3", "--json"]);
    r.ok(&["items", "--full", "--json"]);
    r.ok(&["status", "--json"]);
    assert_eq!(
        r.ok(&["status", "--json"])["unseen"],
        5,
        "reads must not change state"
    );

    let picked = ids(&five);
    let (a, b) = (picked[0].to_string(), picked[1].to_string());
    let m = r.ok(&["seen", &a, &b, "--json"]);
    assert_eq!(
        (m["matched"].as_i64(), m["changed"].as_i64()),
        (Some(2), Some(2))
    );
    let m = r.ok(&["seen", &a, &b, "--json"]);
    assert_eq!(
        (m["matched"].as_i64(), m["changed"].as_i64()),
        (Some(2), Some(0)),
        "repeat is a no-op"
    );
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 3);

    // Conversational paging by acknowledgment: next page excludes what was presented.
    let next = r.ok(&["items", "--unseen", "--limit", "5", "--json"]);
    assert_eq!(ids(&next), picked[2..].to_vec());
    // Offset paging for nonmutating inspection.
    let page2 = r.ok(&["items", "--limit", "2", "--offset", "2", "--json"]);
    assert_eq!(ids(&page2), picked[2..4].to_vec());
    assert_eq!(page2["total"], 5);

    // unseen undoes.
    r.ok(&["unseen", &a]);
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 4);
}

#[test]
fn unknown_id_fails_atomically() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 2, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);
    let err = r.json(&["seen", "1", "999", "--json"], 1);
    assert_eq!(err["error"]["code"], "not_found");
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 2, "nothing changed");
    assert_eq!(r.run(&["get", "42"]).code, 1);
}

// ---- 5: aged backlog ------------------------------------------------------

#[test]
fn aged_backlog_cleared_by_absolute_cutoff() {
    let srv = Server::start();
    let mut list = entries("old", 4, Age::days(5)); // 5 days + i hours old
    list.extend(entries("fresh", 3, Age::zero())); // a few hours old
    srv.set("/a", rss("A", &list));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);

    // The assistant asks how stale the backlog is; status reports the absolute cutoff it used.
    let st = r.ok(&["status", "--older-than", "4d", "--json"]);
    assert_eq!(st["unseen"], 7);
    assert_eq!(st["stale"]["unseen"], 4);
    let cutoff = st["stale"]["before"].as_str().unwrap().to_string();
    assert!(st["oldest_unseen"]["effective_at"].as_str().unwrap() < cutoff.as_str());

    let old = r.ok(&["items", "--unseen", "--before", &cutoff, "--json"]);
    assert_eq!(old["total"], 4);
    assert_eq!(old["before"], cutoff.as_str());
    let starred_old = ids(&old)[0].to_string();
    r.ok(&["star", &starred_old]);

    // User agrees; clear exactly what was described.
    let m = r.ok(&["seen", "--before", &cutoff, "--json"]);
    assert_eq!(m["changed"], 4);
    assert_eq!(m["before"], cutoff.as_str());
    let st = r.ok(&["status", "--json"]);
    assert_eq!(st["unseen"], 3, "younger entries untouched");
    assert_eq!(st["starred"], 1, "stars untouched");
    let got = r.ok(&["get", &starred_old, "--json"]);
    assert!(got["items"][0]["starred_at"].is_string());

    // Relative form also works and echoes its resolved cutoff.
    let m = r.ok(&["seen", "--older-than", "10d", "--json"]);
    assert_eq!(m["changed"], 0);
    assert!(m["before"].is_string());
}

#[test]
fn bulk_seen_requires_explicit_scope() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 3, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);
    let o = r.run(&["seen"]);
    assert_eq!(o.code, 2, "bare `seen` is a usage error");
    assert_eq!(r.run(&["seen", "1", "--all"]).code, 2);
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 3);
    assert_eq!(r.ok(&["seen", "--all", "--json"])["changed"], 3);
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 0);
}

// ---- 6: starring ----------------------------------------------------------

#[test]
fn starring_is_independent_of_seen() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 2, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);
    r.ok(&["star", "1"]);
    r.ok(&["seen", "1"]);
    let st = r.ok(&["items", "--starred", "--json"]);
    assert_eq!(ids(&st), vec![1]);
    assert!(st["items"][0]["seen_at"].is_string());
    r.ok(&["unstar", "1"]);
    let got = r.ok(&["get", "1", "--json"]);
    assert!(got["items"][0]["starred_at"].is_null());
    assert!(
        got["items"][0]["seen_at"].is_string(),
        "unstar leaves seen alone"
    );
}

// ---- 7: failure isolation and exit codes ----------------------------------

#[test]
fn partial_and_total_failure() {
    let srv = Server::start();
    srv.set("/good", rss("Good", &entries("g", 2, Age::zero())));
    srv.set("/flaky", rss("Flaky", &entries("f", 2, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/good"), &srv.url("/flaky")]);
    r.ok(&["sync", "--json"]);

    // Flaky now times out; good publishes something new.
    srv.put(
        "/flaky",
        Route {
            status: 200,
            body: String::new(),
            delay: Duration::from_secs(3),
            etag: None,
        },
    );
    let mut more = entries("g", 2, Age::zero());
    more.push(entry("g-new", Some(Age::minutes(5))));
    srv.set("/good", rss("Good", &more));
    let s = r.json(&["sync", "--timeout", "1", "--json"], 3);
    assert_eq!(s["status"], "partial");
    assert_eq!(s["feeds_failed"], 1);
    assert_eq!(s["new"], 1, "successful feed still persisted");
    assert_eq!(s["failures"][0]["url"], srv.url("/flaky"));
    assert!(
        s["failures"][0]["error"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    let flaky_items = r.ok(&["items", "--feed", &srv.url("/flaky"), "--json"]);
    assert_eq!(
        flaky_items["total"], 2,
        "failed feed's previous data remains"
    );
    assert_eq!(
        r.ok(&["status", "--json"])["last_sync"]["status"],
        "partial"
    );

    // Malformed XML and HTTP errors count as failures too; all failing → exit 4.
    srv.set("/good", "<rss><channel><item>".into());
    srv.put(
        "/flaky",
        Route {
            status: 500,
            body: String::new(),
            delay: Duration::ZERO,
            etag: None,
        },
    );
    let s = r.json(&["sync", "--json"], 4);
    assert_eq!(s["status"], "failed");
    assert_eq!(s["new"], 0);
    assert_eq!(r.ok(&["items", "--json"])["total"], 5, "nothing lost");

    // JSONL mode still signals failure through the exit code.
    assert_eq!(r.run(&["sync", "--jsonl"]).code, 4);
}

// ---- 8: shell composition -------------------------------------------------

#[test]
fn jsonl_and_id_streams_compose() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 6, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);

    // Sync JSONL is homogeneous: only item records.
    let o = r.run(&["sync", "--jsonl"]);
    assert_eq!(o.code, 0);
    let lines: Vec<Value> = o
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 5);
    assert!(
        lines
            .iter()
            .all(|l| l["id"].is_i64() && l["sync_id"].is_i64())
    );

    // Pipe JSONL records straight back into `seen -`.
    let o = r.run(&["items", "--unseen", "--limit", "2", "--jsonl"]);
    let m = r.run_in(&["seen", "-", "--json"], &o.stdout);
    assert_eq!(m.code, 0, "{}", m.stderr);
    assert_eq!(m.json()["changed"], 2);

    // Bare IDs.
    let o = r.run(&["items", "--unseen", "--ids"]);
    assert_eq!(o.stdout.lines().count(), 4);
    assert!(o.stdout.lines().all(|l| l.parse::<i64>().is_ok()));
    let m = r.run_in(&["seen", "-"], &o.stdout);
    assert_eq!((m.code, m.stdout.as_str()), (0, ""), "silent on success");
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 0);

    // Full JSONL carries bodies with sanitized text.
    let o = r.run(&["items", "--full", "--limit", "1", "--jsonl"]);
    let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["summary_text"], "About a1");
    assert!(v["summary"].as_str().unwrap().contains("<b>"));
}

// ---- 9: OPML --------------------------------------------------------------

#[test]
fn opml_import_export_round_trip() {
    let r = Rss::new();
    let opml = r#"<?xml version="1.0"?><opml version="2.0"><body>
        <outline text="News"><outline text="One &amp; Two" xmlUrl="https://one.example/feed?a=1&amp;b=2"/></outline>
        <outline text="Three" type="rss" xmlUrl="https://three.example/rss"/>
        <outline text="Bogus" xmlUrl="ftp://nope"/></body></opml>"#;
    let o = r.run_in(&["import", "-", "--json"], opml);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["feeds"].as_array().unwrap().len(), 2);
    assert_eq!(v["skipped"], 1);

    let exported = r.run(&["export"]).stdout;
    let r2 = Rss::new();
    let v2 = r2.run_in(&["import", "-", "--json"], &exported).json();
    assert_eq!(v2["feeds"][0]["url"], "https://one.example/feed?a=1&b=2");
    let feeds = r2.ok(&["feeds", "--json"]);
    assert_eq!(feeds["feeds"][0]["title"], "One & Two");
    assert_eq!(feeds["feeds"].as_array().unwrap().len(), 2);
}

// ---- 10: no-news cases ----------------------------------------------------

#[test]
fn distinguishes_no_news_cases() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 2, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);

    // (b) no new + unseen backlog
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (
            s["new"].as_i64(),
            s["unseen_total"].as_i64(),
            s["status"].as_str()
        ),
        (Some(0), Some(2), Some("ok"))
    );

    // (a) no new + nothing unseen
    r.ok(&["seen", "--all"]);
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (s["new"].as_i64(), s["unseen_total"].as_i64()),
        (Some(0), Some(0))
    );

    // (c) new entries + older backlog
    r.ok(&["unseen", "1"]);
    let mut list = entries("a", 2, Age::zero());
    list.push(entry("a-new", Some(Age::minutes(1))));
    srv.set("/a", rss("A", &list));
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (s["new"].as_i64(), s["unseen_total"].as_i64()),
        (Some(1), Some(2))
    );
}

// ---- 12: baselining -------------------------------------------------------

#[test]
fn baseline_marks_only_first_fetch() {
    let srv = Server::start();
    srv.put(
        "/a",
        Route {
            status: 503,
            body: String::new(),
            delay: Duration::ZERO,
            etag: None,
        },
    );
    srv.set("/b", rss("B", &entries("b", 2, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", "--baseline", &srv.url("/a")]);
    r.ok(&["add", &srv.url("/b")]);

    // A failed fetch doesn't consume the baseline.
    let s = r.json(&["sync", "--json"], 3);
    assert_eq!(
        (s["new"].as_i64(), s["baselined"].as_i64()),
        (Some(2), Some(0))
    );

    srv.set("/a", rss("A", &entries("a", 30, Age::zero())));
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (
            s["new"].as_i64(),
            s["baselined"].as_i64(),
            s["emitted"].as_i64()
        ),
        (Some(0), Some(30), Some(0))
    );
    assert_eq!(
        s["unseen_total"], 2,
        "baselined entries are seen; other feeds untouched"
    );

    let mut list = entries("a", 30, Age::zero());
    list.push(entry("a-new", Some(Age::minutes(1))));
    srv.set("/a", rss("A", &list));
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(
        (s["new"].as_i64(), s["baselined"].as_i64()),
        (Some(1), Some(0))
    );
    assert_eq!(s["items"][0]["title"], "Headline a-new");
}

// ---- 13: unsubscribe ------------------------------------------------------

#[test]
fn remove_keeps_history_and_readd_resumes() {
    let srv = Server::start();
    srv.set("/a", rss("A", &entries("a", 3, Age::zero())));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    r.ok(&["sync", "--json"]);
    r.ok(&["star", "2"]);
    r.ok(&["seen", "1"]);

    r.ok(&["remove", "1"]);
    assert_eq!(
        r.ok(&["feeds", "--json"])["feeds"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        r.ok(&["feeds", "--all", "--json"])["feeds"][0]["subscribed"],
        false
    );
    assert_eq!(r.ok(&["sync", "--json"])["feeds_checked"], 0);
    assert_eq!(r.ok(&["items", "--json"])["total"], 3);
    assert_eq!(r.ok(&["items", "--starred", "--json"])["total"], 1);

    let add = r.ok(&["add", &srv.url("/a"), "--json"]);
    assert_eq!(add["feeds"][0]["status"], "resubscribed");
    assert_eq!(add["feeds"][0]["id"], 1);
    let s = r.ok(&["sync", "--json"]);
    assert_eq!(s["new"], 0, "not rediscovered as new");
    assert_eq!(r.ok(&["status", "--json"])["unseen"], 2);

    assert_eq!(r.run(&["remove", "99"]).code, 1);
}

// ---- 14: concurrency ------------------------------------------------------

#[test]
fn concurrent_syncs_credit_each_entry_once() {
    let srv = Server::start();
    let r = Rss::new();
    for f in 0..6 {
        let path = format!("/f{f}");
        srv.set(
            &path,
            rss(
                &format!("F{f}"),
                &entries(&format!("f{f}-"), 25, Age::zero()),
            ),
        );
        r.ok(&["add", &srv.url(&path)]);
    }
    let spawn = || {
        r.cmd(&["sync", "--no-emit", "--json", "--jobs", "3"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let children: Vec<_> = (0..4).map(|_| spawn()).collect();
    let mut total_new = 0;
    for c in children {
        let o = c.wait_with_output().unwrap();
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        total_new += v["new"].as_i64().unwrap();
    }
    assert_eq!(total_new, 150, "each entry credited to exactly one sync");
    assert_eq!(r.ok(&["items", "--json"])["total"], 150, "no duplicates");
}

// ---- dates and parsing ----------------------------------------------------

#[test]
fn date_basis_falls_back_for_missing_and_future_dates() {
    let srv = Server::start();
    let list = vec![
        entry("dated", Some(Age::hours(2))),
        entry("undated", None),
        entry("future", Some(Age::days(-30))),
    ];
    srv.set("/a", rss("A", &list));
    let r = Rss::new();
    r.ok(&["add", &srv.url("/a")]);
    let s = r.ok(&["sync", "--emit", "all", "--json"]);
    let by_title = |t: &str| {
        s["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == t)
            .unwrap()
            .clone()
    };
    assert_eq!(by_title("Headline dated")["date_basis"], "published");
    let undated = by_title("Headline undated");
    assert_eq!(undated["date_basis"], "discovered");
    assert_eq!(undated["effective_at"], undated["first_seen_at"]);
    let future = by_title("Headline future");
    assert_eq!(future["date_basis"], "discovered");
    assert!(
        future["published_at"].is_string(),
        "raw publisher date preserved"
    );
}

#[test]
fn parse_needs_no_database() {
    let r = Rss::new();
    let o = r.run_in(
        &["parse", "--json"],
        &rss("Standalone", &entries("p", 2, Age::zero())),
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["title"], "Standalone");
    assert_eq!(v["items"][0]["summary_text"], "About p1");
    assert!(!std::path::Path::new(&r.db).exists());
    assert_eq!(r.run_in(&["parse"], "garbage").code, 1);
}

#[test]
fn usage_and_validation_errors() {
    let r = Rss::new();
    assert_eq!(r.run(&["add", "not-a-url"]).code, 1);
    assert_eq!(r.run(&["items", "--older-than", "3 days"]).code, 2);
    assert_eq!(r.run(&["items", "--json", "--jsonl"]).code, 2);
    assert_eq!(r.run(&["items", "--ids", "--json"]).code, 2);
    assert_eq!(r.run(&["frobnicate"]).code, 2);
    let o = r.run(&["sync", "--json"]);
    assert_eq!(o.code, 0, "no subscriptions is not an error");
    assert_eq!(o.json()["feeds_checked"], 0);
}
