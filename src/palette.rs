//! Persistent semantic colors for the terminal UI.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};

pub const ROLE_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Role {
    Background,
    Accent,
    InactiveAccent,
    OwnUsername,
    OtherUsername,
    Unread,
    Cached,
    Mention,
    Link,
    Code,
    ThreadInfo,
    Status,
    SelectionText,
    SelectionBackground,
    InactiveSelectionBackground,
    /// The box a running /find draws its progress into.
    ProgressOverlay,
}

pub const ROLES: [Role; ROLE_COUNT] = [
    Role::Background,
    Role::Accent,
    Role::InactiveAccent,
    Role::OwnUsername,
    Role::OtherUsername,
    Role::Unread,
    Role::Cached,
    Role::Mention,
    Role::Link,
    Role::Code,
    Role::ThreadInfo,
    Role::Status,
    Role::SelectionText,
    Role::SelectionBackground,
    Role::InactiveSelectionBackground,
    Role::ProgressOverlay,
];

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Background => "background",
            Role::Accent => "accent / focus",
            Role::InactiveAccent => "inactive focus",
            Role::OwnUsername => "my username",
            Role::OtherUsername => "other usernames",
            Role::Unread => "unread",
            Role::Cached => "cached conversations",
            Role::Mention => "mentions",
            Role::Link => "links",
            Role::Code => "code",
            Role::ThreadInfo => "thread information",
            Role::Status => "status / warnings",
            Role::SelectionText => "selected text",
            Role::SelectionBackground => "selected row",
            Role::InactiveSelectionBackground => "inactive selected row",
            Role::ProgressOverlay => "search progress box",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Role::Background => "background",
            Role::Accent => "accent",
            Role::InactiveAccent => "inactive_accent",
            Role::OwnUsername => "own_username",
            Role::OtherUsername => "other_username",
            Role::Unread => "unread",
            Role::Cached => "cached",
            Role::Mention => "mention",
            Role::Link => "link",
            Role::Code => "code",
            Role::ThreadInfo => "thread_info",
            Role::Status => "status",
            Role::SelectionText => "selection_text",
            Role::SelectionBackground => "selection_background",
            Role::InactiveSelectionBackground => "inactive_selection_background",
            Role::ProgressOverlay => "progress_overlay",
        }
    }

    pub fn default_color(self) -> Color {
        match self {
            // The terminal's own background, until a palette paints one.
            Role::Background => Color::Reset,
            Role::Accent | Role::Mention => Color::Cyan,
            Role::InactiveAccent => Color::DarkGray,
            Role::OwnUsername => Color::LightMagenta,
            Role::OtherUsername => Color::LightBlue,
            Role::Unread => Color::LightYellow,
            Role::Cached => Color::LightGreen,
            Role::Link => Color::Blue,
            Role::Code | Role::ThreadInfo | Role::Status => Color::Yellow,
            Role::SelectionText => Color::Black,
            Role::SelectionBackground => Color::White,
            Role::InactiveSelectionBackground => Color::Gray,
            Role::ProgressOverlay => Color::DarkGray,
        }
    }
}

pub const COLORS: [(&str, Color); 16] = [
    ("black", Color::Black),
    ("dark gray", Color::DarkGray),
    ("gray", Color::Gray),
    ("white", Color::White),
    ("red", Color::Red),
    ("light red", Color::LightRed),
    ("green", Color::Green),
    ("light green", Color::LightGreen),
    ("yellow", Color::Yellow),
    ("light yellow", Color::LightYellow),
    ("blue", Color::Blue),
    ("light blue", Color::LightBlue),
    ("magenta", Color::Magenta),
    ("light magenta", Color::LightMagenta),
    ("cyan", Color::Cyan),
    ("light cyan", Color::LightCyan),
];

/// A named set of colors, applied over the defaults.
pub struct Preset {
    pub name: &'static str,
    pub help: &'static str,
    pub colors: &'static [(Role, Color)],
}

