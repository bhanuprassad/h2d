use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const MQTT_PORT: u16 = 8883;
pub const FTP_PORT: u16 = 990;

fn default_mqtt() -> u16 {
    MQTT_PORT
}
fn default_ftp() -> u16 {
    FTP_PORT
}
fn default_ftp_dir() -> String {
    "cache".to_string()
}
fn default_timeout() -> u64 {
    20
}
fn default_orca() -> String {
    "orca-slicer".to_string()
}
fn default_curl() -> String {
    "curl".to_string()
}

/// On-disk config. The access code is never stored here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConfig {
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub access_code_file: String,
    #[serde(default = "default_mqtt")]
    pub mqtt_port: u16,
    #[serde(default = "default_ftp")]
    pub ftp_port: u16,
    #[serde(default = "default_ftp_dir")]
    pub ftp_dir: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_orca")]
    pub orca_bin: String,
    #[serde(default)]
    pub printer_profile: String,
    #[serde(default)]
    pub process_profile: String,
    #[serde(default)]
    pub filament_profiles: Vec<String>,
    #[serde(default = "default_curl")]
    pub curl_bin: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        FileConfig {
            ip: String::new(),
            serial: String::new(),
            access_code_file: String::new(),
            mqtt_port: default_mqtt(),
            ftp_port: default_ftp(),
            ftp_dir: default_ftp_dir(),
            timeout_secs: default_timeout(),
            orca_bin: default_orca(),
            printer_profile: String::new(),
            process_profile: String::new(),
            filament_profiles: Vec::new(),
            curl_bin: default_curl(),
        }
    }
}

/// Flags and environment that override the config file.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub config_path: Option<PathBuf>,
    pub ip: Option<String>,
    pub serial: Option<String>,
    pub access_code_file: Option<PathBuf>,
}

/// Resolved printer connection. Debug output redacts the access code.
pub struct Connection {
    pub ip: String,
    pub serial: String,
    pub access_code: String,
    pub mqtt_port: u16,
    pub ftp_port: u16,
    pub ftp_dir: String,
    pub timeout: Duration,
    pub curl_bin: String,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("ip", &self.ip)
            .field("serial", &self.serial)
            .field("access_code", &"********")
            .field("mqtt_port", &self.mqtt_port)
            .field("ftp_port", &self.ftp_port)
            .field("ftp_dir", &self.ftp_dir)
            .finish()
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("h2d").join("config.toml");
    }
    let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("h2d")
        .join("config.toml")
}

pub fn expand_user(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

pub fn resolve_config_path(overrides: &Overrides) -> PathBuf {
    if let Some(path) = &overrides.config_path {
        return path.clone();
    }
    if let Ok(path) = env::var("H2D_CONFIG")
        && !path.is_empty()
    {
        return expand_user(&path);
    }
    default_config_path()
}

pub fn load_file(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(FileConfig::default());
    }
    Ok(toml::from_str(&text)?)
}

pub fn save_file(path: &Path, config: &FileConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(config)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn prefer(flag: Option<String>, env_key: &str, file_value: String) -> String {
    if let Some(value) = flag
        && !value.is_empty()
    {
        return value;
    }
    if let Ok(value) = env::var(env_key)
        && !value.is_empty()
    {
        return value;
    }
    file_value
}

pub fn load_effective(overrides: &Overrides) -> Result<(PathBuf, FileConfig)> {
    let path = resolve_config_path(overrides);
    let mut file = load_file(&path)?;
    file.ip = prefer(overrides.ip.clone(), "H2D_IP", file.ip);
    file.serial = prefer(overrides.serial.clone(), "H2D_SERIAL", file.serial);
    let code_flag = overrides
        .access_code_file
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    file.access_code_file = prefer(code_flag, "H2D_ACCESS_CODE_FILE", file.access_code_file);
    if let Ok(bin) = env::var("H2D_ORCA")
        && !bin.is_empty()
    {
        file.orca_bin = bin;
    }
    if let Ok(bin) = env::var("H2D_CURL")
        && !bin.is_empty()
    {
        file.curl_bin = bin;
    }
    Ok((path, file))
}

pub fn validate_access_code(code: &str) -> Result<()> {
    if !(4..=32).contains(&code.len()) || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::msg(
            "access code must be 4-32 letters or digits. Read it from the printer under Settings, LAN Only.",
        ));
    }
    Ok(())
}

pub fn read_access_code(path: &Path) -> Result<String> {
    if let Ok(from_env) = env::var("H2D_ACCESS_CODE") {
        let code = from_env.trim().to_string();
        if !code.is_empty() {
            validate_access_code(&code)?;
            return Ok(code);
        }
    }
    if path.as_os_str().is_empty() {
        return Err(Error::msg(
            "no access code file. Set one with `h2d config set --access-code-file`, or export H2D_ACCESS_CODE_FILE.",
        ));
    }
    let text = fs::read_to_string(path).map_err(|err| {
        Error::msg(format!(
            "could not read access code file {}: {err}",
            path.display()
        ))
    })?;
    let code = text.trim().to_string();
    validate_access_code(&code)?;
    warn_if_loose(path);
    Ok(code)
}

fn warn_if_loose(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        eprintln!(
            "warning: {} is readable by group or others (mode {:o}). Run chmod 600 on it.",
            path.display(),
            mode & 0o777
        );
    }
}

pub fn connection_from(file: &FileConfig) -> Result<Connection> {
    if file.ip.trim().is_empty() {
        return Err(Error::msg(
            "printer IP is not set. Pass --ip or run `h2d config set --ip`.",
        ));
    }
    if file.serial.trim().is_empty() {
        return Err(Error::msg(
            "printer serial is not set. Pass --serial or run `h2d config set --serial`.",
        ));
    }
    let code_path = expand_user(file.access_code_file.trim());
    let access_code = read_access_code(&code_path)?;
    let ftp_dir = crate::job::sanitize_ftp_dir(&file.ftp_dir)?;
    let timeout_secs = file.timeout_secs.clamp(3, 120);
    Ok(Connection {
        ip: file.ip.trim().to_string(),
        serial: file.serial.trim().to_string(),
        access_code,
        mqtt_port: file.mqtt_port,
        ftp_port: file.ftp_port,
        ftp_dir,
        timeout: Duration::from_secs(timeout_secs),
        curl_bin: {
            let bin = file.curl_bin.trim();
            if bin.is_empty() {
                default_curl()
            } else {
                bin.to_string()
            }
        },
    })
}

pub fn binary_on_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_skips_the_secret() {
        let dir = std::env::temp_dir().join(format!("h2d-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let config = FileConfig {
            ip: "192.168.1.20".into(),
            serial: "01P00C0TEST".into(),
            access_code_file: dir.join("access.code").to_string_lossy().to_string(),
            printer_profile: "h2d.json".into(),
            ..FileConfig::default()
        };
        save_file(&path, &config).unwrap();
        let loaded = load_file(&path).unwrap();
        assert_eq!(loaded.ip, "192.168.1.20");
        assert_eq!(loaded.serial, "01P00C0TEST");
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("access.code\nsecret")
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn access_code_rejects_punctuation() {
        assert!(validate_access_code("abc12345").is_ok());
        assert!(validate_access_code("bad code").is_err());
        assert!(validate_access_code("quote\"").is_err());
    }
}
