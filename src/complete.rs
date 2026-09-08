//! Slash-command completion: the command table, what a half-typed line can
//! still become, and the line a Tab produces.

/// One command as the prompt advertises it.
pub struct Cmd {
    pub name: &'static str,
    /// The argument shape, empty when the command takes none.
    pub args: &'static str,
    pub help: &'static str,
}

/// The canonical names, in the order the suggestions list them. Aliases
/// (`s`, `palette`, ...) still parse; only these complete.
pub const COMMANDS: &[Cmd] = &[
    Cmd { name: "save", args: "", help: "save the selected message to Slack Later" },
    Cmd { name: "unsave", args: "", help: "remove the selected message from Slack Later" },
    Cmd {
        name: "conversations-pane",
        args: "",
        help: "choose visible categories and individual conversations",
    },
    Cmd {
        name: "find",
        args: "TEXT",
        help: "filter the conversation list, or search the open conversation",
    },
    Cmd {
        name: "search",
        args: "TEXT",
        help: "the same as find",
    },
    Cmd {
        name: "leave",
        args: "[#name]",
        help: "leave the channel, the open one by default",
    },
    Cmd { name: "star", args: "[#name]", help: "star a conversation in Slack and keep it on top" },
    Cmd { name: "pin", args: "[#name]", help: "alias for star" },
    Cmd { name: "unstar", args: "[#name]", help: "remove a conversation star in Slack" },
    Cmd { name: "unpin", args: "[#name]", help: "alias for unstar" },
    Cmd {
        name: "mute",
        args: "[#name]",
        help: "keep the channel at the bottom of the list",
    },
    Cmd {
        name: "unmute",
        args: "[#name]",
        help: "undo a mute",
    },
    Cmd {
        name: "cache",
        args: "start|stop|wipe [#name], highlight on|off",
        help: "the hourly archive refresh, or the color cached conversations take",
    },
    Cmd {
        name: "upload",
        args: "[path]",
        help: "send a file with the next message; no path takes the clipboard's image",
    },
    Cmd {
        name: "keys",
        args: "",
        help: "rebind what the keys do in the lists",
    },
    Cmd {
        name: "colorpalette",
        args: "[name]",
        help: "edit and persist the UI colors, from a named palette when given",
    },
    Cmd {
        name: "version",
        args: "",
        help: "show the version in the corner of the status line, or hide it",
    },
];

const CACHE_OPS: &[(&str, &str)] = &[
    ("start", "archive it and refresh it hourly"),
    ("stop", "keep the archive, stop refreshing it"),
    ("wipe", "delete the archive from disk"),
    (
        "highlight",
        "color the cached conversations, on or off, whichever list is shown",
    ),
];

const HIGHLIGHT_ARGS: &[(&str, &str)] = &[
    ("on", "cached conversations in the palette's cached color"),
    ("off", "no color of their own"),
];

/// A completion candidate: the text it inserts, and the line describing it.
#[derive(Debug, PartialEq)]
pub struct Item {
    pub text: String,
    pub help: String,
}

/// What the token under the cursor can become.
#[derive(Debug, PartialEq)]
pub struct Completion {
    /// Byte offset in the line where the token being completed starts.
    pub start: usize,
    /// The token itself, as typed.
    pub token: String,
    pub items: Vec<Item>,
}

/// One line of the command list, with `#`-prefixed conversation names for the
/// commands that take one.
pub fn complete(line: &str, convs: &[String]) -> Completion {
    let body = line.trim_start();
    let body = body.strip_prefix('/').unwrap_or(body);
    let start = line.len() - body.len();
    let token_start = start + body.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let token = line[token_start..].to_string();
    let words: Vec<&str> = body.split_whitespace().collect();
    // Which argument the token is: 0 while the command word itself is typed.
    let position = if body.ends_with(char::is_whitespace) {
        words.len()
    } else {
        words.len().saturating_sub(1)
    };
    let items = match (position, words.first().map(|w| w.to_lowercase())) {
        (0, _) => COMMANDS
            .iter()
            .map(|c| Item {
                text: c.name.to_string(),
                help: if c.args.is_empty() {
                    c.help.to_string()
                } else {
                    format!("{}  {}", c.args, c.help)
                },
            })
            .collect(),
        (1, Some(w)) if w == "cache" => CACHE_OPS
            .iter()
            .map(|(op, help)| Item {
                text: op.to_string(),
                help: help.to_string(),
            })
            .collect(),
        (1, Some(w)) if matches!(w.as_str(), "colorpalette" | "palette" | "colors") => {
            crate::palette::PRESETS
                .iter()
                .map(|p| Item {
                    text: p.name.to_string(),
                    help: p.help.to_string(),
                })
                .collect()
        }
        (1, Some(w)) if matches!(w.as_str(), "leave" | "mute" | "unmute" | "star" | "pin" | "unstar" | "unpin") => conv_items(convs),
        (2, Some(w)) if w == "cache" && words.get(1) == Some(&"highlight") => HIGHLIGHT_ARGS
            .iter()
            .map(|(name, help)| Item {
                text: name.to_string(),
                help: help.to_string(),
            })
            .collect(),
        (2, Some(w)) if w == "cache" => conv_items(convs),
        _ => Vec::new(),
    };
    let want = token.trim_start_matches(['#', '@']).to_lowercase();
    let items = items
        .into_iter()
        .filter(|i| {
            i.text
                .trim_start_matches(['#', '@'])
                .to_lowercase()
                .starts_with(&want)
        })
        .collect();
    Completion {
        start: token_start,
        token,
        items,
    }
}

