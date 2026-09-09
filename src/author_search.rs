//! The query prefixes shared by archive search and Slack queries: the
//! `from:@name` author filter, and the `message:` text search that reaches
//! every conversation from the list.
pub struct Query {
    pub user_id: Option<String>,
    pub text: String,
    pub slack: String,
}

pub fn has_author(query: &str) -> bool {
    query.split_whitespace().any(|word| word.to_lowercase().starts_with("from:@"))
}

/// `message:` asks for a text search rather than a name filter. The needle is
/// the rest of the line, with one pair of wrapping double quotes stripped, so
/// `message: foo bar` and `message: "foo bar"` mean the same. The quotes wrap
/// the phrase, not the line: a `from:@name` beside them is lifted out first,
/// so `message: "foo bar" from:@me` searches `foo bar` for that author. The
/// caller still gets that author back, at the end, for `parse` to resolve.
pub fn message_needle(query: &str) -> Option<String> {
    let query = query.trim();
    let rest = query.get(..MESSAGE.len()).filter(|head| head.eq_ignore_ascii_case(MESSAGE)).map(|_| query[MESSAGE.len()..].trim())?;
    if !has_author(rest) { return Some(unquote(rest)); }
    // Word order inside each group survives; `parse` joins the text the same
    // way, so nothing is lost that the author path would have kept.
    let (author, phrase): (Vec<&str>, Vec<&str>) =
        rest.split_whitespace().partition(|word| word.to_lowercase().starts_with("from:@"));
    let phrase = unquote(&phrase.join(" "));
    Some(if phrase.is_empty() { author.join(" ") } else { format!("{phrase} {}", author.join(" ")) })
}

/// One pair of wrapping double quotes off, and the surrounding blanks with
/// them: a needle of blanks is an empty needle, not a match-everything scan.
fn unquote(text: &str) -> String {
    let text = text.trim();
    match text.strip_prefix('"').and_then(|inner| inner.strip_suffix('"')).filter(|inner| !inner.contains('"')) {
        Some(unquoted) => unquoted.trim().to_string(),
        None => text.to_string(),
    }
}

pub const MESSAGE: &str = "message:";

/// A query with no author filter: the needle goes to the archives and to
/// Slack as it was typed.
pub fn text_query(text: &str) -> Query {
    let text = text.trim().to_string();
    Query { user_id: None, slack: text.clone(), text }
}

pub fn parse(query: &str, users: &[(String, String)], me: Option<&str>) -> Result<Query, String> {
    let mut author = None;
    let mut text = Vec::new();
    for word in query.split_whitespace() {
        if word.to_lowercase().starts_with("from:@") {
            if author.is_some() { return Err("Use one from:@ author filter".into()); }
            author = Some(&word[6..]);
        } else { text.push(word); }
    }
    let name = author.filter(|name| !name.is_empty()).ok_or("Type a name after from:@; Tab completes it")?;
    let user_id = if name.eq_ignore_ascii_case("me") { me.map(str::to_string) } else if let Some((id, _)) = users.iter().find(|(id, _)| id.eq_ignore_ascii_case(name)) {
        Some(id.clone())
    } else {
        let matches: Vec<_> = users.iter().filter(|(_, handle)| handle.to_lowercase() == name.to_lowercase()).collect();
        match matches.as_slice() {
            [(id, _)] => Some(id.clone()),
            [] => return Err(format!("Unknown author @{name}; choose a suggestion with Tab")),
            _ => return Err(format!("Ambiguous author @{name}; choose a user ID from the suggestions")),
        }
    };
    let text = text.join(" ");
    let slack = format!("from:{}{}{}", user_id.as_deref().unwrap_or("me"), if text.is_empty() { "" } else { " " }, text);
    Ok(Query { user_id, text, slack })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_me_names_and_ids_and_preserves_text() {
        let users=vec![("U1".into(),"Gabriel.Clima".into())];
        for query in ["from:@me nginx", "nginx FROM:@gabriel.clima", "from:@U1 nginx"] {
            let q=parse(query,&users,Some("U1")).unwrap();assert_eq!(q.user_id.as_deref(),Some("U1"));assert_eq!(q.text,"nginx");assert_eq!(q.slack,"from:U1 nginx");
        }
        assert_eq!(parse("from:@me",&[],None).unwrap().slack,"from:me");
        assert!(parse("from:@",&users,None).is_err());assert!(parse("from:@unknown",&users,None).is_err());
        assert!(parse("from:@me from:@U1",&users,None).is_err());
        let collision=vec![("U1".into(),"alice".into()),("U2".into(),"u1".into())];
        assert_eq!(parse("from:@U1",&collision,None).unwrap().user_id.as_deref(),Some("U1"));
        let duplicate=vec![("U1".into(),"same".into()),("U2".into(),"Same".into())];
        assert!(parse("from:@same",&duplicate,None).is_err());assert!(parse("from:@U2",&duplicate,None).is_ok());
    }

    #[test]
    fn a_message_prefix_takes_the_rest_of_the_line_and_one_pair_of_quotes() {
        for query in ["message: foo bar", "message:foo bar", "  MESSAGE: \"foo bar\" ", "Message:\"foo bar\""] {
            assert_eq!(message_needle(query).as_deref(), Some("foo bar"), "{query}");
        }
        // Blanks, quoted or not, are an empty needle rather than a scan of
        // everything.
        for query in ["message:", "message:   ", "message: \"\"", "message: \"   \"", "message:\" \""] {
            assert_eq!(message_needle(query).as_deref(), Some(""), "{query}");
        }
        assert_eq!(message_needle("message: \"foo\" \"bar\"").as_deref(), Some("\"foo\" \"bar\""));
        assert_eq!(message_needle("message: \"").as_deref(), Some("\""));
        assert_eq!(message_needle("message: foo from:@me").as_deref(), Some("foo from:@me"));
        assert_eq!(message_needle("me message: foo"), None);
        assert_eq!(message_needle("messages: foo"), None);
        assert_eq!(message_needle("ünicode"), None);
        let parsed = parse(&message_needle("message: foo from:@me").unwrap(), &[], Some("U1")).unwrap();
        assert_eq!(parsed.text, "foo");
        assert_eq!(parsed.slack, "from:U1 foo");
        // The quotes wrap the phrase, so an author beside them, on either
        // side, does not leave them in the needle.
        for query in ["message: \"foo bar\" from:@me", "message: from:@me \"foo bar\"", "message:\"foo bar\" FROM:@me"] {
            // The author token keeps the case it was typed in; `parse` folds it.
            assert!(message_needle(query).unwrap().eq_ignore_ascii_case("foo bar from:@me"), "{query}");
            let parsed = parse(&message_needle(query).unwrap(), &[], Some("U1")).unwrap();
            assert_eq!(parsed.text, "foo bar", "{query}");
            assert_eq!(parsed.user_id.as_deref(), Some("U1"), "{query}");
            assert_eq!(parsed.slack, "from:U1 foo bar", "{query}");
        }
        // An author with no phrase of its own stays an author search.
        assert_eq!(message_needle("message: from:@me").as_deref(), Some("from:@me"));
        assert_eq!(message_needle("message: \"  \" from:@me").as_deref(), Some("from:@me"));
        assert!(parse(&message_needle("message: \"a\" from:@me from:@U1").unwrap(), &[], Some("U1")).is_err());
        let text = text_query(" nginx ");
        assert!(text.user_id.is_none());
        assert_eq!((text.text.as_str(), text.slack.as_str()), ("nginx", "nginx"));
    }
}
