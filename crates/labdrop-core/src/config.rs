use crate::error::{LabDropError, Result};
use crate::i18n::Lang;
use crate::session::SharingStrategy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Persistent operator configuration for LabDrop (`config/labdrop.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabDropConfig {
    /// Language preference: `"auto"`, `"en"`, or `"pt"`.
    pub language: String,
    /// Preferred/default network interface name (e.g. `"wlp2s0"`, `"eth0"`), or `None` for automatic.
    pub preferred_interface: Option<String>,
    /// Default sharing strategy: `"auto"`, `"smb"`, `"webdav"`, or `"http"`.
    pub default_protocol: String,
    /// Preferred HTTP / WebDAV port (default `8080`).
    pub preferred_http_port: u16,
    /// Preferred rootless Samba port on Linux (default `4445`).
    pub preferred_rootless_smb_port: u16,
    /// Whether to delete staged files on session exit by default.
    pub delete_staged_on_exit: bool,
}

impl Default for LabDropConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            preferred_interface: None,
            default_protocol: "auto".to_string(),
            preferred_http_port: 8080,
            preferred_rootless_smb_port: 4445,
            delete_staged_on_exit: true,
        }
    }
}

impl LabDropConfig {
    /// Returns the candidate configuration file paths in priority order:
    /// 1. `<exe_dir>/config/labdrop.toml` (portable USB or local `LabDrop/` package)
    /// 2. `<user_config_dir>/labdrop/labdrop.toml` (`~/.config/labdrop/labdrop.toml` or `%APPDATA%\LabDrop\labdrop.toml`)
    pub fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                paths.push(parent.join("config").join("labdrop.toml"));
            }
        }
        if let Some(user_cfg) = user_config_file_path() {
            if !paths.contains(&user_cfg) {
                paths.push(user_cfg);
            }
        }
        paths
    }

    /// Loads configuration from the first existing candidate path, or returns `Default::default()`.
    pub fn load() -> Self {
        for path in Self::candidate_paths() {
            if path.is_file() {
                if let Ok(cfg) = Self::load_from_path(&path) {
                    return cfg;
                }
            }
        }
        Self::default()
    }

    /// Loads configuration from a specific `labdrop.toml` path.
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|e| LabDropError::IoPath {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(Self::parse_toml(&content))
    }

    /// Parses a minimal TOML configuration string into `LabDropConfig`.
    pub fn parse_toml(content: &str) -> Self {
        let mut cfg = Self::default();
        for line in content.lines() {
            let trimmed = line.split('#').next().unwrap_or("").trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((key_raw, val_raw)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key_raw.trim();
            let val = val_raw.trim().trim_matches('"').trim_matches('\'').trim();

            match key {
                "language" => {
                    if !val.is_empty() {
                        cfg.language = val.to_string();
                    }
                }
                "preferred_interface" => {
                    if val.is_empty() || val.eq_ignore_ascii_case("auto") || val.eq_ignore_ascii_case("none") {
                        cfg.preferred_interface = None;
                    } else {
                        cfg.preferred_interface = Some(val.to_string());
                    }
                }
                "default_protocol" => {
                    if !val.is_empty() {
                        cfg.default_protocol = val.to_string();
                    }
                }
                "preferred_http_port" => {
                    if let Ok(p) = val.parse::<u16>() {
                        cfg.preferred_http_port = p;
                    }
                }
                "preferred_rootless_smb_port" => {
                    if let Ok(p) = val.parse::<u16>() {
                        cfg.preferred_rootless_smb_port = p;
                    }
                }
                "delete_staged_on_exit" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.delete_staged_on_exit = b;
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    /// Formats this configuration as a well-commented `labdrop.toml` document.
    pub fn to_toml_string(&self) -> String {
        let pref_iface = self
            .preferred_interface
            .as_deref()
            .unwrap_or("auto");
        format!(
            "# LabDrop Configuration\n\
             # Place installers or classroom files inside the '../files/' directory for quick access.\n\n\
             language = \"{}\"\n\
             preferred_interface = \"{}\"\n\
             default_protocol = \"{}\"\n\
             preferred_http_port = {}\n\
             preferred_rootless_smb_port = {}\n\
             delete_staged_on_exit = {}\n",
            self.language,
            pref_iface,
            self.default_protocol,
            self.preferred_http_port,
            self.preferred_rootless_smb_port,
            self.delete_staged_on_exit
        )
    }

    /// Saves the configuration to the portable `<exe_dir>/config/labdrop.toml` if writable,
    /// or falls back to the user config directory (`~/.config/labdrop/labdrop.toml`).
    pub fn save(&self) -> Result<PathBuf> {
        let candidates = Self::candidate_paths();
        // Prefer an existing config file first
        for path in &candidates {
            if path.is_file() && self.save_to_path(path).is_ok() {
                return Ok(path.clone());
            }
        }
        // Otherwise try candidate paths in order (portable config/ dir if it exists next to exe, then user config)
        for path in &candidates {
            if let Some(parent) = path.parent() {
                // Only create portable `<exe_dir>/config` automatically if `<exe_dir>/files` exists
                // or `<exe_dir>/config` already exists; otherwise use user config path or fallback.
                let is_portable_layout = parent.is_dir()
                    || parent
                        .parent()
                        .map(|root| root.join("files").is_dir())
                        .unwrap_or(false);
                if is_portable_layout && self.save_to_path(path).is_ok() {
                    return Ok(path.clone());
                }
            }
        }
        if let Some(last) = candidates.last() {
            self.save_to_path(last)?;
            return Ok(last.clone());
        }
        Err(LabDropError::Config(
            "No writable configuration path available".to_string(),
        ))
    }

    /// Saves the configuration to a specific file path, creating parent directories if needed.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| LabDropError::IoPath {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        fs::write(path, self.to_toml_string()).map_err(|e| LabDropError::IoPath {
            path: path.to_path_buf(),
            source: e,
        })
    }

    /// Resolves the configured `Lang`, falling back to environment detection when `"auto"`.
    pub fn resolved_lang(&self, cli_override: Option<&str>) -> Lang {
        if let Some(cli) = cli_override {
            if let Some(l) = Lang::parse_str(cli) {
                return l;
            }
        }
        if let Some(l) = Lang::parse_str(&self.language) {
            return l;
        }
        Lang::detect_from_env()
    }

    /// Resolves the configured `SharingStrategy`.
    pub fn resolved_strategy(&self) -> SharingStrategy {
        match self.default_protocol.trim().to_lowercase().as_str() {
            "smb" | "smbonly" => SharingStrategy::SmbOnly,
            "webdav" => SharingStrategy::WebDavSecondary,
            "http" => SharingStrategy::HttpFallback,
            _ => SharingStrategy::AutoLayered,
        }
    }

    /// Updates `default_protocol` from a `SharingStrategy`.
    pub fn set_strategy(&mut self, strategy: SharingStrategy) {
        self.default_protocol = match strategy {
            SharingStrategy::AutoLayered => "auto".to_string(),
            SharingStrategy::SmbOnly => "smb".to_string(),
            SharingStrategy::WebDavSecondary => "webdav".to_string(),
            SharingStrategy::HttpFallback => "http".to_string(),
        };
    }
}

fn user_config_file_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return Some(PathBuf::from(xdg).join("labdrop").join("labdrop.toml"));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return Some(
                    PathBuf::from(home)
                        .join(".config")
                        .join("labdrop")
                        .join("labdrop.toml"),
                );
            }
        }
        None
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return Some(PathBuf::from(appdata).join("LabDrop").join("labdrop.toml"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn config_roundtrip_preserves_preferred_interface_and_settings() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config").join("labdrop.toml");

        let mut cfg = LabDropConfig {
            preferred_interface: Some("wlp2s0".to_string()),
            language: "pt".to_string(),
            preferred_http_port: 8090,
            ..Default::default()
        };
        cfg.set_strategy(SharingStrategy::WebDavSecondary);

        cfg.save_to_path(&cfg_path).unwrap();
        let loaded = LabDropConfig::load_from_path(&cfg_path).unwrap();
        assert_eq!(loaded.preferred_interface.as_deref(), Some("wlp2s0"));
        assert_eq!(loaded.language, "pt");
        assert_eq!(loaded.resolved_strategy(), SharingStrategy::WebDavSecondary);
        assert_eq!(loaded.preferred_http_port, 8090);
    }

    #[test]
    fn config_auto_interface_parses_as_none() {
        let parsed = LabDropConfig::parse_toml(
            "language = \"auto\"\npreferred_interface = \"auto\"\ndefault_protocol = \"auto\"\n",
        );
        assert_eq!(parsed.preferred_interface, None);
    }
}
