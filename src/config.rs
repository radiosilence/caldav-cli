use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Apple iCloud is the default CalDAV host — it's what most people mean by
/// "my calendar" and it needs no per-user server discovery beyond the standard
/// principal walk. Any other RFC 4791 server (Fastmail, Google, Nextcloud,
/// Radicale) works by overriding `server_url`.
pub const DEFAULT_SERVER_URL: &str = "https://caldav.icloud.com";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub core: CoreConfig,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CoreConfig {
    /// CalDAV server base URL. Defaults to iCloud when unset.
    pub server_url: Option<String>,
    /// Account username — an email address for iCloud and Fastmail.
    pub username: Option<String>,
    /// App-specific password. iCloud rejects the primary account password for
    /// CalDAV; generate one at appleid.apple.com.
    pub app_password: Option<String>,
    /// Calendar new events land in. Unset defers to the server's own default
    /// calendar, which is what the user's calendar app writes to.
    pub calendar: Option<String>,
}

impl Config {
    fn config_dir() -> Result<PathBuf> {
        // Use ~/.config on all platforms for consistency
        let dir = dirs::home_dir()
            .ok_or_else(|| Error::Config("Could not find home directory".into()))?
            .join(".config")
            .join("caldav-cli");
        Ok(dir)
    }

    fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| Error::Config(format!("Failed to parse config: {}", e)))?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        create_private_dir(&dir)?;

        let path = Self::config_path()?;

        // Refuse to write through a symlink — an attacker or mistaken user
        // could redirect the credential file elsewhere. symlink_metadata
        // inspects the link itself, not its target.
        if let Ok(md) = fs::symlink_metadata(&path)
            && md.file_type().is_symlink()
        {
            return Err(Error::Config(format!(
                "Refusing to write config: {} is a symlink",
                path.display()
            )));
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("Failed to serialize config: {}", e)))?;

        // Write to a sibling temp file with 0o600, then rename atomically over
        // the target. This closes the TOCTOU window between writing the
        // password and tightening permissions.
        let tmp_path = path.with_extension("toml.tmp");
        let _ = fs::remove_file(&tmp_path);
        write_private_file(&tmp_path, content.as_bytes()).inspect_err(|_| {
            let _ = fs::remove_file(&tmp_path);
        })?;
        fs::rename(&tmp_path, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            Error::Config(format!("Failed to install config file: {}", e))
        })?;

        Ok(())
    }

    /// Server URL, preferring `CALDAV_SERVER_URL`, then config, then iCloud.
    pub fn get_server_url(&self) -> String {
        std::env::var("CALDAV_SERVER_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.core.server_url.clone())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
    }

    /// Username, preferring the `CALDAV_USERNAME` env var over the config file.
    pub fn get_username(&self) -> Result<String> {
        if let Ok(username) = std::env::var("CALDAV_USERNAME")
            && !username.trim().is_empty()
        {
            return Ok(username);
        }
        self.core.username.clone().ok_or(Error::NotAuthenticated)
    }

    /// App password, preferring the `CALDAV_APP_PASSWORD` env var.
    pub fn get_app_password(&self) -> Result<String> {
        if let Ok(password) = std::env::var("CALDAV_APP_PASSWORD")
            && !password.trim().is_empty()
        {
            return Ok(password);
        }
        self.core
            .app_password
            .clone()
            .ok_or(Error::NotAuthenticated)
    }

    /// Chosen calendar for new events, preferring `CALDAV_CALENDAR`.
    pub fn get_calendar(&self) -> Option<String> {
        std::env::var("CALDAV_CALENDAR")
            .ok()
            .or_else(|| self.core.calendar.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_credentials(&mut self, server_url: String, username: String, app_password: String) {
        self.core.server_url = Some(server_url);
        self.core.username = Some(username);
        self.core.app_password = Some(app_password);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // DirBuilder::mode applies to newly-created directories only. Following up
    // with set_permissions tightens the mode if the directory already existed.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_is_empty() {
        let config = Config::default();
        assert!(config.core.username.is_none());
        assert!(config.core.app_password.is_none());
        assert!(config.core.server_url.is_none());
    }

    #[test]
    fn test_set_credentials() {
        let mut config = Config::default();
        config.set_credentials(
            "https://caldav.example.com".into(),
            "me@example.com".into(),
            "hunter2".into(),
        );
        assert_eq!(config.core.username.as_deref(), Some("me@example.com"));
        assert_eq!(config.core.app_password.as_deref(), Some("hunter2"));
        assert_eq!(
            config.core.server_url.as_deref(),
            Some("https://caldav.example.com")
        );
    }

    #[test]
    fn test_server_url_falls_back_to_icloud() {
        // No env override in this test process, no config value set.
        let config = Config::default();
        assert_eq!(config.get_server_url(), DEFAULT_SERVER_URL);
    }

    #[test]
    fn test_server_url_prefers_config_over_default() {
        let config = Config {
            core: CoreConfig {
                server_url: Some("https://caldav.fastmail.com".into()),
                ..Default::default()
            },
        };
        assert_eq!(config.get_server_url(), "https://caldav.fastmail.com");
    }

    #[test]
    fn test_config_serialize_deserialize() {
        let config = Config {
            core: CoreConfig {
                server_url: Some("https://caldav.icloud.com".into()),
                username: Some("me@icloud.com".into()),
                app_password: Some("abcd-efgh".into()),
                calendar: Some("Personal".into()),
            },
        };
        let toml_str = toml::to_string(&config).unwrap();
        let round: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(round.core.username.as_deref(), Some("me@icloud.com"));
        assert_eq!(round.core.app_password.as_deref(), Some("abcd-efgh"));
        assert_eq!(round.core.calendar.as_deref(), Some("Personal"));
    }
}
