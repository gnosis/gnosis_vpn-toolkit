//! Preserve macOS installer choices across updater-driven installs.
//!
//! The client's distribution package carries installer "choices" (HOPR
//! network: jura/rotsee, log level: info/debug). A GUI install records each
//! selection in `/Library/Logs/GnosisVPN/installer/` and symlinks
//! `/etc/gnosisvpn/config.toml` to the chosen network's config. A CLI
//! `installer(8)` run — which is how this updater installs — applies the
//! Distribution.xml *defaults* instead (jura, debug), whose choice packages
//! overwrite the recorded selections and would silently flip e.g. a rotsee
//! install back to jura.
//!
//! To counteract that, the engine detects the installed choices here and
//! passes `-applyChoiceChangesXML` to `installer(8)`, pinning the selection to
//! what is already on disk. When nothing can be detected the installer runs
//! with its defaults, which matches the postinstall's own fallback (jura).

use std::path::Path;

/// Choice identifiers the distribution package offers per group. Only values
/// from these lists are ever pinned, so unexpected on-disk state can neither
/// inject plist markup nor select a choice the package doesn't have.
const NETWORK_IDS: &[&str] = &["jura", "rotsee"];
const LOGLEVEL_IDS: &[&str] = &["info", "debug"];

/// Installer selections detected from the current installation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallerChoices {
    /// HOPR network the installed client is configured for (`jura`/`rotsee`).
    pub network: Option<String>,
    /// Log level chosen at install time (`info`/`debug`).
    pub loglevel: Option<String>,
}

impl InstallerChoices {
    /// Detect the installed choices from the live system paths.
    pub fn detect() -> Self {
        Self::detect_from(
            &super::paths::config_symlink_path(),
            &super::paths::network_choice_path(),
            &super::paths::loglevel_choice_path(),
        )
    }

    /// Path-parameterized detection (unit-testable).
    ///
    /// The network is taken from the `config.toml` symlink target first — that
    /// is what the client actually runs with — falling back to the choice file
    /// the GUI installer recorded. The log level only exists in its choice
    /// file. Values outside the known identifier lists are discarded.
    pub fn detect_from(config_symlink: &Path, network_choice: &Path, loglevel_choice: &Path) -> Self {
        let network = network_from_symlink(config_symlink)
            .filter(|v| NETWORK_IDS.contains(&v.as_str()))
            .or_else(|| {
                choice_from_file(network_choice, "INSTALLER_CHOICE_NETWORK")
                    .filter(|v| NETWORK_IDS.contains(&v.as_str()))
            });
        let loglevel = choice_from_file(loglevel_choice, "INSTALLER_CHOICE_LOGLEVEL")
            .filter(|v| LOGLEVEL_IDS.contains(&v.as_str()));
        InstallerChoices { network, loglevel }
    }

    /// Render an `installer(8)` choice-changes plist pinning the detected
    /// choices, or `None` when nothing was detected (run with defaults).
    pub fn to_choice_changes_xml(&self) -> Option<String> {
        let mut entries = String::new();
        for (value, ids) in [(&self.network, NETWORK_IDS), (&self.loglevel, LOGLEVEL_IDS)] {
            if let Some(chosen) = value {
                // Deselect the group's other members before selecting the
                // pinned one: Distribution.xml's `exclusiveEnabled` JS keeps a
                // member deselectable only while it is the selected one, so
                // order matters when the pin differs from the default.
                for other in ids.iter().filter(|id| *id != chosen) {
                    entries.push_str(&choice_dict(other, false));
                }
                entries.push_str(&choice_dict(chosen, true));
            }
        }
        if entries.is_empty() {
            return None;
        }
        Some(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<array>\n{entries}</array>\n</plist>\n"
        ))
    }
}

fn choice_dict(id: &str, selected: bool) -> String {
    format!(
        "  <dict>\n    \
         <key>choiceIdentifier</key>\n    <string>{id}</string>\n    \
         <key>choiceAttribute</key>\n    <string>selected</string>\n    \
         <key>attributeSetting</key>\n    <integer>{}</integer>\n  </dict>\n",
        u8::from(selected)
    )
}

/// Network name from the `config.toml` symlink target (`rotsee.toml` →
/// `rotsee`). `None` if the path is not a symlink (a regular `config.toml` is
/// a user-managed config the postinstall preserves as-is).
fn network_from_symlink(config_symlink: &Path) -> Option<String> {
    let target = std::fs::read_link(config_symlink).ok()?;
    Some(target.file_stem()?.to_str()?.to_string())
}

