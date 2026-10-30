//! The network the installer chose, read from the `config.toml` symlink; no HTTP, works offline.

use std::path::Path;

/// `config-<network>.toml` (Linux) or `<network>.toml` (macOS); the name is a path segment, so only `[a-z0-9-]`.
pub fn from_config_link(link: &Path) -> Result<String, String> {
    let target = std::fs::read_link(link).map_err(|e| {
        format!(
            "{} is not a symlink ({e}); cannot tell which network was chosen",
            link.display()
        )
    })?;
    let stem = target.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
        format!(
            "{} points at {} which has no file name",
            link.display(),
            target.display()
        )
    })?;
    let name = stem.strip_prefix("config-").unwrap_or(stem);
    let valid_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    let valid = !name.is_empty() && name.chars().all(valid_char);
    if !valid {
        return Err(format!(
            "{} points at {}; '{name}' is not a network name",
            link.display(),
            target.display()
        ));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn link_to(target: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        symlink(target, &link).unwrap();
        (dir, link)
    }

    #[test]
    fn linux_prefix_is_stripped_and_mac_bare_stem_is_kept() {
        let (_dir, link) = link_to("config-jura.toml");
        assert_eq!(from_config_link(&link).unwrap(), "jura");
        let (_dir, link) = link_to("/etc/gnosisvpn/piz-palu-dev.toml");
        assert_eq!(from_config_link(&link).unwrap(), "piz-palu-dev");
    }

    #[test]
    fn a_regular_file_is_not_a_choice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        assert!(from_config_link(&path).unwrap_err().contains("not a symlink"));
        assert!(from_config_link(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn names_outside_the_charset_are_rejected() {
        for target in [
            "Jura.toml",
            "config-.toml",
            "config-ju ra.toml",
            "jura_1.toml",
            "..toml",
        ] {
            let (_dir, link) = link_to(target);
            assert!(from_config_link(&link).is_err(), "{target}");
        }
    }
}