const VINTAGE_TERRACOTTA: Color = Color::Rgb(0xaa, 0x59, 0x53);
const VINTAGE_AMBER: Color = Color::Rgb(0xd3, 0x9b, 0x49);
const VINTAGE_SAND: Color = Color::Rgb(0xe9, 0xd9, 0x9f);
const VINTAGE_OLIVE: Color = Color::Rgb(0x82, 0x83, 0x69);
const VINTAGE_SLATE: Color = Color::Rgb(0x4b, 0x63, 0x69);
/// Text drawn on the light selection bar, not a background of its own.
const VINTAGE_INK: Color = Color::Rgb(0x16, 0x16, 0x16);

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "default",
        help: "the sixteen terminal colors, on the terminal's own background",
        colors: &[],
    },
    Preset {
        name: "vintage",
        help: "terracotta, amber, sand, olive and slate, over the terminal's own background",
        colors: &[
            (Role::Accent, VINTAGE_AMBER),
            (Role::InactiveAccent, VINTAGE_SLATE),
            (Role::OwnUsername, VINTAGE_TERRACOTTA),
            (Role::OtherUsername, VINTAGE_OLIVE),
            (Role::Unread, VINTAGE_SAND),
            (Role::Cached, VINTAGE_OLIVE),
            (Role::Mention, VINTAGE_AMBER),
            (Role::Link, VINTAGE_SLATE),
            (Role::Code, VINTAGE_SAND),
            (Role::ThreadInfo, VINTAGE_OLIVE),
            (Role::Status, VINTAGE_AMBER),
            (Role::SelectionText, VINTAGE_INK),
            (Role::SelectionBackground, VINTAGE_SAND),
            (Role::InactiveSelectionBackground, VINTAGE_OLIVE),
        ],
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Highlight {
    pub word: String,
    pub color: Color,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub highlights: Vec<Highlight>,
    colors: [Color; ROLE_COUNT],
}

impl Default for Palette {
    fn default() -> Self {
        let mut colors = [Color::Reset; ROLE_COUNT];
        for role in ROLES {
            colors[role as usize] = role.default_color();
        }
        Self {
            colors,
            highlights: vec![Highlight {
                word: "nginx".into(),
                color: Color::Green,
            }],
        }
    }
}

impl Palette {
    /// The palette a preset names, or None when no preset has that name.
    pub fn preset(name: &str) -> Option<Palette> {
        let wanted = name.trim().to_lowercase();
        let preset = PRESETS.iter().find(|p| p.name == wanted)?;
        let mut palette = Palette::default();
        for (role, color) in preset.colors {
            palette.set(*role, *color);
        }
        Some(palette)
    }

    pub fn get(&self, role: Role) -> Color {
        self.colors[role as usize]
    }

    pub fn set(&mut self, role: Role, color: Color) {
        self.colors[role as usize] = color;
    }

    pub fn reset(&mut self, role: Role) {
        self.set(role, role.default_color());
    }

    pub fn cycle(&mut self, role: Role, delta: isize) {
        let at = COLORS
            .iter()
            .position(|(_, color)| *color == self.get(role))
            .unwrap_or(0) as isize;
        let next = (at + delta).rem_euclid(COLORS.len() as isize) as usize;
        self.set(role, COLORS[next].1);
    }

    /// A color as it is typed and stored: one of the sixteen names, or
    /// `#rrggbb` for anything else the terminal can show.
    pub fn color_name(&self, role: Role) -> String {
        color_name(self.get(role))
    }

    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let object = serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let object = object
            .as_object()
            .ok_or_else(|| format!("{}: expected a JSON object", path.display()))?;
        let mut palette = Self::default();
        for role in ROLES {
            let Some(value) = object.get(role.key()) else {
                continue;
            };
            let name = value.as_str().ok_or_else(|| {
                format!("{}: {} must be a color name", path.display(), role.key())
            })?;
            let color = parse_color(name)
                .ok_or_else(|| format!("{}: unknown color {name:?}", path.display()))?;
            palette.set(role, color);
        }
        if let Some(rules) = object.get("highlights") {
            let rules = rules.as_array().ok_or("highlights must be a list")?;
            palette.highlights.clear();
            for rule in rules {
                let word = rule
                    .get("word")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("highlight word must be nonempty text")?;
                let color = rule
                    .get("color")
                    .and_then(Value::as_str)
                    .and_then(parse_color)
                    .ok_or("highlight color must be a color name or hex")?;
                palette.highlights.push(Highlight {
                    word: word.to_string(),
                    color,
                });
            }
        }
        Ok(palette)
    }

    /// Literal, case-insensitive substrings; the first rule wins overlaps.
    /// Keep the original bytes and styles, even across span boundaries.
    pub fn highlight_line<'a>(&self, mut line: Line<'a>) -> Line<'a> {
        if self.highlights.is_empty() {
            return line;
        }
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let mut folded = String::new();
        let mut boundaries = vec![Some(0)];
        for (offset, character) in text.char_indices() {
            folded.extend(
                character
                    .to_lowercase()
                    .map(|c| if c == 'ς' { 'σ' } else { c }),
            );
            boundaries.resize(folded.len() + 1, None);
            boundaries[folded.len()] = Some(offset + character.len_utf8());
        }
        let mut colors = vec![None; text.len()];
        for rule in &self.highlights {
            let needle: String = rule
                .word
                .chars()
                .flat_map(char::to_lowercase)
                .map(|c| if c == 'ς' { 'σ' } else { c })
                .collect();
            if needle.is_empty() {
                continue;
            }
            // Search from every character boundary so repeated matches can overlap.
            for (start, _) in folded.char_indices() {
                if !folded[start..].starts_with(&needle) {
                    continue;
                }
                if let (Some(start), Some(end)) =
                    (boundaries[start], boundaries[start + needle.len()])
                {
                    for color in &mut colors[start..end] {
                        color.get_or_insert(rule.color);
                    }
                }
            }
        }
        if colors.iter().all(Option::is_none) {
            return line;
        }
        let mut spans = Vec::new();
        let mut offset = 0;
        for span in line.spans {
            let content = span.content.as_ref();
            let mut start = 0;
            while start < content.len() {
                let color = colors[offset + start];
                let end = content[start..]
                    .char_indices()
                    .skip(1)
                    .find_map(|(index, _)| {
                        (colors[offset + start + index] != color).then_some(start + index)
                    })
                    .unwrap_or(content.len());
                let mut style = span.style;
                if let Some(color) = color {
                    style = style.fg(color);
                }
                spans.push(Span::styled(content[start..end].to_string(), style));
                start = end;
            }
            offset += content.len();
        }
        line.spans = spans;
        line
    }

    pub fn save(&self, path: Option<&Path>) -> Result<PathBuf, String> {
        let path = path.ok_or_else(|| {
            "no configuration directory; set SLACK_TUI_PALETTE to a file".to_string()
        })?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let mut object = Map::new();
        object.insert("version".to_string(), Value::from(1));
        for role in ROLES {
            object.insert(role.key().to_string(), Value::from(self.color_name(role)));
        }
        object.insert("highlights".into(), Value::Array(self.highlights.iter().map(|rule|
            serde_json::json!({"word": rule.word, "color": color_name(rule.color)})
        ).collect()));
        let text = serde_json::to_string_pretty(&object).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&temporary, format!("{text}\n"))
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("{}: {error}", path.display()));
        }
        Ok(path.to_path_buf())
    }
}

