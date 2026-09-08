//! Custom emoji URLs and public image downloads; no Slack credentials.
use std::collections::{BTreeMap, BTreeSet};
use sha1::{Digest, Sha1};

pub type Catalog = BTreeMap<String, String>;

pub fn url<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a str> {
    let mut value = catalog.get(name)?.as_str();
    let mut seen = BTreeSet::new();
    while let Some(alias) = value.strip_prefix("alias:") {
        if !seen.insert(alias) { return None; }
        value = catalog.get(alias)?.as_str();
    }
    allowed_url(value).then_some(value)
}

pub fn allowed_url(url: &str) -> bool {
    let Some(host) = url.strip_prefix("https://").and_then(|rest| rest.split('/').next()) else { return false; };
    ["slack-edge.com", "slack.com"].iter().any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

pub fn image_key(url: &str) -> String {
    format!("emoji-{:x}", Sha1::digest(url.as_bytes()))
}

pub fn decode(path: &std::path::Path) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::open(path).map_err(|e| e.to_string())?
        .with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(512);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

pub fn download(url: &str) -> Result<Vec<u8>, String> {
    if !allowed_url(url) { return Err("unsupported emoji image URL".into()); }
    // A fresh agent carries no session headers or cookies. Reject redirects.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .max_redirects(0).build().into();
    let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err(format!("emoji image: HTTP {}", response.status())); }
    response.body_mut().with_config().limit(4 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_images_are_rejected_before_decoding() {
        let path = std::env::temp_dir().join(format!("slack-emoji-limits-{}.png", std::process::id()));
        image::DynamicImage::new_rgb8(513, 1).save(&path).unwrap();
        assert!(decode(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn aliases_cycles_missing_targets_and_url_boundaries() {
        let catalog = Catalog::from([
            ("original".into(), "https://emoji.slack-edge.com/team/image.png".into()),
            ("alias".into(), "alias:original".into()),
            ("chain".into(), "alias:alias".into()),
            ("cycle".into(), "alias:cycle".into()),
            ("missing".into(), "alias:absent".into()),
        ]);
        assert_eq!(url(&catalog, "chain"), url(&catalog, "original"));
        assert!(url(&catalog, "cycle").is_none());
        assert!(url(&catalog, "missing").is_none());
        for bad in ["http://emoji.slack-edge.com/a", "https://slack-edge.com.evil/a", "https://slack.com@evil/a", "file:///tmp/a"] {
            assert!(!allowed_url(bad));
        }
        assert_ne!(image_key("https://emoji.slack-edge.com/one/a"), image_key("https://emoji.slack-edge.com/two/a"));
    }
}
