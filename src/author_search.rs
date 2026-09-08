//! Author filters shared by archive search and Slack queries.
pub struct Query {
    pub user_id: Option<String>,
    pub text: String,
    pub slack: String,
}

pub fn has_author(query: &str) -> bool {
    query.split_whitespace().any(|word| word.to_lowercase().starts_with("from:@"))
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
}
