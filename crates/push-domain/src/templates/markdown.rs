//! The Markdown a `markdown` block shows: a CommonMark subset with GFM tables and
//! strikethrough, without raw HTML, images, footnotes or links other than absolute
//! http(s) ones.

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

const MAX_LIST_DEPTH: usize = 4;
const MAX_QUOTE_DEPTH: usize = 3;
const MAX_TABLE_COLUMNS: usize = 20;
const MAX_TABLE_ROWS: usize = 200;
const MAX_SHOWN_SCHEME_CHARS: usize = 32;
const ONLY_HTTP: &str = "only absolute http(s) links are allowed";

/// Checks the Markdown structure of `text`: only the allowed subset, within its nesting
/// and table limits. Lengths and characters are `check_value`'s job.
pub fn check_markdown(text: &str) -> Result<(), String> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(text, options);
    let mut definitions: Vec<_> = parser.reference_definitions().iter().collect();
    definitions.sort_by_key(|(_, def)| def.span.start);
    for (_, def) in definitions {
        check_url(&def.dest).map_err(|e| format!("has a link reference definition that {e}"))?;
    }
    let mut lists = 0;
    let mut quotes = 0;
    let mut rows = 0;
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::CodeBlock(_)
                | Tag::Item
                | Tag::TableHead
                | Tag::TableCell
                | Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough => {}
                Tag::List(_) => {
                    lists += 1;
                    if lists > MAX_LIST_DEPTH {
                        return Err(format!(
                            "nests lists {lists} levels; the limit is {MAX_LIST_DEPTH}"
                        ));
                    }
                }
                Tag::BlockQuote(None) => {
                    quotes += 1;
                    if quotes > MAX_QUOTE_DEPTH {
                        return Err(format!(
                            "nests quotes {quotes} levels; the limit is {MAX_QUOTE_DEPTH}"
                        ));
                    }
                }
                Tag::Table(columns) => {
                    if columns.len() > MAX_TABLE_COLUMNS {
                        return Err(format!(
                            "has a table with {} columns; the limit is {MAX_TABLE_COLUMNS}",
                            columns.len()
                        ));
                    }
                    rows = 0;
                }
                Tag::TableRow => {
                    rows += 1;
                    if rows > MAX_TABLE_ROWS {
                        return Err(format!(
                            "has a table with more than {MAX_TABLE_ROWS} rows; \
                             the limit is {MAX_TABLE_ROWS}, not counting the header"
                        ));
                    }
                }
                Tag::Link {
                    link_type: LinkType::Email,
                    ..
                } => return Err(format!("contains an email link; {ONLY_HTTP}")),
                Tag::Link { dest_url, .. } => check_url(&dest_url)?,
                Tag::HtmlBlock => return Err("contains raw HTML".into()),
                Tag::Image { .. } => return Err("contains an image".into()),
                Tag::FootnoteDefinition(_) => return Err("contains a footnote".into()),
                _ => return Err("contains unsupported Markdown".into()),
            },
            Event::End(TagEnd::List(_)) => lists -= 1,
            Event::End(TagEnd::BlockQuote(_)) => quotes -= 1,
            Event::End(_)
            | Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Rule => {}
            Event::Html(_) | Event::InlineHtml(_) => return Err("contains raw HTML".into()),
            Event::FootnoteReference(_) => return Err("contains a footnote".into()),
            _ => return Err("contains unsupported Markdown".into()),
        }
    }
    Ok(())
}

