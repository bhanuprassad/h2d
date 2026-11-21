use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::{self, FileConfig};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub level: String,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            level: "ok".into(),
            detail: detail.into(),
        }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            level: "warn".into(),
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            level: "fail".into(),
            detail: detail.into(),
        }
    }
}

pub fn doctor(path: &Path, file: &FileConfig) -> Vec<Check> {
    let mut checks = Vec::new();
    if path.is_file() {
        checks.push(Check::ok("config", format!("reading {}", path.display())));
    } else {
        checks.push(Check::warn(
            "config",
            format!(
                "no file at {}. Flags and environment still apply.",
                path.display()
            ),
        ));
    }

    if file.ip.trim().is_empty() {
        checks.push(Check::fail("ip", "not set"));
    } else {
        checks.push(Check::ok("ip", file.ip.trim().to_string()));
    }
    if file.serial.trim().is_empty() {
        checks.push(Check::fail("serial", "not set"));
    } else {
        checks.push(Check::ok("serial", file.serial.trim().to_string()));
    }

    checks.push(access_code_check(file));

    if file.ip.trim().is_empty() {
        checks.push(Check::fail("mqtt", "skipped, IP is not set"));
        checks.push(Check::fail("ftp", "skipped, IP is not set"));
    } else {
        checks.push(tcp_check("mqtt", &file.ip, file.mqtt_port));
        checks.push(tcp_check("ftp", &file.ip, file.ftp_port));
    }

    match config::binary_on_path(file.curl_bin.trim()) {
        Some(bin) => checks.push(Check::ok("curl", bin.display().to_string())),
        None => checks.push(Check::fail(
            "curl",
            format!(
                "'{}' not found. Uploads need curl for implicit FTPS.",
                file.curl_bin
            ),
        )),
    }
    match config::binary_on_path(file.orca_bin.trim()) {
        Some(bin) => checks.push(Check::ok("orca", bin.display().to_string())),
        None => checks.push(Check::warn(
            "orca",
            format!(
                "'{}' not found. Printing an existing .gcode.3mf does not need it.",
                file.orca_bin
            ),
        )),
    }
    checks
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.level == "fail")
}

fn access_code_check(file: &FileConfig) -> Check {
    if std::env::var("H2D_ACCESS_CODE")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return match config::validate_access_code(
            std::env::var("H2D_ACCESS_CODE").unwrap_or_default().trim(),
        ) {
            Ok(()) => Check::warn(
                "access code",
                "using H2D_ACCESS_CODE from the environment. A mode 600 file is safer.",
            ),
            Err(err) => Check::fail("access code", err.to_string()),
        };
    }
    let path = config::expand_user(file.access_code_file.trim());
    if file.access_code_file.trim().is_empty() {
        return Check::fail("access code", "no access code file set");
    }
    match config::read_access_code(&path) {
        Ok(_) => Check::ok("access code", format!("readable at {}", path.display())),
        Err(err) => Check::fail("access code", err.to_string()),
    }
}

fn tcp_check(name: &str, ip: &str, port: u16) -> Check {
    let addr = format!("{ip}:{port}");
    let Ok(socket) = addr.parse::<SocketAddr>() else {
        return Check::fail(name, format!("{addr} is not a socket address"));
    };
    match TcpStream::connect_timeout(&socket, Duration::from_secs(2)) {
        Ok(_) => Check::ok(name, format!("{addr} accepts a connection")),
        Err(err) => Check::fail(name, format!("{addr} unreachable: {err}")),
    }
}
