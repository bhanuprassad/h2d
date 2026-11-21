use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Error, Result, redact};
use crate::job::ftp_url;

/// curl's OpenSSL backend resumes the TLS session on the FTPS data connection.
/// The printer rejects uploads that do not. native-tls cannot resume that session.
pub fn curl_args() -> &'static [&'static str] {
    &[
        "--config",
        "-",
        "--silent",
        "--show-error",
        "--fail",
        "--disable-epsv",
        "--ftp-pasv",
        "--ssl-reqd",
        "--insecure",
        "--connect-timeout",
        "10",
    ]
}

pub fn curl_config_body(user: &str, password: &str, local: &Path, url: &str) -> Result<String> {
    if local
        .as_os_str()
        .to_string_lossy()
        .contains(['"', '\n', '\r'])
    {
        return Err(Error::msg(
            "the sliced file path contains a quote or newline, which the curl config cannot carry",
        ));
    }
    if url.contains(['"', '\n', '\r'])
        || user.contains(['"', '\n'])
        || password.contains(['"', '\n'])
    {
        return Err(Error::msg(
            "ftp url or login contains a character curl config cannot quote",
        ));
    }
    Ok(format!(
        "user = \"{user}:{password}\"\nupload-file = \"{}\"\nurl = \"{url}\"\n",
        local.display()
    ))
}

pub async fn upload_gcode(
    curl_bin: &str,
    ip: &str,
    port: u16,
    access_code: &str,
    ftp_dir: &str,
    remote_name: &str,
    local: &Path,
) -> Result<String> {
    let url = ftp_url(ip, port, ftp_dir, remote_name);
    let body = curl_config_body("bblp", access_code, local, &url)?;
    let args: Vec<&str> = curl_args().to_vec();
    let bin = curl_bin.to_string();
    let secret = access_code.to_string();
    let output = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(&bin).args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::msg(format!(
                    "could not run '{bin}'. curl is required to upload over implicit FTPS on port 990."
                ))
            } else {
                Error::from(err)
            }
        })?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::msg("could not write the curl config"))?;
            stdin.write_all(body.as_bytes())?;
        }
        child.wait_with_output().map_err(Error::from)
    })
    .await
    .map_err(|err| Error::msg(format!("upload task failed: {err}")))??;

    if output.status.success() {
        return Ok(url);
    }
    let stderr = redact(&String::from_utf8_lossy(&output.stderr), &secret);
    let stdout = redact(&String::from_utf8_lossy(&output.stdout), &secret);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(Error::msg(format!(
        "FTPS upload failed (curl exit {:?}): {detail}",
        output.status.code()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_stays_out_of_argv() {
        let args = curl_args().join(" ");
        assert!(!args.contains("s3cret"));
        assert!(args.contains("--config"));
        let body = curl_config_body(
            "bblp",
            "s3cret",
            Path::new("/tmp/job.gcode.3mf"),
            "ftps://192.168.1.9:990/cache/job.gcode.3mf",
        )
        .unwrap();
        assert!(body.contains("bblp:s3cret"));
        assert!(body.contains("upload-file"));
    }
}
