//! `/labels`: while it is on, every element on screen carries a short dim tag
//! naming the code that draws it, so a change can be asked for by name.
//!
//! Two placements, and both are applied to the buffer after the element has
//! drawn itself. A bordered element takes the right end of its top border, or
//! of its bottom border when a title fills the top one; a borderless one
//! appends the tag after its own text, one space clear of it. Either way the
//! cells the tag would take must be free — border dashes, or blanks — so a tag
//! never covers content, is never truncated to fit, and the element drawn over
//! another one keeps its own: a tag that has nowhere to go is dropped.
//!
//! Nothing here is stored. A list holds its lines, never their tags, and the
//! draw asks for them again each frame.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// The dim `ui.rs` gives secondary text. Only the modifier is set, so a tag
/// keeps whatever colors the cells under it already had.
pub fn style() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// A tag as a border carries it: a space each side, the way a title sits off
/// the corner.
pub fn border_text(tag: &str) -> String {
    format!(" {tag} ")
}

/// A rect that is not wholly on screen carries no tag. Every caller passes a
/// rect the layout produced, so this is a guard rather than a case: the
/// primitives index the buffer directly, and an element hanging off the edge
/// has no right end and no row end to append to either.
fn on_screen(area: Rect, buffer: &Buffer) -> bool {
    !area.is_empty() && area.intersection(buffer.area) == area
}

/// The last cell the text of `row` occupies.
///
/// A double-width glyph is stored in one cell and the next cell is left
/// blank, so the rightmost cell with a symbol in it is not the rightmost cell
/// the text covers: a row ending in `界` occupies the blank after it too.
/// Counting the glyph's display width is what keeps the space between text
/// and tag a real space.
fn occupied_end(buffer: &Buffer, row: Rect) -> Option<u16> {
    let mut end = None;
    for x in row.x..row.right() {
        let symbol = buffer[(x, row.y)].symbol();
        if symbol == " " || symbol.is_empty() {
            continue;
        }
        end = Some((x + symbol.width().max(1) as u16 - 1).min(row.right() - 1));
    }
    end
}

/// Where a tag `width` cells wide goes on `row`: one space past the end of
/// the row's own text, or one space in from the left when the row has none.
/// `None` when the tag and that space do not both fit, or when the cells are
/// not blank.
fn slot(buffer: &Buffer, row: Rect, width: u16) -> Option<u16> {
    if !on_screen(row, buffer) || width == 0 {
        return None;
    }
    // The space is budgeted whether the row has text or not: the rule is room
    // for the whole tag plus one space. An empty row's text ends just left of
    // it, so the tag lands one cell in.
    let start = match occupied_end(buffer, row) {
        Some(end) => u32::from(end) + 2,
        None => u32::from(row.x) + 1,
    };
    if start + u32::from(width) > u32::from(row.right()) {
        return None;
    }
    let start = start as u16;
    (start..start + width)
        .all(|x| buffer[(x, row.y)].symbol() == " ")
        .then_some(start)
}

/// The right end of `area`'s top border, ending one cell before the corner;
/// failing that, the right end of its bottom border.
///
/// Every cell the tag wants must still be a border dash: a title that reaches
/// that far is content, and content wins. The conversations pane is the case
/// the fallback exists for — its title routinely fills the top border, while
/// the bottom one carries a title far less often. A box with room on neither
/// border drops the tag.
pub fn border(frame: &mut Frame, on: bool, area: Rect, tag: &str) {
    if !on {
        return;
    }
    let text = border_text(tag);
    let width = text.width() as u16;
    let buffer = frame.buffer_mut();
    if !on_screen(area, buffer) || width == 0 {
        return;
    }
    // Both corners stay, and there is no point in a tag with no border left.
    if u32::from(width) + 2 > u32::from(area.width) {
        return;
    }
    let corner = area.right() - 1;
    let start = corner - width;
    let bottom = area.bottom() - 1;
    // One row when the box is one row tall: its top border is its bottom one.
    for y in [area.y, bottom].into_iter().take(if area.y == bottom { 1 } else { 2 }) {
        if (start..corner).all(|x| buffer[(x, y)].symbol() == "─") {
            buffer.set_string(start, y, &text, style());
            return;
        }
    }
}

