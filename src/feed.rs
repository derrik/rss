//! Feed parsing and normalization: bytes in, plain entries out. No I/O, no database.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use feed_rs::model::{Entry, Link, Text};
use sha2::{Digest, Sha256};

use crate::text;

#[derive(Debug, Clone)]
pub struct ParsedFeed {
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub entries: Vec<ParsedEntry>,
}

#[derive(Debug, Clone)]
pub struct ParsedEntry {
    /// Internal per-feed dedup key: `id:<guid>`, `url:<link>`, or `hash:<sha256>`.
    pub identity: String,
    pub guid: Option<String>,
    pub url: Option<String>,
    /// Plain-text title.
    pub title: Option<String>,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub summary_type: Option<String>,
    pub content: Option<String>,
    pub content_type: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
}

impl ParsedEntry {
    pub fn summary_text(&self) -> Option<String> {
        summary_text(
            self.summary.as_deref(),
            self.summary_type.as_deref(),
            self.content.as_deref(),
            self.content_type.as_deref(),
        )
    }

    pub fn content_text(&self) -> Option<String> {
        render_opt(self.content.as_deref(), self.content_type.as_deref())
    }
}

/// `summary_text` falls back to the content when no summary is supplied.
pub fn summary_text(
    summary: Option<&str>,
    summary_type: Option<&str>,
    content: Option<&str>,
    content_type: Option<&str>,
) -> Option<String> {
    render_opt(summary, summary_type).or_else(|| render_opt(content, content_type))
}

pub fn render_opt(body: Option<&str>, mime: Option<&str>) -> Option<String> {
    let t = text::render(body?, mime);
    (!t.is_empty()).then_some(t)
}

/// Parse a feed document (RSS 0.9x/1.0/2.0, Atom, or JSON Feed).
pub fn parse(bytes: &[u8], base_url: Option<&str>) -> Result<ParsedFeed> {
    let parser = feed_rs::parser::Builder::new()
        .sanitize_content(false)
        // Leave missing IDs empty; we derive our own identity below.
        .id_generator(|_, _, _| String::new())
        .base_uri(base_url)
        .build();
    let feed = parser
        .parse(bytes)
        .map_err(|e| anyhow!("malformed feed: {e}"))?;
    Ok(ParsedFeed {
        title: feed.title.as_ref().map(text_of).filter(|t| !t.is_empty()),
        site_url: pick_link(&feed.links),
        entries: feed.entries.iter().map(normalize).collect(),
    })
}

fn text_of(t: &Text) -> String {
    text::render(
        &t.content,
        Some(t.content_type.essence().to_string().as_str()),
    )
}

fn nonempty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The article link: first `alternate` (or rel-less) link, else the first link.
fn pick_link(links: &[Link]) -> Option<String> {
    links
        .iter()
        .find(|l| matches!(l.rel.as_deref(), None | Some("alternate")))
        .or_else(|| links.first())
        .and_then(|l| nonempty(&l.href))
}

fn normalize(e: &Entry) -> ParsedEntry {
    let guid = nonempty(&e.id);
    let url = pick_link(&e.links);
    let title = e.title.as_ref().map(text_of).filter(|t| !t.is_empty());
    let summary = e.summary.as_ref().and_then(|s| nonempty(&s.content));
    let summary_type = e
        .summary
        .as_ref()
        .map(|s| s.content_type.essence().to_string());
    let content = e
        .content
        .as_ref()
        .and_then(|c| c.body.as_deref())
        .and_then(nonempty);
    let content_type = e
        .content
        .as_ref()
        .map(|c| c.content_type.essence().to_string());
    let author = e
        .authors
        .iter()
        .filter_map(|p| p.name.as_deref().and_then(nonempty))
        .collect::<Vec<_>>();
    let author = (!author.is_empty()).then(|| author.join(", "));

    let identity = if let Some(g) = &guid {
        format!("id:{g}")
    } else if let Some(u) = &url {
        format!("url:{u}")
    } else {
        let mut h = Sha256::new();
        for part in [
            title.as_deref().unwrap_or(""),
            summary.as_deref().unwrap_or(""),
            content.as_deref().unwrap_or(""),
            &e.published.map(|t| t.to_rfc3339()).unwrap_or_default(),
        ] {
            h.update(part.as_bytes());
            h.update([0u8]);
        }
        let digest = h.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("hash:{hex}")
    };

    ParsedEntry {
        identity,
        guid,
        url,
        title,
        author,
        summary,
        summary_type,
        content,
        content_type,
        published: e.published,
        updated: e.updated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Test &amp; Co</title><link>https://example.org/</link>
<item><title>With guid</title><guid isPermaLink="false">abc-1</guid><link>https://example.org/1</link>
  <pubDate>Mon, 16 Nov 2026 08:00:00 GMT</pubDate><description>&lt;p&gt;Hi &lt;b&gt;there&lt;/b&gt;&lt;/p&gt;</description></item>
<item><title>Link only</title><link>https://example.org/2</link></item>
<item><title>Nothing</title><description>orphan</description></item>
<item><title>Bad date</title><link>https://example.org/4</link><pubDate>not a date</pubDate></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Atom Feed</title><id>urn:feed</id>
<updated>2026-11-16T00:00:00Z</updated><link rel="alternate" href="https://atom.example/"/>
<entry><title type="html">A &lt;em&gt;fancy&lt;/em&gt; title</title><id>urn:e1</id>
  <updated>2026-11-15T10:00:00Z</updated><author><name>Ada</name></author>
  <link rel="alternate" href="https://atom.example/e1"/>
  <content type="html">&lt;p&gt;Body&lt;/p&gt;&lt;script&gt;x()&lt;/script&gt;</content></entry>
</feed>"#;

    #[test]
    fn rss_identity_fallbacks() {
        let f = parse(RSS.as_bytes(), None).unwrap();
        assert_eq!(f.title.as_deref(), Some("Test & Co"));
        assert_eq!(f.site_url.as_deref(), Some("https://example.org/"));
        let ids: Vec<_> = f.entries.iter().map(|e| e.identity.as_str()).collect();
        assert_eq!(ids[0], "id:abc-1");
        assert_eq!(ids[1], "url:https://example.org/2");
        assert!(ids[2].starts_with("hash:"));
        assert_eq!(f.entries[0].summary_text().as_deref(), Some("Hi there"));
        assert!(f.entries[0].published.is_some());
        assert!(
            f.entries[3].published.is_none(),
            "unparseable dates become None"
        );
    }

    #[test]
    fn hash_identity_is_deterministic() {
        let a = parse(RSS.as_bytes(), None).unwrap();
        let b = parse(RSS.as_bytes(), None).unwrap();
        assert_eq!(a.entries[2].identity, b.entries[2].identity);
    }

    #[test]
    fn atom_fields() {
        let f = parse(ATOM.as_bytes(), None).unwrap();
        let e = &f.entries[0];
        assert_eq!(e.identity, "id:urn:e1");
        assert_eq!(e.title.as_deref(), Some("A fancy title"));
        assert_eq!(e.author.as_deref(), Some("Ada"));
        assert_eq!(e.url.as_deref(), Some("https://atom.example/e1"));
        assert!(e.published.is_none() && e.updated.is_some());
        // No summary: summary_text falls back to content, scripts dropped.
        assert_eq!(e.summary_text().as_deref(), Some("Body"));
        assert!(
            e.content.as_deref().unwrap().contains("<script>"),
            "raw content preserved"
        );
    }

    #[test]
    fn malformed_is_an_error() {
        assert!(parse(b"<rss><channel><item>", None).is_err());
        assert!(parse(b"not xml at all", None).is_err());
    }
}
