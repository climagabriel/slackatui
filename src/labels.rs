//! `/labels`: while it is on, every element on screen carries a short dim tag
//! naming the code that draws it, so a change can be asked for by name.
//!
//! Two placements, and both are applied to the buffer after the element has
//! drawn itself. A bordered element takes the right end of its top border; a
//! borderless one appends the tag after its own text, one space clear of it.
//! Either way the cells the tag would take must be free — border dashes, or
//! blanks — so a tag never covers content, is never truncated to fit, and the
//! element drawn over another one keeps its own: a tag that has nowhere to go
//! is dropped.
//!
//! Nothing here is stored. A list holds its lines, never their tags, and the
//! draw asks for them again each frame.

#[cfg(test)]
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

/// The right end of `area`'s top border, ending one cell before the corner.
/// Dropped unless every cell it wants is still a border dash: a title that
/// reaches that far is content, and content wins.
pub fn border(frame: &mut Frame, on: bool, area: Rect, tag: &str) {
    if !on || area.height == 0 {
        return;
    }
    let text = border_text(tag);
    let width = text.width() as u16;
    // Both corners stay, and there is no point in a tag with no border left.
    if width == 0 || u32::from(width) + 2 > u32::from(area.width) {
        return;
    }
    let corner = area.right() - 1;
    let start = corner - width;
    let y = area.y;
    let buffer = frame.buffer_mut();
    if !(start..corner).all(|x| buffer[(x, y)].symbol() == "─") {
        return;
    }
    buffer.set_string(start, y, &text, style());
}

/// One space past the end of `row`'s own text, inside `row`. Dropped when the
/// whole tag plus that space does not fit, or when the cells are not blank.
pub fn after_text(frame: &mut Frame, on: bool, row: Rect, tag: &str) {
    if !on || row.height == 0 || row.width == 0 {
        return;
    }
    let width = tag.width() as u16;
    if width == 0 {
        return;
    }
    let y = row.y;
    let buffer = frame.buffer_mut();
    // A cell continuing a double-width character has no symbol of its own and
    // is not blank: the text ends after it, not on it.
    let end = (row.x..row.right())
        .rev()
        .find(|&x| buffer[(x, y)].symbol() != " ");
    let start = match end {
        Some(x) => x + 2,
        None => row.x,
    };
    if u32::from(start) + u32::from(width) > u32::from(row.right()) {
        return;
    }
    if !(start..start + width).all(|x| buffer[(x, y)].symbol() == " ") {
        return;
    }
    buffer.set_string(start, y, tag, style());
}

/// Where `after_text` would put `tag` on `row` of `buffer`, or `None` when it
/// would drop it. The tests read the rule from here rather than restating it.
#[cfg(test)]
pub fn after_text_at(buffer: &Buffer, row: Rect, tag: &str) -> Option<u16> {
    let width = tag.width() as u16;
    let y = row.y;
    let end = (row.x..row.right())
        .rev()
        .find(|&x| buffer[(x, y)].symbol() != " ");
    let start = match end {
        Some(x) => x + 2,
        None => row.x,
    };
    (u32::from(start) + u32::from(width) <= u32::from(row.right())
        && (start..start + width).all(|x| buffer[(x, y)].symbol() == " "))
    .then_some(start)
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

    /// A bordered element's tag ends against the corner of its own top border,
    /// with border left of it rather than a title it would have covered.
    fn assert_border_tag(buffer: &Buffer, tag: &str) {
        let text = border_text(tag);
        let (x, y) = find(buffer, &text).unwrap_or_else(|| panic!("{tag} is not drawn"));
        assert_eq!(
            symbol(buffer, x + text.width() as u16, y),
            "┐",
            "{tag} does not end at the corner of its top border"
        );
        assert_eq!(symbol(buffer, x - 1, y), "─", "{tag} covers a title");
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

    /// The panes and the overlays each name themselves on their top border.
    #[test]
    fn a_bordered_element_tags_the_right_end_of_its_top_border() {
        let mut home = state("conversations");
        // A pane whose title already reaches the right end has no room, and
        // drops the tag rather than covering the title.
        home.unreads_first = false;
        let buffer = drawn(home, 160, 24);
        assert_border_tag(&buffer, "draw_convs");
        assert_border_tag(&buffer, "draw_msgs");
        assert_border_tag(&drawn(state("scan-overlay"), 160, 24), "scan_overlay");
        assert_border_tag(&drawn(state("channel-tabs"), 160, 24), "Browser::draw");
        assert_border_tag(&drawn(state("pane-picker"), 160, 24), "draw_pane_menu");
        assert_border_tag(&drawn(state("key-guide"), 160, 40), "draw_help");
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

    /// Nowhere to put a tag is not a reason to move anything.
    #[test]
    fn a_pane_too_narrow_for_a_tag_shows_none_and_moves_no_content() {
        let off = draw(state("narrow"), 24, 10);
        let on = drawn(state("narrow"), 24, 10);
        assert_eq!(off, on);
        // The status line is the row with the most text on it; the rule has
        // nowhere to put a tag there either.
        let status = Rect { x: 0, y: 9, width: 24, height: 1 };
        assert_eq!(after_text_at(&off, status, "draw_status"), None);
        for y in 0..10 {
            let row = text_at(&on, 0, y, 24);
            assert!(!row.contains("draw_"), "{row:?}");
        }
    }
}