/// One space past the end of `row`'s own text, inside `row`.
pub fn after_text(frame: &mut Frame, on: bool, row: Rect, tag: &str) {
    if !on {
        return;
    }
    let buffer = frame.buffer_mut();
    let Some(start) = slot(buffer, row, tag.width() as u16) else {
        return;
    };
    buffer.set_string(start, row.y, tag, style());
}

/// Where `after_text` would put `tag` on `row` of `buffer`, or `None` when it
/// would drop it. The tests read the rule from here rather than restating it.
#[cfg(test)]
pub fn after_text_at(buffer: &Buffer, row: Rect, tag: &str) -> Option<u16> {
    slot(buffer, row, tag.width() as u16)
}

/// The text at `x, y` for `width` cells.
#[cfg(test)]
pub fn text_at(buffer: &Buffer, x: u16, y: u16, width: u16) -> String {
    (x..x + width).map(|x| buffer[(x, y)].symbol()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::ui_fixtures::state;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// One state drawn at one size, with the mode on.
    fn drawn(mut app: App, width: u16, height: u16) -> Buffer {
        app.labels = true;
        draw(app, width, height)
    }

    fn draw(mut app: App, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn symbol(buffer: &Buffer, x: u16, y: u16) -> String {
        buffer[(x, y)].symbol().to_string()
    }

    /// Where `tag` is drawn, as the column and row of its first cell. Matched
    /// cell by cell, since a column is not a byte offset. A name that is the
    /// start of a longer one (`divider` inside `divider_new`) is not a match.
    fn find(buffer: &Buffer, tag: &str) -> Option<(u16, u16)> {
        let area = buffer.area;
        let want: Vec<String> = tag.chars().map(|c| c.to_string()).collect();
        for y in area.top()..area.bottom() {
            let row: Vec<&str> = (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            for start in 0..row.len().saturating_sub(want.len() - 1) {
                if row[start..start + want.len()]
                    .iter()
                    .zip(&want)
                    .any(|(cell, want)| *cell != want.as_str())
                {
                    continue;
                }
                let after = row
                    .get(start + want.len())
                    .and_then(|cell| cell.chars().next());
                if !after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    return Some((area.left() + start as u16, y));
                }
            }
        }
        None
    }

    /// A bordered element's tag ends against a corner of its own box, with
    /// border left of it rather than a title it would have covered. The row it
    /// landed on comes back, so a test can say which border that was.
    fn assert_border_tag(buffer: &Buffer, tag: &str) -> u16 {
        let text = border_text(tag);
        let (x, y) = find(buffer, &text).unwrap_or_else(|| panic!("{tag} is not drawn"));
        let corner = symbol(buffer, x + text.width() as u16, y);
        assert!(
            ["┐", "┘", "╮", "╯"].contains(&corner.as_str()),
            "{tag} ends on {corner:?}, not a corner of its box"
        );
        assert_eq!(symbol(buffer, x - 1, y), "─", "{tag} covers a title");
        y
    }

    /// A borderless element's tag follows its own text, one space clear of it.
    fn assert_appended_tag(buffer: &Buffer, tag: &str) {
        let (x, y) = find(buffer, tag).unwrap_or_else(|| panic!("{tag} is not drawn"));
        assert_eq!(symbol(buffer, x - 1, y), " ", "{tag} is not clear of the text");
        assert_ne!(
            symbol(buffer, x - 2, y),
            " ",
            "{tag} does not follow the text of its row"
        );
    }

    /// The panes and the overlays each name themselves on their top border,
    /// when its right end is free.
    #[test]
    fn a_bordered_element_tags_the_right_end_of_its_top_border() {
        let mut home = state("conversations");
        // A short title leaves the right end of the top border free.
        home.unreads_first = false;
        let buffer = drawn(home, 160, 24);
        assert_eq!(assert_border_tag(&buffer, "draw_convs"), 0);
        assert_eq!(assert_border_tag(&buffer, "draw_msgs"), 0);
        assert_eq!(assert_border_tag(&drawn(state("scan-overlay"), 160, 24), "scan_overlay"), 2);
        assert_eq!(assert_border_tag(&drawn(state("channel-tabs"), 160, 24), "Browser::draw"), 0);
        assert_eq!(assert_border_tag(&drawn(state("pane-picker"), 160, 24), "draw_pane_menu"), 0);
        assert_border_tag(&drawn(state("key-guide"), 160, 40), "draw_help");
    }

    /// A title that fills the top border sends the tag to the bottom one. The
    /// conversations pane at an ordinary width is that case: its own title
    /// leaves the top border nothing.
    #[test]
    fn a_full_top_border_sends_the_tag_to_the_bottom_border() {
        let buffer = drawn(state("conversations"), 120, 24);
        // The pane is 30 wide and 23 tall; its bottom border is the last row
        // of the main area, not the status line below it.
        let y = assert_border_tag(&buffer, "draw_convs");
        assert_eq!(y, 22);
        assert_eq!(symbol(&buffer, 29, 22), "┘");
        // The messages pane beside it has room on top and stays there.
        assert_eq!(assert_border_tag(&buffer, "draw_msgs"), 0);
    }

    /// Neither border has room, so there is no tag and no truncation.
    #[test]
    fn a_box_too_narrow_for_either_border_shows_no_tag() {
        let mut terminal = Terminal::new(TestBackend::new(12, 4)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(ratatui::widgets::Block::bordered(), area);
                border(frame, true, area, "draw_something_long");
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(text_at(buffer, 0, 0, 12), "┌──────────┐");
        assert_eq!(text_at(buffer, 0, 3, 12), "└──────────┘");
    }

    /// Every part of a message, and the dividers between messages.
    #[test]
    fn a_message_names_each_part_it_draws() {
        let buffer = drawn(state("conversation"), 160, 30);
        for tag in [
            "message_header",
            "message_body",
            "file_label",
            "reaction_lines",
            "thread_footer",
            "divider",
            "divider_new",
        ] {
            assert_appended_tag(&buffer, tag);
        }
        // The header's tag sits on the header's own row, not on a body row.
        let (_, header) = find(&buffer, "message_header").expect("header");
        let row = text_at(&buffer, 0, header, 160);
        assert!(row.contains("1970-01-01 00:16 UTC"), "{row:?}");
    }

    /// A collapsed message's elision, and the two lines a THREADS card puts
    /// around its messages.
    #[test]
    fn an_elision_and_a_card_name_their_own_lines() {
        assert_appended_tag(&drawn(state("collapsed"), 160, 18), "collapse_elision");
        let buffer = drawn(state("threads"), 160, 30);
        assert_appended_tag(&buffer, "card_header");
        assert_appended_tag(&buffer, "card_elision");
    }

    /// The editors and viewers that fill the messages pane have no border of
    /// their own, and append their tag to their first row.
    #[test]
    fn a_borderless_view_appends_its_tag_to_its_first_row() {
        for (fixture, tag) in [
            ("keys-editor", "draw_keys"),
            ("color-palette", "draw_color_palette"),
            ("raw-json", "draw_raw"),
            ("images", "draw_image_view"),
            ("conversations", "draw_status"),
        ] {
            let buffer = drawn(state(fixture), 160, 20);
            assert_appended_tag(&buffer, tag);
        }
        // The first row of the view, not some later row of it.
        let buffer = drawn(state("keys-editor"), 160, 20);
        let (_, y) = find(&buffer, "draw_keys").expect("draw_keys");
        assert!(text_at(&buffer, 0, y, 160).contains(" action "));
    }

    /// The compose box carries its tag past the hints already on its border,
    /// and gives the tag up first when the border narrows.
    #[test]
    fn the_compose_border_drops_its_tag_before_a_hint() {
        let wide = drawn(state("compose"), 120, 14);
        let (x, y) = find(&wide, "draw_compose").expect("draw_compose");
        let row = text_at(&wide, 0, y, 120);
        assert!(row.contains("Ctrl-v image · draw_compose "), "{row:?}");
        assert_eq!(symbol(&wide, x + "draw_compose ".width() as u16, y), "┐");
        let narrow = drawn(state("compose"), 50, 12);
        let row = (0..12)
            .map(|y| text_at(&narrow, 0, y, 50))
            .find(|row| row.contains("Enter send"))
            .expect("the hints");
        assert!(row.contains("Ctrl-j newline"), "{row:?}");
        assert!(!row.contains("draw_compose"), "{row:?}");
    }

    /// One row of `width` cells holding `text`, tagged, as symbols.
    fn tagged_row(text: &str, width: u16, tag: &str) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(ratatui::widgets::Paragraph::new(text.to_string()), area);
                after_text(frame, true, area, tag);
            })
            .expect("draw");
        text_at(terminal.backend().buffer(), 0, 0, width)
    }

    /// A double-width glyph holds two cells and leaves the second blank. The
    /// tag starts past both of them, with a real space between.
    #[test]
    fn a_double_width_glyph_keeps_its_second_cell_and_the_space_after_it() {
        for glyph in ["界", "🙂"] {
            assert_eq!(glyph.width(), 2, "{glyph} is not double width");
            // Glyph, its blank second cell, the space, the two-cell tag.
            assert_eq!(tagged_row(glyph, 5, "ab"), format!("{glyph}  ab"));
            // One cell short: the space is part of what has to fit.
            assert_eq!(tagged_row(glyph, 4, "ab"), format!("{glyph}   "));
            // Narrower still, and there is nothing to argue about.
            assert_eq!(tagged_row(glyph, 3, "ab"), format!("{glyph}  "));
        }
        // A single-width glyph is the same rule with one cell less.
        assert_eq!(tagged_row("x", 4, "ab"), "x ab");
        assert_eq!(tagged_row("x", 3, "ab"), "x  ");
        // An empty row budgets the space too.
        assert_eq!(tagged_row("", 3, "ab"), " ab");
        assert_eq!(tagged_row("", 2, "ab"), "  ");
    }

    /// A rect the layout should never produce, produced anyway.
    #[test]
    fn a_rect_past_the_edge_of_the_buffer_draws_nothing() {
        let mut terminal = Terminal::new(TestBackend::new(10, 3)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                for wide in [
                    Rect::new(0, 0, area.width + 1, 1),
                    Rect::new(area.width, 0, 4, 1),
                    Rect::new(0, area.height, area.width, 1),
                    Rect::new(0, 0, area.width, area.height + 1),
                ] {
                    after_text(frame, true, wide, "tag");
                    border(frame, true, wide, "tag");
                }
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        for y in 0..3 {
            assert_eq!(text_at(buffer, 0, y, 10), " ".repeat(10));
        }
    }

    /// A system message draws neither a header nor a body, and says so.
    #[test]
    fn a_system_message_is_tagged_after_the_branch_that_draws_it() {
        let mut app = state("conversation");
        let joined = crate::archive::Msg::from_api(
            "C1".to_string(),
            serde_json::json!({
                "ts": "1000.000000", "user": "U1", "subtype": "channel_join",
                "text": "<@U1> has joined the channel"
            }),
        )
        .expect("system message");
        app.open.as_mut().expect("open conversation").list =
            crate::app::MsgList::new(vec![joined], false);
        let buffer = drawn(app, 160, 12);
        assert_appended_tag(&buffer, "system_message");
        assert!(find(&buffer, "message_header").is_none());
        assert!(find(&buffer, "message_body").is_none());
    }

    /// The key just pressed, in its own corner popup.
    #[test]
    fn the_last_key_popup_carries_its_own_tag() {
        let mut app = state("conversations");
        app.last_key = Some(("ctrl-shift-alt-x".to_string(), std::time::Instant::now()));
        let buffer = drawn(app, 160, 24);
        let text = border_text("draw_last_key");
        let (x, y) = find(&buffer, &text).expect("draw_last_key is not drawn");
        // The popup is rounded, so its corner is not the panes' corner.
        assert_eq!(symbol(&buffer, x + text.width() as u16, y), "╮");
        assert_eq!(symbol(&buffer, x - 1, y), "─");
    }

    /// Nowhere to put a tag is not a reason to move anything. Twelve columns
    /// leave the messages pane too narrow for its tag on either border, and
    /// every row inside it too full for one.
    #[test]
    fn a_pane_too_narrow_for_a_tag_shows_none_and_moves_no_content() {
        let off = draw(state("collapsed"), 12, 8);
        let on = drawn(state("collapsed"), 12, 8);
        assert_eq!(off, on);
        // The status line is the row with the most text on it; the rule has
        // nowhere to put a tag there either.
        let status = Rect { x: 0, y: 7, width: 12, height: 1 };
        assert_eq!(after_text_at(&off, status, "draw_status"), None);
        for y in 0..8 {
            let row = text_at(&on, 0, y, 12);
            assert!(!row.contains("draw_"), "{row:?}");
        }
    }
}


