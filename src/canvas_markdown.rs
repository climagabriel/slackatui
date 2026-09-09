//! Canvas Markdown is CommonMark, not message mrkdwn. Keep rendering read-only.
use crate::palette::{Palette, Role};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

pub fn lines(markdown: &str, width: usize, palette: &Palette) -> Vec<Line<'static>> {
    let mut spans = Vec::new();
    let mut styles = vec![Style::default()];
    let mut links = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES,
    ) {
        let style = *styles.last().unwrap();
        match event {
            Event::Start(tag) => {
                let mut next = style;
                match tag {
                    Tag::Heading { .. } => {
                        next = next
                            .fg(palette.get(Role::Accent))
                            .add_modifier(Modifier::BOLD)
                    }
                    Tag::Strong => next = next.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => next = next.add_modifier(Modifier::ITALIC),
                    Tag::Strikethrough => next = next.add_modifier(Modifier::CROSSED_OUT),
                    Tag::CodeBlock(_) => next = next.fg(palette.get(Role::Code)),
                    Tag::BlockQuote(_) => spans.push(Span::styled("│ ", style)),
                    Tag::List(start) => {
                        if !spans.is_empty()
                            && !spans.last().is_some_and(|s| s.content.ends_with('\n'))
                        {
                            spans.push(Span::raw("\n"));
                        }
                        lists.push(start);
                    }
                    Tag::Item => {
                        let prefix = match lists.last_mut() {
                            Some(Some(number)) => {
                                let p = format!("{number}. ");
                                *number += 1;
                                p
                            }
                            _ => "• ".into(),
                        };
                        spans.push(Span::styled(
                            format!("{}{prefix}", "  ".repeat(lists.len().saturating_sub(1))),
                            style,
                        ));
                    }
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                        links.push(dest_url.to_string());
                        next = next
                            .fg(palette.get(Role::Link))
                            .add_modifier(Modifier::UNDERLINED);
                    }
                    Tag::TableHead => next = next.add_modifier(Modifier::BOLD),
                    _ => {}
                }
                styles.push(next);
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Link | TagEnd::Image => {
                        if let Some(url) = links.pop() {
                            spans.push(Span::styled(format!(" <{url}>"), style));
                        }
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                    }
                    TagEnd::TableCell => spans.push(Span::styled(" │ ", style)),
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::CodeBlock
                    | TagEnd::BlockQuote(_)
                    | TagEnd::Item
                    | TagEnd::TableRow
                    | TagEnd::TableHead => {
                        if !spans.last().is_some_and(|s| s.content.ends_with('\n')) {
                            spans.push(Span::raw("\n"));
                        }
                    }
                    _ => {}
                }
                styles.pop();
            }
            Event::Text(text) => spans.push(Span::styled(text.into_string(), style)),
            Event::Code(text) => spans.push(Span::styled(
                text.into_string(),
                style.fg(palette.get(Role::Code)),
            )),
            Event::HardBreak | Event::SoftBreak => spans.push(Span::styled("\n", style)),
            Event::Rule => spans.push(Span::styled(
                format!("{}\n", "─".repeat(width.min(40))),
                style.add_modifier(Modifier::DIM),
            )),
            Event::TaskListMarker(done) => {
                spans.push(Span::styled(if done { "[x] " } else { "[ ] " }, style))
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                spans.push(Span::styled(text.into_string(), style))
            }
            _ => {}
        }
    }
    wrap(spans, width)
}

// Wrap even long code and URLs so every character remains reachable in narrow panes.
fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut line = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut text = String::new();
        for ch in span.content.graphemes(true) {
            let size = unicode_width::UnicodeWidthStr::width(ch);
            if ch == "\n" || (used + size > width.max(1) && used > 0) {
                if !text.is_empty() {
                    line.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut line)));
                used = 0;
                if ch == "\n" {
                    continue;
                }
            }
            text.push_str(ch);
            used += size;
        }
        if !text.is_empty() {
            line.push(Span::styled(text, span.style));
        }
    }
    if !line.is_empty() || rows.is_empty() {
        rows.push(Line::from(line));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_lists_and_graphemes_do_not_split_or_run_together() {
        let rows = lines("- parent\n  - child\n- sibling", 40, &Palette::default());
        let text = rows
            .iter()
            .map(crate::render::line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("• parent\n  • child\n• sibling"), "{text}");
        let rows = lines("abc👍🏽", 4, &Palette::default());
        assert_eq!(
            rows.iter()
                .map(crate::render::line_text)
                .collect::<Vec<_>>(),
            ["abc", "👍🏽"]
        );
    }

    #[test]
    fn markdown_styles_and_literal_code_survive_wrapping() {
        let palette = Palette::default();
        let rows = lines("# Heading\n\n**bold** *italic* ~~gone~~ `a_b`\n\n- item\n\n> quote\n\n```\n  世界abcdefgh\n```\n\n[site](https://example.org)", 12, &palette);
        let spans: Vec<_> = rows.iter().flat_map(|l| l.spans.iter()).collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "Heading" && s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans
            .iter()
            .any(|s| s.content == "italic" && s.style.add_modifier.contains(Modifier::ITALIC)));
        let text = rows
            .iter()
            .map(crate::render::line_text)
            .collect::<String>();
        assert!(text.contains("  世界abcdefgh"));
        assert!(text.contains("• item"));
        assert!(text.contains("│ quote"));
        assert!(text.contains("https://example.org"));
        assert!(!text.contains("**"));
        assert!(rows.iter().all(|l| l.width() <= 12));
    }
}
