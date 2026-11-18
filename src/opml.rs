//! OPML import/export (subscriptions only).

use anyhow::{Context, Result};
use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

#[derive(Debug, PartialEq)]
pub struct Outline {
    pub url: String,
    pub title: Option<String>,
}

/// Collect every `<outline xmlUrl="...">`, at any nesting depth (folders are flattened).
pub fn parse(xml: &str) -> Result<Vec<Outline>> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event().context("malformed OPML")? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "outline" => {
                let (mut url, mut title, mut text) = (None, None, None);
                for attr in e.attributes().flatten() {
                    let value = attr
                        .normalized_value(XmlVersion::Implicit1_0)
                        .context("malformed OPML attribute")?;
                    let value = value.trim().to_string();
                    match attr.key.local_name().as_ref() {
                        "xmlUrl" | "xmlurl" => url = Some(value),
                        "title" => title = Some(value),
                        "text" => text = Some(value),
                        _ => {}
                    }
                }
                if let Some(url) = url.filter(|u| !u.is_empty()) {
                    let title = title.or(text).filter(|t| !t.is_empty());
                    out.push(Outline { url, title });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

pub fn render(feeds: &[Outline]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">\n  <head><title>rss subscriptions</title></head>\n  <body>\n",
    );
    for f in feeds {
        let title = escape(f.title.as_deref().unwrap_or(&f.url)).into_owned();
        s.push_str(&format!(
            "    <outline type=\"rss\" text=\"{title}\" title=\"{title}\" xmlUrl=\"{}\"/>\n",
            escape(f.url.as_str())
        ));
    }
    s.push_str("  </body>\n</opml>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let feeds = vec![
            Outline {
                url: "https://a.example/feed?x=1&y=2".into(),
                title: Some("A & \"B\"".into()),
            },
            Outline {
                url: "https://c.example/rss".into(),
                title: None,
            },
        ];
        let parsed = parse(&render(&feeds)).unwrap();
        assert_eq!(parsed[0], feeds[0]);
        assert_eq!(parsed[1].url, feeds[1].url);
    }

    #[test]
    fn nested_folders_and_text_fallback() {
        let xml = r#"<opml version="1.0"><body><outline text="Tech">
            <outline text="One" xmlUrl="https://one.example/feed"/>
            <outline text="No url"/></outline></body></opml>"#;
        assert_eq!(
            parse(xml).unwrap(),
            vec![Outline {
                url: "https://one.example/feed".into(),
                title: Some("One".into())
            }]
        );
    }
}