/// An absolute `http://` or `https://` URL with a host and no whitespace or control
/// characters. The error names the scheme only, never the whole URL.
fn check_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err(format!("links to nothing; {ONLY_HTTP}"));
    }
    let Some((scheme, rest)) = url.split_once(':').filter(|(s, _)| is_scheme(s)) else {
        return Err(format!("links to a relative URL; {ONLY_HTTP}"));
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        let shown: String = scheme.chars().take(MAX_SHOWN_SCHEME_CHARS).collect();
        return Err(format!("links to \"{shown}:...\"; {ONLY_HTTP}"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("links to a URL with whitespace or control characters".into());
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return Err(format!(
            "links to an http(s) URL without a host; {ONLY_HTTP}"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(ip) => ip.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(format!(
            "links to an http(s) URL without a host; {ONLY_HTTP}"
        ));
    }
    Ok(())
}

/// RFC 3986: a letter, then letters, digits, `+`, `-` or `.`.
fn is_scheme(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(text: &str, expected: &str) {
        let error = check_markdown(text).expect_err(text);
        assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    }

    fn nested_lists(depth: usize) -> String {
        (0..depth)
            .map(|i| format!("{}- level {}\n", "  ".repeat(i), i + 1))
            .collect()
    }

    fn table(columns: usize, rows: usize) -> String {
        let row = |cell: &str| format!("|{}\n", format!(" {cell} |").repeat(columns));
        let mut text = row("h");
        text.push_str(&row("-"));
        for _ in 0..rows {
            text.push_str(&row("x"));
        }
        text
    }

    #[test]
    fn a_document_with_every_allowed_construct_passes() {
        let text = concat!(
            "# Heading 1\n\n",
            "###### Heading 6\n\n",
            "Setext\n======\n\n",
            "Setext two\n----------\n\n",
            "Plain *em* _em_ **strong** ~~gone~~ `code <b>` and a\nsoft break and a  \nhard one.\n\n",
            "A [link](https://example.com/a?b=c#d \"title\"), [plain](http://x.io), ",
            "<https://auto.example.com> and [a ref][r].\n\n",
            "[r]: https://example.com/ref\n\n",
            "- one\n- two\n  1. nested\n  2. ordered\n\n",
            "3. starts at three\n\n",
            "> quoted\n> > twice\n\n",
            "```rust\nlet x = \"<script>\";\n```\n\n",
            "    indented <script>\n\n",
            "---\n\n",
            "| a | b |\n|:--|--:|\n| 1 | 2 |\n",
        );
        assert_eq!(check_markdown(text), Ok(()));
    }

    #[test]
    fn each_allowed_construct_passes_alone() {
        for text in [
            "plain",
            "line\nbreak",
            "hard  \nbreak",
            "**strong**",
            "*em*",
            "~~strike~~",
            "`<script>alert(1)</script>`",
            "```\n<script>alert(1)</script>\n```",
            "~~~js\nrun()\n~~~",
            "    indented",
            "## Heading",
            "Setext\n===",
            "- a\n- b",
            "1. a\n2. b",
            "> quote",
            "| a |\n|---|\n| b |",
            "***",
            "[x](https://example.com)",
            "[x](HTTPS://EXAMPLE.COM)",
            "[x](http://[::1]:8080/p)",
            "<http://example.com>",
            "[x][r]\n\n[r]: https://example.com",
            "",
        ] {
            assert_eq!(check_markdown(text), Ok(()), "{text:?}");
        }
    }

    #[test]
    fn refuses_raw_html() {
        refused("a <b>x</b>", "contains raw HTML");
        refused("<script>alert(1)</script>", "contains raw HTML");
        refused("<div>\nx\n</div>", "contains raw HTML");
        refused("<!-- hidden -->", "contains raw HTML");
    }

    #[test]
    fn refuses_images() {
        refused("![cat](https://example.com/cat.png)", "contains an image");
    }

    #[test]
    fn refuses_links_other_than_absolute_http() {
        refused(
            "[x](javascript:alert(1))",
            "links to \"javascript:...\"; only absolute http(s) links are allowed",
        );
        refused("[x](javascript&#58;alert(1))", "\"javascript:...\"");
        refused("[x](JavaScript:alert(1))", "\"JavaScript:...\"");
        refused("<data:text/html,x>", "links to \"data:...\"");
        refused("[x](file:///etc/passwd)", "links to \"file:...\"");
        refused("[x](mailto:a@example.com)", "links to \"mailto:...\"");
        refused("<a@example.com>", "contains an email link");
        refused("[x](/relative/path)", "links to a relative URL");
        refused("[x](example.com)", "links to a relative URL");
        refused("[x]()", "links to nothing");
        refused("[x](https:example.com)", "without a host");
        refused("[x](https:///path)", "without a host");
        refused("[x](https://user@/path)", "without a host");
        refused("[x](<https://a b.com>)", "whitespace or control characters");
    }

    #[test]
    fn a_long_scheme_is_not_echoed_in_full() {
        let text = format!("[x]({}:payload)", "a".repeat(100));
        let error = check_markdown(&text).unwrap_err();
        assert!(!error.contains(&"a".repeat(33)), "{error}");
        assert!(!error.contains("payload"), "{error}");
    }

    #[test]
    fn refuses_reference_definitions_to_other_schemes_even_unused() {
        refused(
            "text\n\n[a]: file:///etc/passwd",
            "has a link reference definition that links to \"file:...\"",
        );
        refused("[a]\n\n[a]: javascript:alert(1)", "\"javascript:...\"");
    }

    #[test]
    fn refuses_footnotes() {
        refused("Note[^1].\n\n[^1]: The footnote.", "contains a footnote");
        refused("[^1]: An unused footnote.", "contains a footnote");
    }

    #[test]
    fn lists_nest_at_most_four_levels() {
        assert_eq!(check_markdown(&nested_lists(4)), Ok(()));
        refused(&nested_lists(5), "nests lists 5 levels; the limit is 4");
        assert_eq!(check_markdown("- a\n- b\n\n1. c\n2. d\n"), Ok(()));
    }

    #[test]
    fn quotes_nest_at_most_three_levels() {
        assert_eq!(check_markdown("> > > three"), Ok(()));
        refused("> > > > four", "nests quotes 4 levels; the limit is 3");
        assert_eq!(check_markdown("> a\n\n> b\n\n> c\n\n> d"), Ok(()));
    }

    #[test]
    fn tables_have_at_most_twenty_columns_and_two_hundred_rows() {
        assert_eq!(check_markdown(&table(20, 1)), Ok(()));
        refused(
            &table(21, 1),
            "has a table with 21 columns; the limit is 20",
        );
        assert_eq!(check_markdown(&table(2, 200)), Ok(()));
        refused(&table(2, 201), "more than 200 rows");
        let two = format!("{}\n{}", table(2, 150), table(2, 150));
        assert_eq!(check_markdown(&two), Ok(()));
    }
}
