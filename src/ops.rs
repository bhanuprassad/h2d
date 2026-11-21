use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::Connection;
use crate::error::{Error, Result};
use crate::job::{self, External, PrintRequest};
use crate::mqtt::Session;
use crate::status::{self, StatusReport};
use crate::threemf::{self, PlateInfo};
use crate::upload;

#[derive(Debug, Clone)]
pub struct PrintOptions {
    pub file: PathBuf,
    pub plate: u32,
    pub ams: Option<String>,
    pub external: Option<External>,
    pub bed: Option<String>,
    /// None keeps the default, which is on whenever a mapping is present.
    pub use_ams: Option<bool>,
    pub timelapse: bool,
    pub bed_leveling: bool,
    pub flow_cali: bool,
    pub vibration_cali: bool,
    pub layer_inspect: bool,
    pub dry_run: bool,
    pub watch: Option<Duration>,
    pub ftp_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatchOutcome {
    pub status: StatusReport,
    pub timed_out: bool,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitOutcome {
    pub dry_run: bool,
    pub remote_name: String,
    pub file_url: String,
    pub ftp_url: Option<String>,
    pub plate: PlateInfo,
    pub payload: Value,
    pub accepted: bool,
    pub detail: String,
    pub status: Option<StatusReport>,
    pub watch: Option<WatchOutcome>,
}

impl SubmitOutcome {
    pub fn exit_code(&self) -> i32 {
        if self.dry_run {
            return 0;
        }
        if let Some(watch) = &self.watch {
            if watch.timed_out {
                return 3;
            }
            if watch.failed {
                return 2;
            }
            return 0;
        }
        if self.accepted { 0 } else { 2 }
    }
}

pub async fn fetch_status(conn: &Connection) -> Result<StatusReport> {
    let mut session = Session::connect(conn).await?;
    session.status(conn.timeout).await
}

pub async fn control(conn: &Connection, command: &str) -> Result<Value> {
    let mut session = Session::connect(conn).await?;
    let ack = session.print_command(command, conn.timeout).await?;
    explain_ack(&ack)?;
    Ok(ack)
}

pub fn explain_ack(payload: &Value) -> Result<()> {
    let Some(obj) = status::ack_fields(payload, "print") else {
        return Err(Error::msg("printer acknowledgement had no print object"));
    };
    let result = status::ack_result(obj);
    if result.eq_ignore_ascii_case("success") {
        return Ok(());
    }
    let reason = status::ack_reason(obj);
    if reason.to_ascii_lowercase().contains("verify failed") {
        return Err(Error::msg(format!(
            "printer rejected the command ({reason}). Turn on LAN Only and Developer Mode. Unsigned commands are refused while Developer Mode is off."
        )));
    }
    Err(Error::msg(format!(
        "printer rejected the command: {result} ({reason})"
    )))
}

pub async fn submit<F>(
    conn: Option<&Connection>,
    opts: PrintOptions,
    mut on_watch: F,
) -> Result<SubmitOutcome>
where
    F: FnMut(&StatusReport),
{
    if opts.plate == 0 {
        return Err(Error::msg("plate numbers start at 1"));
    }
    let plate = threemf::inspect_path(&opts.file, opts.plate)?;
    threemf::require_sliced(&plate, &opts.file)?;
    let mapping = job::resolve_mapping(opts.ams.as_deref(), opts.external, plate.filaments.len())?;
    let use_ams = opts.use_ams.unwrap_or(true);
    let bed_type = opts
        .bed
        .clone()
        .filter(|bed| !bed.is_empty() && bed != "auto")
        .or_else(|| plate.bed_type.clone())
        .unwrap_or_else(|| "auto".to_string());
    let file_name = opts
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::msg("print file name is not valid UTF-8"))?;
    let remote_name = job::remote_name(file_name)?;
    let ftp_dir = if let Some(dir) = &opts.ftp_dir {
        job::sanitize_ftp_dir(dir)?
    } else if let Some(conn) = conn {
        conn.ftp_dir.clone()
    } else {
        "cache".to_string()
    };

    let mut request = PrintRequest {
        sequence_id: "1".to_string(),
        remote_name: remote_name.clone(),
        ftp_dir: ftp_dir.clone(),
        plate: opts.plate,
        subtask_name: job::subtask_name(&remote_name),
        bed_type,
        mapping,
        use_ams,
        timelapse: opts.timelapse,
        bed_leveling: opts.bed_leveling,
        flow_cali: opts.flow_cali,
        vibration_cali: opts.vibration_cali,
        layer_inspect: opts.layer_inspect,
    };
    let file_url = job::mqtt_file_url(&ftp_dir, &remote_name);
    let ftp_url = conn.map(|conn| job::ftp_url(&conn.ip, conn.ftp_port, &ftp_dir, &remote_name));

    if opts.dry_run {
        return Ok(SubmitOutcome {
            dry_run: true,
            remote_name,
            file_url,
            ftp_url,
            plate,
            payload: job::project_file_payload(&request),
            accepted: false,
            detail: "dry run, nothing was uploaded or started".to_string(),
            status: None,
            watch: None,
        });
    }

    let Some(conn) = conn else {
        return Err(Error::msg(
            "printer connection is required to start a print",
        ));
    };

    let mut session = Session::connect(conn).await?;
    let before = session.status(conn.timeout).await?;
    if before.is_active() {
        let job = if before.job.is_empty() {
            "(unnamed)"
        } else {
            before.job.as_str()
        };
        return Err(Error::msg(format!(
            "printer is already {} on job '{job}'. Stop it before starting another.",
            before.gcode_state
        )));
    }

    upload::upload_gcode(
        &conn.curl_bin,
        &conn.ip,
        conn.ftp_port,
        &conn.access_code,
        &ftp_dir,
        &remote_name,
        &opts.file,
    )
    .await?;

    request.sequence_id = session.next_seq();
    let payload = job::project_file_payload(&request);
    let ack = session.publish_print(payload.clone(), conn.timeout).await?;
    explain_ack(&ack)?;
    let status = confirm_started(&mut session).await?;
    let accepted = status.is_active() || status.gcode_state == "FINISH";
    let detail = if accepted {
        format!("printer is {}", status.gcode_state)
    } else if status.print_error != 0 {
        format!(
            "command was accepted but the printer stayed {} with print_error {}",
            status.gcode_state, status.print_error
        )
    } else {
        format!(
            "command was accepted but the printer stayed {}. The file is on the printer and was not confirmed running.",
            empty_state(&status.gcode_state)
        )
    };

    let watch = if accepted {
        if let Some(limit) = opts.watch {
            Some(watch_job(&mut session, limit, &mut on_watch).await?)
        } else {
            None
        }
    } else {
        None
    };

    Ok(SubmitOutcome {
        dry_run: false,
        remote_name,
        file_url,
        ftp_url,
        plate,
        payload,
        accepted,
        detail,
        status: Some(status),
        watch,
    })
}

fn empty_state(state: &str) -> &str {
    if state.is_empty() { "IDLE" } else { state }
}

async fn confirm_started(session: &mut Session) -> Result<StatusReport> {
    session.status(Duration::from_secs(8)).await?;
    session
        .wait_until(Duration::from_secs(30), |status| {
            status.is_active()
                || status.gcode_state == "FINISH"
                || status.gcode_state == "FAILED"
                || status.print_error != 0
        })
        .await
}

pub async fn watch_job<F>(
    session: &mut Session,
    timeout_for: Duration,
    on_update: &mut F,
) -> Result<WatchOutcome>
where
    F: FnMut(&StatusReport),
{
    let mut saw_active = false;
    let mut last = String::new();
    let status = session
        .wait_until(timeout_for, |status| {
            if status.is_active() {
                saw_active = true;
            }
            let key = format!(
                "{}:{}:{}:{}",
                status.gcode_state, status.progress_percent, status.layer, status.print_error
            );
            if !status.gcode_state.is_empty() && key != last {
                last.clone_from(&key);
                on_update(status);
            }
            status.gcode_state == "FINISH"
                || status.gcode_state == "FAILED"
                || (saw_active && status.gcode_state == "IDLE")
        })
        .await?;
    let failed = status.is_terminal_failure();
    let finished = status.gcode_state == "FINISH" || (saw_active && status.gcode_state == "IDLE");
    Ok(WatchOutcome {
        timed_out: !failed && !finished,
        failed,
        status,
    })
}

pub fn ack_summary(payload: &Value) -> Value {
    let result = status::ack_fields(payload, "print")
        .map(status::ack_result)
        .unwrap_or_default();
    let reason = status::ack_fields(payload, "print")
        .map(status::ack_reason)
        .unwrap_or_default();
    json!({
        "result": result,
        "reason": reason,
    })
}
