//! Plain-text rendering of feed-supplied HTML. Strips markup, drops script/style,
//! decodes entities, collapses whitespace. Never summarizes or truncates.

use ego_tree::iter::Edge;
use scraper::{Html, Node};

const SKIP: &[&str] = &[
    "script", "style", "noscript", "template", "head", "iframe", "object",
];
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tr",
    "ul",
];

/// Render HTML as plain text: paragraphs separated by a blank line, inner whitespace collapsed.
pub fn html_to_text(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    let mut out = String::new();
    let mut skipping = 0usize;
    for edge in doc.tree.root().traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(e) if SKIP.contains(&e.name()) => skipping += 1,
                Node::Element(e) if BLOCK.contains(&e.name()) => out.push('\n'),
                // Source newlines in HTML are ordinary whitespace; only block elements break lines.
                Node::Text(t) if skipping == 0 => {
                    out.extend(t.chars().map(|c| if c == '\n' { ' ' } else { c }))
                }
                _ => {}
            },
            Edge::Close(node) => match node.value() {
                Node::Element(e) if SKIP.contains(&e.name()) => skipping -= 1,
                Node::Element(e) if BLOCK.contains(&e.name()) => out.push('\n'),
                _ => {}
            },
        }
    }
    paragraphs(&out)
}

/// Collapse whitespace in text that is already plain.
pub fn plain(text: &str) -> String {
    paragraphs(text)
}

fn paragraphs(s: &str) -> String {
    s.split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render according to a MIME type: HTML-ish types are parsed, anything else is collapsed.
pub fn render(body: &str, mime: Option<&str>) -> String {
    match mime {
        Some(m) if !m.contains("html") => plain(body),
        _ => html_to_text(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_and_scripts() {
        let html = r#"<p>Hello <b>world</b> &amp; friends</p><script>alert(1)</script>
            <style>p{}</style><ul><li>one</li><li>two</li></ul>"#;
        assert_eq!(html_to_text(html), "Hello world & friends\n\none\n\ntwo");
    }

    #[test]
    fn collapses_whitespace_and_breaks() {
        assert_eq!(html_to_text("a\n  b<br>c&nbsp;d"), "a b\n\nc d");
        assert_eq!(html_to_text("   "), "");
    }

    #[test]
    fn plain_text_is_not_parsed_as_html() {
        assert_eq!(render("a <b and c", Some("text/plain")), "a <b and c");
    }
}
