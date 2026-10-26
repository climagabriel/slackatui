//! The internal `<workspace>/canvas/-/...` endpoints.
//!
//! The desktop client does not edit a canvas through the public
//! `canvases.edit` method; it POSTs a protobuf change set to
//! `<workspace>/canvas/-/edit-document`, with the session token (`xoxc-`) in
//! the form body and the `d=` cookie beside it. `canvases.edit` refuses that
//! token with `not_allowed_token_type`, which is what stops the interactive
//! `:w` path; these endpoints accept it.
//!
//! Reads work end to end. `files.info` yields the canvas's `quip_thread_id`,
//! and `canvas/-/load-data/editor/1` returns the whole document as protobuf:
//! every section with its id, key, type, edit token and materialised text.
//! The HTML export `canvas.rs` parses does not expose the section key or the
//! edit token, so this is the model the writes are built on.
//!
//! Writes work end to end for text. A change set is one section entry: the
//! section id, its kind, its key, the text as `{1:{1:"text"}}` in field 12,
//! and a `<session>-<sequence>` token. Two shapes were verified against a
//! live canvas: a new section (`append_change`, key suffixed `:temp`) and a
//! replacement of an existing section's text (`set_change`, the section's own
//! key and kind). The browser also sends a serialised rich-text state (field
//! 38 of the entry), a Quip collab blob with embedded SHA-256 hashes; it is
//! not needed for plain text, and no plain-text change was refused for its
//! absence.
//!
//! Escape text before sending it: a section is markdown, so a literal
//! backslash or `*` in the text is markup, and the canvas echoes it escaped
//! on read. The `data_binary` form field is base64 of the change set and is
//! percent-encoded exactly once by the form encoder; pre-quoting it, as a
//! hand-written probe is tempted to do, double-encodes it and the server
//! answers `400 invalid_arguments`.

use serde_json::Value;

use crate::api::Client;

/// One canvas section, as the editor state reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The section's stable id, e.g. `temp:C:AYA5943da8285ae4404ab52a8abd`.
    pub id: String,
    /// The one-letter creation key the document orders sections by.
    pub key: String,
    /// The client's section kind; 5000 titles, 6000+ paragraphs.
    pub kind: u64,
    /// The `<session>-<sequence>` token of the last edit to this section.
    pub token: String,
    /// The section's text, empty for a deleted or non-text section.
    pub text: String,
}

/// A canvas's editable state, decoded from `canvas/-/load-data/editor/1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The internal document id, distinct from the file id.
    pub document: String,
    /// The Quip thread id, also `file.quip_thread_id`.
    pub thread: String,
    /// Every section in document order, deleted ones included.
    pub sections: Vec<Section>,
}

/// What a file id resolves to before any canvas call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The workspace host, e.g. `gcore.slack.com`.
    pub host: String,
    /// The Quip thread id.
    pub thread: String,
    /// The canvas title, echoed back on an edit.
    pub title: String,
}

/// Resolve a canvas file id to the host and thread id its endpoints need.
pub fn target(client: &Client, file_id: &str) -> Result<Target, String> {
    let info = client.call("files.info", &[("file", file_id)])?;
    let file = info
        .get("file")
        .ok_or_else(|| format!("files.info {file_id}: no file in the answer"))?;
    let thread = file
        .get("quip_thread_id")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("{file_id} is not a canvas (files.info has no quip_thread_id)"))?
        .to_string();
    let permalink = file.get("permalink").and_then(Value::as_str).unwrap_or("");
    let host = permalink
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .filter(|h| h.ends_with(".slack.com"))
        .ok_or_else(|| format!("{file_id}: no workspace host in permalink {permalink:?}"))?
        .to_string();
    let title = file
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(Target { host, thread, title })
}

