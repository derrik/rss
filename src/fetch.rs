//! HTTP fetching with conditional requests, timeouts, and a response size cap.

use std::time::Duration;

use ureq::Agent;

/// Largest feed body we will read.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

pub enum Fetched {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

pub fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(10)
        .user_agent(concat!("rss/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Fetch one feed. Errors are returned as short human-readable strings for storage/reporting.
pub fn fetch(
    agent: &Agent,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Fetched, String> {
    let mut req = agent.get(url);
    if let Some(v) = etag {
        req = req.header("If-None-Match", v);
    }
    if let Some(v) = last_modified {
        req = req.header("If-Modified-Since", v);
    }
    let mut resp = req.call().map_err(describe)?;
    let status = resp.status().as_u16();
    if status == 304 {
        return Ok(Fetched::NotModified);
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let etag = header("etag");
    let last_modified = header("last-modified");
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_vec()
        .map_err(describe)?;
    Ok(Fetched::Body {
        bytes,
        etag,
        last_modified,
    })
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "timed out".to_string(),
        ureq::Error::BodyExceedsLimit(n) => format!("response larger than {n} bytes"),
        other => other.to_string(),
    }
}
