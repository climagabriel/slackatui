//! The image on the system clipboard, as a file on disk. The terminal cannot
//! hand a pasted picture to a program, so the clipboard is read beside it,
//! through whichever helper the session has.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a clipboard image may weigh before this reads it as a mistake.
const MAX_CLIPBOARD: u64 = 64 * 1024 * 1024;

/// The helpers, for the message when none of them is installed.
const TOOLS: &[&str] = &["wl-paste", "xclip"];

/// Wayland first, then X11; each is asked for PNG and then JPEG.
const READERS: &[(&str, &[&str], &str)] = &[
    ("wl-paste", &["--no-newline", "--type", "image/png"], "png"),
    ("wl-paste", &["--no-newline", "--type", "image/jpeg"], "jpg"),
    (
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
        "png",
    ),
    (
        "xclip",
        &["-selection", "clipboard", "-t", "image/jpeg", "-o"],
        "jpg",
    ),
];

/// Writes the clipboard's image into `dir` and returns the file.
pub fn image(dir: &Path) -> Result<PathBuf, String> {
    let mut missing = Vec::new();
    let mut refused: Option<String> = None;
    for (tool, args, extension) in READERS {
        let bytes = match read_from(tool, args) {
            Ok(bytes) => bytes,
            Err(Reader::Missing) => {
                if !missing.contains(tool) {
                    missing.push(tool);
                }
                continue;
            }
            Err(Reader::Failed(why)) => {
                refused.get_or_insert(format!("{tool}: {why}"));
                continue;
            }
        };
        if !looks_like_image(&bytes) {
            continue;
        }
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        // Nanoseconds and the process id: two captures in the same second,
        // or two instances, must not write the same file.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "clipboard-{stamp}-{}.{extension}",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        return Ok(path);
    }
    if missing.len() == TOOLS.len() {
        return Err("no clipboard reader: install wl-clipboard or xclip".to_string());
    }
    match refused {
        Some(why) => Err(format!("no image on the clipboard ({why})")),
        None => Err("no image on the clipboard".to_string()),
    }
}

/// Why a reader gave nothing: it is not installed, or it ran and failed.
enum Reader {
    Missing,
    Failed(String),
}

/// The helper's stdout, capped: a helper that answers with something huge
/// must not be read into memory whole.
fn read_from(tool: &str, args: &[&str]) -> Result<Vec<u8>, Reader> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Reader::Missing,
            _ => Reader::Failed(error.to_string()),
        })?;
    let mut bytes = Vec::new();
    if let Some(out) = child.stdout.take() {
        let _ = out.take(MAX_CLIPBOARD).read_to_end(&mut bytes);
    }
    let status = child.wait().map_err(|e| Reader::Failed(e.to_string()))?;
    if !status.success() {
        let mut why = String::new();
        if let Some(err) = child.stderr.take() {
            let mut text = Vec::new();
            let _ = err.take(2048).read_to_end(&mut text);
            why = String::from_utf8_lossy(&text).trim().to_string();
        }
        if why.is_empty() {
            why = format!("exit {}", status.code().unwrap_or(-1));
        }
        return Err(Reader::Failed(why));
    }
    if bytes.is_empty() {
        return Err(Reader::Failed("nothing on the clipboard".to_string()));
    }
    Ok(bytes)
}

/// PNG, JPEG, GIF or WebP by their first bytes: a clipboard helper that
/// answered with text or an error page must not reach Slack.
fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_image_bytes_pass() {
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n\x00\x00"));
        assert!(looks_like_image(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(looks_like_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
        assert!(!looks_like_image(b"<!DOCTYPE html>"));
        assert!(!looks_like_image(b""));
    }
}