/// The whole editor state as protobuf; `parse_state` decodes it.
pub fn load_state(client: &Client, host: &str, thread: &str) -> Result<Vec<u8>, String> {
    let mut request = field_bytes(1, thread.as_bytes());
    let mut inner = field_varint(1, 1);
    inner.extend(field_bytes(2, thread.as_bytes()));
    request.extend(field_bytes(3, &inner));
    request.extend(field_bytes(5, b"editor"));
    request.extend(field_varint(6, 1));
    let encoded = base64_encode(&request);
    client.internal_post(
        host,
        "/canvas/-/load-data/editor/1",
        &[("request_binary", encoded.as_str())],
    )
}

/// Decode `load-data/editor/1`'s answer. The document sits behind two nested
/// field-2 wrappers (a sync envelope around the document), so the section
/// fields are found by descending until a message carries the metadata
/// (field 6) or the sections (field 7).
pub fn parse_state(bytes: &[u8]) -> Result<State, String> {
    let fields = document_fields(bytes)?;
    let mut document = String::new();
    let mut thread = String::new();
    if let Some(meta) = bytes_field(&fields, 6) {
        let meta = parse_fields(meta)?;
        document = string_field(&meta, 1).unwrap_or_default();
        thread = string_field(&meta, 6).unwrap_or_default();
    }
    let mut sections = Vec::new();
    for (field, value) in &fields {
        if *field != 7 {
            continue;
        }
        let Field::Bytes(raw) = value else { continue };
        let section = parse_fields(raw)?;
        let text = bytes_field(&section, 12)
            .and_then(section_text)
            .unwrap_or_default();
        sections.push(Section {
            id: string_field(&section, 1).unwrap_or_default(),
            kind: varint_field(&section, 2).unwrap_or(0),
            key: string_field(&section, 8).unwrap_or_default(),
            token: string_field(&section, 33).unwrap_or_default(),
            text,
        });
    }
    if document.is_empty() && thread.is_empty() && sections.is_empty() {
        return Err("canvas state: no document id, thread or sections".to_string());
    }
    Ok(State { document, thread, sections })
}

/// Descend the sync envelope to the document: the fields that carry the
/// metadata (6) or the sections (7), found behind up to three nested field-2
/// wrappers.
fn document_fields(bytes: &[u8]) -> Result<Vec<(u64, Field)>, String> {
    let mut current = bytes.to_vec();
    for _ in 0..4 {
        let fields = parse_fields(&current)?;
        let has_document = fields.iter().any(|(f, _)| *f == 6 || *f == 7);
        if has_document {
            return Ok(fields);
        }
        match bytes_field(&fields, 2) {
            Some(next) => current = next.to_vec(),
            None => return Ok(fields),
        }
    }
    Err("canvas state: no document body behind the sync envelope".to_string())
}

/// Build the change set that appends one new section, in the envelope the
/// desktop client sends: the key carries a `:temp` suffix and the kind is
/// left for the server to assign.
pub fn append_change(id: &str, key: &str, text: &str, token: &str) -> Vec<u8> {
    section_change(id, 0, &format!("{key}:temp"), text, token)
}

/// Build the change set that replaces one existing section's text. The
/// section's own key and kind go back unchanged; only field 12 differs.
pub fn set_change(id: &str, kind: u64, key: &str, text: &str, token: &str) -> Vec<u8> {
    section_change(id, kind, key, text, token)
}

fn section_change(id: &str, kind: u64, key: &str, text: &str, token: &str) -> Vec<u8> {
    let content = field_bytes(1, &field_bytes(1, text.as_bytes()));
    let mut entry = Vec::new();
    entry.extend(field_bytes(1, id.as_bytes()));
    entry.extend(field_varint(2, kind));
    if kind == 0 {
        entry.extend(field_varint(3, 0));
    }
    entry.extend(field_bytes(8, key.as_bytes()));
    entry.extend(field_varint(9, 0));
    entry.extend(field_varint(10, 0));
    entry.extend(field_varint(11, 0));
    entry.extend(field_bytes(12, &content));
    entry.extend(field_bytes(13, b""));
    entry.extend(field_bytes(16, &field_varint(4, 0)));
    entry.extend(field_varint(19, 1));
    entry.extend(field_varint(20, 1));
    entry.extend(field_bytes(21, key.as_bytes()));
    entry.extend(field_varint(29, 0));
    entry.extend(field_bytes(33, token.as_bytes()));
    entry.extend(field_varint(35, 0));
    entry.extend(field_varint(39, 0));
    field_bytes(1, &entry)
}