pub fn color_name(color: Color) -> String {
    if color == Color::Reset {
        return TERMINAL_DEFAULT.to_string();
    }
    if let Some(name) = COLORS
        .iter()
        .find_map(|(name, value)| (*value == color).then_some(*name))
    {
        return name.to_string();
    }
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// The name of the terminal's own color, which `d` puts back under the
/// background role and which no palette file may lose.
pub const TERMINAL_DEFAULT: &str = "terminal";

/// A color name, `terminal`, or `#rgb` / `#rrggbb`.
pub fn parse_color(name: &str) -> Option<Color> {
    let text = name.trim();
    if let Some(hex) = text.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<Vec<u8>>>()?;
        return match digits.len() {
            3 => Some(Color::Rgb(digits[0] * 17, digits[1] * 17, digits[2] * 17)),
            6 => Some(Color::Rgb(
                digits[0] * 16 + digits[1],
                digits[2] * 16 + digits[3],
                digits[4] * 16 + digits[5],
            )),
            _ => None,
        };
    }
    let normalized = text.to_lowercase().replace(['-', '_'], " ");
    if normalized == TERMINAL_DEFAULT {
        return Some(Color::Reset);
    }
    COLORS
        .iter()
        .find_map(|(known, color)| (*known == normalized).then_some(*color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "slack-tui-{label}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn greek_sigma_uses_the_same_folding_in_rules_and_messages() {
        let mut palette = Palette::default();
        palette.highlights = vec![Highlight {
            word: "ΟΣ".into(),
            color: Color::Green,
        }];
        let line = palette.highlight_line(Line::from("ΟΣ ος οσ"));
        assert_eq!(line.to_string(), "ΟΣ ος οσ");
        let green: String = line
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::Green))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(green, "ΟΣοςοσ");
    }

    #[test]
    fn highlights_preserve_text_unicode_styles_and_span_boundaries() {
        use ratatui::style::{Modifier, Style};
        let mut palette = Palette::default();
        palette.highlights.push(Highlight {
            word: "écho".into(),
            color: Color::Red,
        });
        let style = Style::new().bg(Color::White).add_modifier(Modifier::BOLD);
        let input = Line::from(vec![
            Span::styled("İ #team-NG", style),
            Span::styled("INX-fork ÉCHO nginx", style),
        ]);
        let output = palette.highlight_line(input.clone());
        assert_eq!(output.to_string(), input.to_string());
        for span in &output.spans {
            assert_eq!(span.style.bg, Some(Color::White));
            assert!(span.style.add_modifier.contains(Modifier::BOLD));
        }
        let green: String = output
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Green))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(green, "NGINXnginx");
        let red: String = output
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Red))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(red, "ÉCHO");
        palette.highlights.insert(
            0,
            Highlight {
                word: "ng".into(),
                color: Color::Blue,
            },
        );
        let output = palette.highlight_line(Line::from("nginx"));
        assert_eq!(output.spans[0].content, "ng");
        assert_eq!(output.spans[0].style.fg, Some(Color::Blue));
        assert_eq!(output.spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn highlights_migrate_old_config_and_preserve_explicit_empty_rules() {
        let path = temporary_file("highlights");
        std::fs::write(&path, "{}").unwrap();
        let mut palette = Palette::load(Some(&path)).unwrap();
        assert_eq!(palette.highlights[0].word, "nginx");
        assert_eq!(palette.highlights[0].color, Color::Green);
        palette.highlights.push(Highlight {
            word: "ÉCHO".into(),
            color: Color::Rgb(1, 2, 3),
        });
        palette.save(Some(&path)).unwrap();
        assert_eq!(palette, Palette::load(Some(&path)).unwrap());
        palette.highlights.clear();
        palette.save(Some(&path)).unwrap();
        assert!(Palette::load(Some(&path)).unwrap().highlights.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn own_username_and_unread_have_distinct_defaults() {
        let palette = Palette::default();
        assert_eq!(palette.get(Role::OwnUsername), Color::LightMagenta);
        assert_eq!(palette.get(Role::Unread), Color::LightYellow);
        assert_ne!(palette.get(Role::OwnUsername), palette.get(Role::Unread));
    }

    #[test]
    fn palette_round_trips_and_missing_roles_keep_their_defaults() {
        let path = temporary_file("round-trip");
        let mut palette = Palette::default();
        palette.set(Role::OwnUsername, Color::Green);
        palette.set(Role::Unread, Color::LightRed);
        palette.save(Some(&path)).unwrap();
        assert_eq!(Palette::load(Some(&path)).unwrap(), palette);

        std::fs::write(&path, "{\"own_username\":\"light-cyan\"}\n").unwrap();
        let partial = Palette::load(Some(&path)).unwrap();
        assert_eq!(partial.get(Role::OwnUsername), Color::LightCyan);
        assert_eq!(partial.get(Role::Unread), Role::Unread.default_color());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn typed_colors_take_names_and_hex() {
        assert_eq!(parse_color(" Light-Blue "), Some(Color::LightBlue));
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_color("#f80"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_color("#ff88"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(color_name(Color::Rgb(255, 136, 0)), "#ff8800");
        assert_eq!(color_name(Color::LightBlue), "light blue");
        assert_eq!(color_name(Color::Reset), "terminal");
        assert_eq!(parse_color("Terminal"), Some(Color::Reset));

        let path = temporary_file("hex");
        let mut palette = Palette::default();
        palette.set(Role::OwnUsername, Color::Rgb(1, 2, 3));
        palette.save(Some(&path)).unwrap();
        assert_eq!(Palette::load(Some(&path)).unwrap(), palette);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_vintage_preset_paints_every_role() {
        let vintage = Palette::preset("Vintage ").expect("a preset named vintage");
        assert_eq!(vintage.get(Role::Accent), VINTAGE_AMBER);
        assert_ne!(vintage, Palette::default());
        // The terminal keeps its own background; every other role is painted.
        assert_eq!(vintage.get(Role::Background), Color::Reset);
        for role in ROLES.iter().filter(|r| **r != Role::Background) {
            assert_ne!(vintage.get(*role), Color::Reset, "{}", role.label());
        }
        assert_eq!(Palette::preset("default"), Some(Palette::default()));
        assert_eq!(Palette::preset("sepia"), None);

        // A preset survives the round trip through the file.
        let path = temporary_file("preset");
        vintage.save(Some(&path)).unwrap();
        assert_eq!(Palette::load(Some(&path)).unwrap(), vintage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cycling_wraps_and_invalid_colors_are_rejected() {
        let mut palette = Palette::default();
        palette.set(Role::Accent, COLORS[0].1);
        palette.cycle(Role::Accent, -1);
        assert_eq!(palette.get(Role::Accent), COLORS[COLORS.len() - 1].1);
        palette.cycle(Role::Accent, 1);
        assert_eq!(palette.get(Role::Accent), COLORS[0].1);

        let path = temporary_file("invalid");
        std::fs::write(&path, "{\"accent\":\"ultraviolet\"}\n").unwrap();
        let error = Palette::load(Some(&path)).unwrap_err();
        assert!(error.contains("unknown color"), "{error}");
        std::fs::remove_file(path).unwrap();
    }
}