fn conv_items(convs: &[String]) -> Vec<Item> {
    convs
        .iter()
        .map(|name| Item {
            text: name.clone(),
            help: String::new(),
        })
        .collect()
}

/// The line a Tab produces, or None when the token is already the only
/// candidate. One match completes it; several extend the token to their
/// common prefix, and when that adds nothing they cycle.
pub fn apply(line: &str, convs: &[String]) -> Option<String> {
    let c = complete(line, convs);
    if c.items.is_empty() {
        return None;
    }
    let head = &line[..c.start];
    let rest = &line[c.start + c.token.len()..];
    if c.items.len() == 1 {
        let one = &c.items[0].text;
        // A command that takes arguments gets the space its argument needs.
        let tail = if rest.is_empty() && takes_argument(head, one) {
            " "
        } else {
            ""
        };
        let out = format!("{head}{one}{tail}{rest}");
        return (out != line).then_some(out);
    }
    let shared = common_prefix(&c.items);
    if shared.len() > c.token.len() {
        return Some(format!("{head}{shared}{rest}"));
    }
    let at = c.items.iter().position(|i| i.text == c.token);
    let next = at.map_or(0, |i| (i + 1) % c.items.len());
    Some(format!("{head}{}{rest}", c.items[next].text))
}

/// Whether a completed `word` still leaves something to type, and so earns a
/// trailing space. A conversation name ends the line; a command or a cache
/// operation does not.
fn takes_argument(head: &str, word: &str) -> bool {
    if head.trim().trim_start_matches('/').trim().is_empty() {
        return COMMANDS
            .iter()
            .any(|c| c.name == word && !c.args.is_empty());
    }
    CACHE_OPS.iter().any(|(op, _)| *op == word)
}

fn common_prefix(items: &[Item]) -> String {
    let mut shared = items[0].text.clone();
    for i in &items[1..] {
        while !i.text.starts_with(&shared) {
            shared.pop();
        }
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convs() -> Vec<String> {
        vec!["#team-alpha".to_string(), "#tt-ops-room".to_string()]
    }

    #[test]
    fn a_prefix_narrows_to_its_commands() {
        let c = complete("/mu", &convs());
        assert_eq!(c.token, "mu");
        assert_eq!(
            c.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["mute"]
        );
        assert_eq!(apply("/mu", &convs()).as_deref(), Some("/mute "));
    }

    #[test]
    fn several_matches_extend_then_cycle() {
        // The three commands share only "c", which the token already is.
        assert_eq!(
            apply("/c", &convs()).as_deref(),
            Some("/conversations-pane")
        );
        assert_eq!(
            apply("/conversations-p", &convs()).as_deref(),
            Some("/conversations-pane")
        );
        // start and stop share "st"; a further Tab walks them.
        assert_eq!(
            apply("/cache st", &convs()).as_deref(),
            Some("/cache start")
        );
        assert_eq!(
            apply("/cache start", &convs()).as_deref(),
            Some("/cache start ")
        );
    }

    #[test]
    fn arguments_complete_after_their_command() {
        assert_eq!(apply("/cache s", &convs()).as_deref(), Some("/cache st"));
        assert_eq!(apply("/cache ", &convs()).as_deref(), Some("/cache start"));
        assert_eq!(apply("/cache w", &convs()).as_deref(), Some("/cache wipe "));
        assert_eq!(
            apply("/leave #team", &convs()).as_deref(),
            Some("/leave #team-alpha")
        );
        assert_eq!(
            apply("/cache stop #tt", &convs()).as_deref(),
            Some("/cache stop #tt-ops-room")
        );
    }

    #[test]
    fn a_name_matches_without_its_sigil() {
        let c = complete("/mute team", &convs());
        assert_eq!(
            c.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["#team-alpha"]
        );
    }

    #[test]
    fn highlight_completes_its_on_and_off() {
        assert_eq!(
            apply("/cache high", &convs()).as_deref(),
            Some("/cache highlight ")
        );
        let c = complete("/cache highlight ", &convs());
        assert_eq!(
            c.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["on", "off"]
        );
        assert_eq!(
            apply("/cache highlight o", &convs()).as_deref(),
            Some("/cache highlight on")
        );
    }

    #[test]
    fn the_named_palettes_complete() {
        assert_eq!(
            apply("/colorpalette v", &convs()).as_deref(),
            Some("/colorpalette vintage")
        );
    }

    #[test]
    fn an_unknown_word_offers_nothing() {
        assert!(complete("/zzz", &convs()).items.is_empty());
        assert_eq!(apply("/zzz", &convs()), None);
    }
}