/// Send a change set. The answer is protobuf on success and a JSON `error`
/// body on refusal, surfaced as a string.
pub fn edit_document(
    client: &Client,
    host: &str,
    target: &Target,
    document: &str,
    change: &[u8],
    session: &str,
    sequence: u32,
) -> Result<Vec<u8>, String> {
    let data = base64_encode(change);
    let sequence = sequence.to_string();
    client.internal_post(
        host,
        "/canvas/-/edit-document?_x_version_ts=1792908451",
        &[
            ("thread", target.thread.as_str()),
            ("document", document),
            ("retry_count", "0"),
            ("secret_path", ""),
            ("session", session),
            ("sequence", sequence.as_str()),
            ("nav_action_id", "null"),
            ("search_session_id", "null"),
            ("data_binary", data.as_str()),
            ("title", target.title.as_str()),
        ],
    )
}

/// A new section id in the shape the client assigns: `temp:C:AYA` and 28 hex
/// digits. The server answers an id outside the shape with "Section does not
/// match document", so the shape is fixed here rather than left to callers.
pub fn mint_section_id() -> String {
    let mut out = String::from("temp:C:AYA");
    for _ in 0..28 {
        out.push(std::char::from_digit(next_hex() as u32, 16).unwrap());
    }
    out
}

/// A 12-hex editing-session id, the first half of every section token.
pub fn mint_session() -> String {
    (0..12)
        .map(|_| std::char::from_digit(next_hex() as u32, 16).unwrap())
        .collect()
}

/// The creation key for the section after the last one: `a` + the previous
/// key's last letter + 1, so `aad` follows `aac`.
pub fn next_key(keys: impl Iterator<Item = String>) -> String {
    let last = keys
        .filter(|k| k.len() == 3 && k.starts_with('a') && !k.contains('-'))
        .max();
    match last.and_then(|k| k.as_bytes().last().copied()) {
        Some(b) if b < b'z' => format!("aa{}", (b + 1) as char),
        _ => "aaa".to_string(),
    }
}

/// Standard base64, no padding line breaks. Kept here rather than adding a
/// dependency the rest of the crate does not need.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The section's text: `{1:{1:"text"}}` for a paragraph,
/// `{58:{1:"text"}}` for a title, nothing for a structural marker.
fn section_text(content: &[u8]) -> Option<String> {
    let fields = parse_fields(content).ok()?;
    for field in [1u64, 58] {
        if let Some(inner) = bytes_field(&fields, field) {
            if let Ok(inner) = parse_fields(inner) {
                if let Some(text) = string_field(&inner, 1) {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// One protobuf field: a varint or a length-delimited byte string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(Vec<u8>),
}

fn parse_fields(bytes: &[u8]) -> Result<Vec<(u64, Field)>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let (key, next) = read_varint(bytes, at)?;
        at = next;
        match key & 7 {
            0 => {
                let (value, next) = read_varint(bytes, at)?;
                at = next;
                out.push((key >> 3, Field::Varint(value)));
            }
            2 => {
                let (len, next) = read_varint(bytes, at)?;
                at = next;
                let end = at
                    .checked_add(len as usize)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| "canvas protobuf: field runs past the message".to_string())?;
                out.push((key >> 3, Field::Bytes(bytes[at..end].to_vec())));
                at = end;
            }
            1 | 5 => {
                let width = if key & 7 == 1 { 8 } else { 4 };
                let end = at
                    .checked_add(width)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| "canvas protobuf: fixed field runs past the message".to_string())?;
                let slice = &bytes[at..end];
                at = end;
                if width == 8 {
                    let mut raw = [0u8; 8];
                    raw.copy_from_slice(slice);
                    out.push((key >> 3, Field::Fixed64(u64::from_le_bytes(raw))));
                } else {
                    let mut raw = [0u8; 4];
                    raw.copy_from_slice(slice);
                    out.push((key >> 3, Field::Fixed32(u32::from_le_bytes(raw))));
                }
            }
            other => return Err(format!("canvas protobuf: unsupported wire type {other}")),
        }
    }
    Ok(out)
}