/// Parse `KEY="value"` (quotes optional) out of an installer choice file such
/// as `network_choice`. The files are one-line shell fragments written by the
/// choice packages' postinstall scripts.
fn choice_from_file(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(raw) = line.trim().strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            let value = raw.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gnosis_vpn-update-choices-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_choice(path: &Path, line: &str) {
        std::fs::write(path, format!("{line}\n")).unwrap();
    }

    #[test]
    fn detects_network_from_config_symlink_over_choice_file() {
        let dir = TempDir::new("symlink-wins");
        let symlink = dir.path("config.toml");
        std::os::unix::fs::symlink("rotsee.toml", &symlink).unwrap();
        let network_choice = dir.path("network_choice");
        write_choice(&network_choice, "INSTALLER_CHOICE_NETWORK=\"jura\"");

        let choices = InstallerChoices::detect_from(&symlink, &network_choice, &dir.path("missing"));
        assert_eq!(choices.network.as_deref(), Some("rotsee"));
        assert_eq!(choices.loglevel, None);
    }

    #[test]
    fn falls_back_to_choice_file_without_symlink() {
        let dir = TempDir::new("choice-fallback");
        let network_choice = dir.path("network_choice");
        write_choice(&network_choice, "INSTALLER_CHOICE_NETWORK=\"rotsee\"");
        let loglevel_choice = dir.path("loglevel_choice");
        write_choice(&loglevel_choice, "INSTALLER_CHOICE_LOGLEVEL=info");

        let choices = InstallerChoices::detect_from(&dir.path("config.toml"), &network_choice, &loglevel_choice);
        assert_eq!(choices.network.as_deref(), Some("rotsee"));
        assert_eq!(choices.loglevel.as_deref(), Some("info"));
    }

    #[test]
    fn regular_file_config_is_not_treated_as_symlink() {
        let dir = TempDir::new("regular-config");
        let config = dir.path("config.toml");
        std::fs::write(&config, "# user managed\n").unwrap();

        let choices = InstallerChoices::detect_from(&config, &dir.path("missing"), &dir.path("missing2"));
        assert_eq!(choices, InstallerChoices::default());
    }

    #[test]
    fn rejects_unknown_choice_values() {
        let dir = TempDir::new("unknown-values");
        let symlink = dir.path("config.toml");
        std::os::unix::fs::symlink("custom.toml", &symlink).unwrap();
        let network_choice = dir.path("network_choice");
        write_choice(&network_choice, "INSTALLER_CHOICE_NETWORK=\"devnet\"");
        let loglevel_choice = dir.path("loglevel_choice");
        write_choice(&loglevel_choice, "INSTALLER_CHOICE_LOGLEVEL=\"trace\"");

        let choices = InstallerChoices::detect_from(&symlink, &network_choice, &loglevel_choice);
        assert_eq!(choices, InstallerChoices::default());
    }

    #[test]
    fn choice_changes_xml_deselects_default_before_selecting_pin() {
        let choices = InstallerChoices {
            network: Some("rotsee".into()),
            loglevel: Some("info".into()),
        };
        let xml = choices.to_choice_changes_xml().expect("xml");

        // Each group: the non-pinned member is deselected before the pinned
        // one is selected (required by the exclusiveEnabled JS).
        let jura_off = xml.find("<string>jura</string>").expect("jura entry");
        let rotsee_on = xml.find("<string>rotsee</string>").expect("rotsee entry");
        assert!(jura_off < rotsee_on);
        let debug_off = xml.find("<string>debug</string>").expect("debug entry");
        let info_on = xml.find("<string>info</string>").expect("info entry");
        assert!(debug_off < info_on);

        // Exactly one selection per group is turned on.
        assert_eq!(xml.matches("<integer>1</integer>").count(), 2);
        assert_eq!(xml.matches("<integer>0</integer>").count(), 2);
    }

    #[test]
    fn choice_changes_xml_skips_undetected_groups() {
        let choices = InstallerChoices {
            network: Some("jura".into()),
            loglevel: None,
        };
        let xml = choices.to_choice_changes_xml().expect("xml");
        assert!(xml.contains("<string>jura</string>"));
        assert!(!xml.contains("info"));
        assert!(!xml.contains("debug"));

        assert_eq!(InstallerChoices::default().to_choice_changes_xml(), None);
    }

    #[test]
    fn choice_file_parsing_handles_quoting_and_garbage() {
        let dir = TempDir::new("parse");
        let f = dir.path("network_choice");

        write_choice(&f, "INSTALLER_CHOICE_NETWORK=\"rotsee\"");
        assert_eq!(
            choice_from_file(&f, "INSTALLER_CHOICE_NETWORK").as_deref(),
            Some("rotsee")
        );

        write_choice(&f, "INSTALLER_CHOICE_NETWORK=jura");
        assert_eq!(
            choice_from_file(&f, "INSTALLER_CHOICE_NETWORK").as_deref(),
            Some("jura")
        );

        write_choice(&f, "SOMETHING_ELSE=\"rotsee\"");
        assert_eq!(choice_from_file(&f, "INSTALLER_CHOICE_NETWORK"), None);

        write_choice(&f, "INSTALLER_CHOICE_NETWORK=\"\"");
        assert_eq!(choice_from_file(&f, "INSTALLER_CHOICE_NETWORK"), None);

        assert_eq!(choice_from_file(&dir.path("missing"), "INSTALLER_CHOICE_NETWORK"), None);
    }
}
