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
/// `message: foo bar` and `message: "foo bar"` mean the same. A `from:@name`
/// left in the remainder still parses: the two filters combine.
pub fn message_needle(query: &str) -> Option<String> {
    let query = query.trim();
    let rest = query.get(..MESSAGE.len()).filter(|head| head.eq_ignore_ascii_case(MESSAGE)).map(|_| query[MESSAGE.len()..].trim())?;
    Some(match rest.strip_prefix('"').and_then(|inner| inner.strip_suffix('"')).filter(|inner| !inner.contains('"')) {
        Some(unquoted) => unquoted.to_string(),
        None => rest.to_string(),
    })
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
        assert_eq!(message_needle("message:").as_deref(), Some(""));
        assert_eq!(message_needle("message: \"\"").as_deref(), Some(""));
        assert_eq!(message_needle("message: \"foo\" \"bar\"").as_deref(), Some("\"foo\" \"bar\""));
        assert_eq!(message_needle("message: \"").as_deref(), Some("\""));
        assert_eq!(message_needle("message: foo from:@me").as_deref(), Some("foo from:@me"));
        assert_eq!(message_needle("me message: foo"), None);
        assert_eq!(message_needle("messages: foo"), None);
        assert_eq!(message_needle("ünicode"), None);
        let parsed = parse(&message_needle("message: foo from:@me").unwrap(), &[], Some("U1")).unwrap();
        assert_eq!(parsed.text, "foo");
        assert_eq!(parsed.slack, "from:U1 foo");
        let text = text_query(" nginx ");
        assert!(text.user_id.is_none());
        assert_eq!((text.text.as_str(), text.slack.as_str()), ("nginx", "nginx"));
    }
}