fn read_varint(bytes: &[u8], mut at: usize) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(at)
            .ok_or_else(|| "canvas protobuf: varint runs past the message".to_string())?;
        at += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, at));
        }
        shift += 7;
        if shift > 63 {
            return Err("canvas protobuf: varint is longer than 64 bits".to_string());
        }
    }
}

fn field_bytes(field: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = varint((field << 3) | 2);
    out.extend(varint(bytes.len() as u64));
    out.extend_from_slice(bytes);
    out
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = varint((field << 3) | 0);
    out.extend(varint(value));
    out
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn bytes_field<'a>(fields: &'a [(u64, Field)], field: u64) -> Option<&'a [u8]> {
    fields.iter().find_map(|(f, v)| match v {
        Field::Bytes(b) if *f == field => Some(b.as_slice()),
        _ => None,
    })
}

fn varint_field(fields: &[(u64, Field)], field: u64) -> Option<u64> {
    fields.iter().find_map(|(f, v)| match v {
        Field::Varint(n) if *f == field => Some(*n),
        _ => None,
    })
}

fn string_field(fields: &[(u64, Field)], field: u64) -> Option<String> {
    bytes_field(fields, field).and_then(|b| String::from_utf8(b.to_vec()).ok())
}

/// A fresh hex digit, from the clock and the process id; the ids only have to
/// be unique within one document, not unpredictable.
fn next_hex() -> u8 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ ((std::process::id() as u64) << 32)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 60) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_match_the_protobuf_rules() {
        assert_eq!(varint(0), vec![0]);
        assert_eq!(varint(127), vec![0x7f]);
        assert_eq!(varint(128), vec![0x80, 0x01]);
        assert_eq!(varint(300), vec![0xac, 0x02]);
    }

    #[test]
    fn length_delimited_fields_carry_their_length_and_tag() {
        assert_eq!(field_bytes(1, b"abc"), vec![0x0a, 3, b'a', b'b', b'c']);
        assert_eq!(field_varint(2, 0), vec![0x10, 0x00]);
    }

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn state_decodes_a_title_a_paragraph_and_a_deleted_section() {
        let title = section(
            "temp:C:AYA5943da8285ae4404ab52a8abd",
            5000,
            "aaa",
            &field_bytes(58, &field_bytes(1, b"test")),
            "50297fd600be-10",
        );
        let paragraph = section(
            "temp:C:AYA2f54b8a4e14a4233b248b9e4c",
            7000,
            "aab",
            &field_bytes(1, &field_bytes(1, b"test")),
            "535254662f75-0",
        );
        let mut meta = field_bytes(1, b"AYA9BA7GjbA");
        meta.extend(field_bytes(6, b"AYA9AANQCtL"));
        meta.extend(field_bytes(63, b"AYA9iANQCtL"));
        let mut body = field_bytes(6, &meta);
        body.extend(title);
        body.extend(paragraph);
        let bytes = field_bytes(2, &body);

        let state = parse_state(&bytes).expect("state decodes");
        assert_eq!(state.document, "AYA9BA7GjbA");
        assert_eq!(state.thread, "AYA9AANQCtL");
        assert_eq!(state.sections.len(), 2);
        assert_eq!(state.sections[0].text, "test");
        assert_eq!(state.sections[0].kind, 5000);
        assert_eq!(state.sections[0].key, "aaa");
        assert_eq!(state.sections[0].token, "50297fd600be-10");
        assert_eq!(state.sections[1].text, "test");
        assert_eq!(state.sections[1].kind, 7000);
        assert_eq!(state.sections[1].id, "temp:C:AYA2f54b8a4e14a4233b248b9e4c");
    }

    fn section(id: &str, kind: u64, key: &str, content: &[u8], token: &str) -> Vec<u8> {
        let mut entry = field_bytes(1, id.as_bytes());
        entry.extend(field_varint(2, kind));
        entry.extend(field_bytes(8, key.as_bytes()));
        entry.extend(field_bytes(12, content));
        entry.extend(field_bytes(33, token.as_bytes()));
        field_bytes(7, &entry)
    }

    #[test]
    fn append_change_matches_the_browser_envelope() {
        let change = append_change("temp:C:AYA0f1e2d3c4b5a69788796a5b4c", "aae", "hi", "535254662f75-0");
        let outer = parse_fields(&change).unwrap();
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].0, 1);
        let Field::Bytes(entry) = &outer[0].1 else { panic!("entry is not a message") };
        let entry = parse_fields(entry).unwrap();
        assert_eq!(string_field(&entry, 1).as_deref(), Some("temp:C:AYA0f1e2d3c4b5a69788796a5b4c"));
        assert_eq!(varint_field(&entry, 2), Some(0));
        assert_eq!(string_field(&entry, 8).as_deref(), Some("aae:temp"));
        assert_eq!(string_field(&entry, 21).as_deref(), Some("aae:temp"));
        assert_eq!(string_field(&entry, 33).as_deref(), Some("535254662f75-0"));
        assert_eq!(varint_field(&entry, 19), Some(1));
        assert_eq!(varint_field(&entry, 20), Some(1));
        assert_eq!(varint_field(&entry, 39), Some(0));
        let content = bytes_field(&entry, 12).unwrap();
        assert_eq!(
            section_text(content).as_deref(),
            Some("hi"),
            "the new section's text is field 12.1.1"
        );
    }

    #[test]
    fn set_change_keeps_the_section_key_and_kind() {
        let change = set_change(
            "temp:C:AYA2f54b8a4e14a4233b248b9e4c",
            7000,
            "aab",
            "new text",
            "535254662f75-1",
        );
        let outer = parse_fields(&change).unwrap();
        let Field::Bytes(entry) = &outer[0].1 else { panic!("entry is not a message") };
        let entry = parse_fields(entry).unwrap();
        assert_eq!(varint_field(&entry, 2), Some(7000));
        assert_eq!(string_field(&entry, 8).as_deref(), Some("aab"));
        assert_eq!(string_field(&entry, 21).as_deref(), Some("aab"));
        assert_eq!(
            varint_field(&entry, 3),
            None,
            "an existing section carries no creation marker"
        );
        assert_eq!(
            section_text(bytes_field(&entry, 12).unwrap()).as_deref(),
            Some("new text")
        );
    }

    #[test]
    fn section_text_ignores_a_structural_marker() {
        let marker = field_bytes(10, &field_bytes(3, b"insert"));
        assert_eq!(section_text(&marker), None);
    }

    #[test]
    fn next_key_continues_the_letter_run() {
        let keys = ["aaa", "aab", "aac"].iter().map(|k| k.to_string());
        assert_eq!(next_key(keys), "aad");
        assert_eq!(next_key(std::iter::empty()), "aaa");
    }

    #[test]
    fn minted_ids_have_the_shape_the_server_accepts() {
        let id = mint_section_id();
        assert!(id.starts_with("temp:C:AYA"), "{id}");
        assert_eq!(id.len(), "temp:C:AYA".len() + 28);
        assert!(id["temp:C:AYA".len()..].chars().all(|c| c.is_ascii_hexdigit()));
        let session = mint_session();
        assert_eq!(session.len(), 12);
        assert!(session.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
