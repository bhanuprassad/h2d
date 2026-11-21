use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::json;

use crate::config::{self, Overrides};
use crate::doctor;
use crate::error::{Error, Result};
use crate::job::External;
use crate::ops::{self, PrintOptions};
use crate::slice::{self, SliceRequest};
use crate::status::StatusReport;
use crate::threemf;
use crate::with_ok;

#[derive(Parser)]
#[command(
    name = "h2d",
    version,
    about = "Slice and submit prints to a Bambu Lab H2D over the LAN",
    after_help = "The printer needs LAN Only and Developer Mode. Keep the access code in a mode 600 file, not in the command arguments."
)]
struct Cli {
    /// Print machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    /// Config file. Defaults to $XDG_CONFIG_HOME/h2d/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Printer IP. Overrides the config file and H2D_IP.
    #[arg(long, global = true)]
    ip: Option<String>,
    /// Printer serial. Overrides the config file and H2D_SERIAL.
    #[arg(long, global = true)]
    serial: Option<String>,
    /// File containing the LAN access code.
    #[arg(long, global = true)]
    access_code_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show or write the config file. The access code is not stored in it.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check config, binaries, and whether the printer accepts TCP on 8883 and 990.
    Doctor,
    /// Read temperatures, progress, AMS, and the print state.
    Status,
    /// Show filaments and bed type inside a sliced .gcode.3mf.
    Inspect {
        file: PathBuf,
        #[arg(long, default_value_t = 1)]
        plate: u32,
    },
    /// Slice a model with OrcaSlicer into a plate .gcode.3mf.
    Slice {
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        printer_profile: Option<PathBuf>,
        #[arg(long)]
        process_profile: Option<PathBuf>,
        #[arg(long = "filament-profile")]
        filament_profiles: Vec<PathBuf>,
        /// Plate to slice. 0 slices every plate.
        #[arg(long, default_value_t = 1)]
        plate: u32,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        arrange: bool,
        #[arg(long, default_value_t = false)]
        orient: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        min_save: bool,
    },
    /// Upload a sliced .gcode.3mf and start it.
    Print {
        file: PathBuf,
        #[arg(long, default_value_t = 1)]
        plate: u32,
        /// ams:slot per filament, comma-separated. Use - for an unused filament.
        #[arg(long)]
        ams: Option<String>,
        /// Single-filament external spool: left or right.
        #[arg(long)]
        external: Option<String>,
        #[arg(long)]
        bed: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Follow the job until it finishes or the timeout hits.
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 720)]
        watch_timeout_mins: u64,
        #[arg(long)]
        timelapse: bool,
        #[arg(long)]
        no_bed_level: bool,
        #[arg(long)]
        no_flow_cali: bool,
        #[arg(long)]
        no_vibration_cali: bool,
        #[arg(long)]
        no_layer_inspect: bool,
        /// Send use_ams false. Default is true when a mapping is present.
        #[arg(long)]
        no_ams: bool,
        #[arg(long)]
        ftp_dir: Option<String>,
    },
    /// Pause the active print.
    Pause,
    /// Resume a paused print.
    Resume,
    /// Stop the active print.
    Stop,
    /// Serve a stdio MCP endpoint for an agent.
    Mcp,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum ConfigAction {
    /// Print the config path.
    Path,
    /// Print the config. The access code is not in it.
    Show,
    /// Update one or more fields.
    Set {
        #[arg(long)]
        ip: Option<String>,
        #[arg(long)]
        serial: Option<String>,
        #[arg(long)]
        access_code_file: Option<PathBuf>,
        #[arg(long)]
        mqtt_port: Option<u16>,
        #[arg(long)]
        ftp_port: Option<u16>,
        #[arg(long)]
        ftp_dir: Option<String>,
        #[arg(long)]
        timeout_secs: Option<u64>,
        #[arg(long)]
        orca_bin: Option<String>,
        #[arg(long)]
        curl_bin: Option<String>,
        #[arg(long)]
        printer_profile: Option<PathBuf>,
        #[arg(long)]
        process_profile: Option<PathBuf>,
        /// Repeat for each filament, in slicer order. Replaces the saved list.
        #[arg(long = "filament-profile")]
        filament_profiles: Vec<PathBuf>,
    },
}

pub async fn run() -> i32 {
    let cli = Cli::parse();
    let json = cli.json;
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) if json => {
            emit(&json!({"ok": false, "error": err.to_string()}));
            1
        }
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

async fn dispatch(cli: Cli) -> Result<i32> {
    let json = cli.json;
    let overrides = Overrides {
        config_path: cli.config.clone(),
        ip: cli.ip.clone(),
        serial: cli.serial.clone(),
        access_code_file: cli.access_code_file.clone(),
    };
    match cli.command {
        Command::Config { action } => config_action(action, &overrides, json),
        Command::Doctor => {
            let (path, file) = config::load_effective(&overrides)?;
            let checks = doctor::doctor(&path, &file);
            let ok = !doctor::failed(&checks);
            if json {
                emit(&with_ok(&checks, ok)?);
            } else {
                for check in &checks {
                    println!("{:<5} {:<12} {}", check.level, check.name, check.detail);
                }
            }
            Ok(if ok { 0 } else { 1 })
        }
        Command::Status => {
            let (_path, file) = config::load_effective(&overrides)?;
            let conn = config::connection_from(&file)?;
            let report = ops::fetch_status(&conn).await?;
            if json {
                emit(&with_ok(&report, true)?);
            } else {
                println!("{}", format_status(&report));
            }
            Ok(0)
        }
        Command::Inspect { file, plate } => {
            let info = threemf::inspect_path(&file, plate)?;
            if json {
                emit(&with_ok(&info, true)?);
            } else {
                println!(
                    "plate {}  gcode {}  {} bytes  bed {}",
                    info.plate,
                    if info.gcode_present { "yes" } else { "no" },
                    info.gcode_bytes,
                    info.bed_type.as_deref().unwrap_or("unknown")
                );
                if info.filaments.is_empty() {
                    println!("filaments: none found in the sliced file");
                }
                for filament in &info.filaments {
                    println!(
                        "  filament {}  {}  {}  {} g",
                        filament.id, filament.kind, filament.color, filament.used_g
                    );
                }
            }
            Ok(0)
        }
        Command::Slice {
            input,
            output,
            printer_profile,
            process_profile,
            filament_profiles,
            plate,
            arrange,
            orient,
            min_save,
        } => {
            let (_path, file) = config::load_effective(&overrides)?;
            let request = SliceRequest {
                bin: file.orca_bin,
                input,
                output: output.clone(),
                printer_profile: printer_profile.unwrap_or_else(|| file.printer_profile.into()),
                process_profile: process_profile.unwrap_or_else(|| file.process_profile.into()),
                filament_profiles: if filament_profiles.is_empty() {
                    file.filament_profiles.into_iter().map(Into::into).collect()
                } else {
                    filament_profiles
                },
                plate,
                arrange,
                orient,
                min_save,
                allow_newer: true,
            };
            let outcome = slice::slice(&request, json)?;
            if json {
                emit(&with_ok(&outcome, true)?);
            } else {
                println!("wrote {}", outcome.output.display());
            }
            Ok(0)
        }
        Command::Print {
            file,
            plate,
            ams,
            external,
            bed,
            dry_run,
            watch,
            watch_timeout_mins,
            timelapse,
            no_bed_level,
            no_flow_cali,
            no_vibration_cali,
            no_layer_inspect,
            no_ams,
            ftp_dir,
        } => {
            let (_path, file_cfg) = config::load_effective(&overrides)?;
            let conn = if dry_run {
                config::connection_from(&file_cfg).ok()
            } else {
                Some(config::connection_from(&file_cfg)?)
            };
            let external = match external {
                Some(value) => Some(External::parse(&value)?),
                None => None,
            };
            let watch_for = if watch {
                Some(Duration::from_secs(watch_timeout_mins.max(1) * 60))
            } else {
                None
            };
            let outcome = ops::submit(
                conn.as_ref(),
                PrintOptions {
                    file,
                    plate,
                    ams,
                    external,
                    bed,
                    use_ams: if no_ams { Some(false) } else { None },
                    timelapse,
                    bed_leveling: !no_bed_level,
                    flow_cali: !no_flow_cali,
                    vibration_cali: !no_vibration_cali,
                    layer_inspect: !no_layer_inspect,
                    dry_run,
                    watch: watch_for,
                    ftp_dir,
                },
                |status| {
                    if json {
                        let mut line = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
                        if let Some(object) = line.as_object_mut() {
                            object.insert("event".into(), json!("progress"));
                        }
                        emit(&line);
                    } else {
                        println!("{}", format_progress(status));
                    }
                },
            )
            .await?;
            let code = outcome.exit_code();
            if json {
                emit(&with_ok(&outcome, code == 0)?);
            } else if dry_run {
                println!("{}", outcome.detail);
                println!("remote {}", outcome.remote_name);
                println!("file url {}", outcome.file_url);
                if let Some(url) = &outcome.ftp_url {
                    println!("ftp url {url}");
                }
                println!("{}", serde_json::to_string_pretty(&outcome.payload)?);
            } else {
                println!("{}", outcome.detail);
            }
            Ok(code)
        }
        Command::Pause => control(&overrides, "pause", json).await,
        Command::Resume => control(&overrides, "resume", json).await,
        Command::Stop => control(&overrides, "stop", json).await,
        Command::Mcp => {
            crate::mcp::serve(crate::mcp::McpContext { overrides }).await?;
            Ok(0)
        }
    }
}

async fn control(overrides: &Overrides, command: &str, json: bool) -> Result<i32> {
    let (_path, file) = config::load_effective(overrides)?;
    let conn = config::connection_from(&file)?;
    let ack = ops::control(&conn, command).await?;
    let summary = ops::ack_summary(&ack);
    if json {
        emit(&with_ok(&summary, true)?);
    } else {
        println!("{command}: {}", summary["result"].as_str().unwrap_or("ok"));
    }
    Ok(0)
}

fn config_action(action: ConfigAction, overrides: &Overrides, json: bool) -> Result<i32> {
    let path = config::resolve_config_path(overrides);
    match action {
        ConfigAction::Path => {
            if json {
                emit(&json!({"ok": true, "path": path}));
            } else {
                println!("{}", path.display());
            }
            Ok(0)
        }
        ConfigAction::Show => {
            let file = config::load_file(&path)?;
            if json {
                emit(&json!({"ok": true, "path": path, "config": file}));
            } else {
                println!("# {}", path.display());
                print!("{}", toml::to_string_pretty(&file)?);
            }
            Ok(0)
        }
        ConfigAction::Set {
            ip,
            serial,
            access_code_file,
            mqtt_port,
            ftp_port,
            ftp_dir,
            timeout_secs,
            orca_bin,
            curl_bin,
            printer_profile,
            process_profile,
            filament_profiles,
        } => {
            if ip.is_none()
                && serial.is_none()
                && access_code_file.is_none()
                && mqtt_port.is_none()
                && ftp_port.is_none()
                && ftp_dir.is_none()
                && timeout_secs.is_none()
                && orca_bin.is_none()
                && curl_bin.is_none()
                && printer_profile.is_none()
                && process_profile.is_none()
                && filament_profiles.is_empty()
            {
                return Err(Error::msg("config set needs at least one field"));
            }
            let mut file = config::load_file(&path)?;
            assign(&mut file.ip, ip);
            assign(&mut file.serial, serial);
            if let Some(code_path) = access_code_file {
                file.access_code_file = code_path.to_string_lossy().to_string();
            }
            if let Some(port) = mqtt_port {
                file.mqtt_port = port;
            }
            if let Some(port) = ftp_port {
                file.ftp_port = port;
            }
            if let Some(dir) = ftp_dir {
                file.ftp_dir = crate::job::sanitize_ftp_dir(&dir)?;
            }
            if let Some(secs) = timeout_secs {
                file.timeout_secs = secs;
            }
            assign(&mut file.orca_bin, orca_bin);
            assign(&mut file.curl_bin, curl_bin);
            if let Some(profile) = printer_profile {
                file.printer_profile = profile.to_string_lossy().to_string();
            }
            if let Some(profile) = process_profile {
                file.process_profile = profile.to_string_lossy().to_string();
            }
            if !filament_profiles.is_empty() {
                file.filament_profiles = filament_profiles
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect();
            }
            config::save_file(&path, &file)?;
            if json {
                emit(&json!({"ok": true, "path": path}));
            } else {
                println!("wrote {}", path.display());
            }
            Ok(0)
        }
    }
}

fn assign(slot: &mut String, value: Option<String>) {
    if let Some(value) = value {
        *slot = value;
    }
}

fn format_status(report: &StatusReport) -> String {
    let mut lines = vec![format_progress(report)];
    let chamber = report
        .chamber_c
        .map(|temp| format!("  chamber {temp:.1}°C"))
        .unwrap_or_default();
    lines.push(format!(
        "nozzle {:.1}/{:.1}°C  bed {:.1}/{:.1}°C{chamber}",
        report.nozzle_c, report.nozzle_target_c, report.bed_c, report.bed_target_c
    ));
    lines.push(format!(
        "speed {} ({})  error {}  wifi {}",
        report.speed_name, report.speed_level, report.print_error, report.wifi_signal
    ));
    if report.ams.is_empty() {
        lines.push("ams: none reported".to_string());
    } else {
        for slot in &report.ams {
            lines.push(format!(
                "ams {} slot {}  {}  {}  remain {:?}",
                slot.ams_id, slot.slot_id, slot.kind, slot.color, slot.remain
            ));
        }
    }
    if !report.hms.is_empty() {
        lines.push(format!(
            "hms {}",
            serde_json::to_string(&report.hms).unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn format_progress(report: &StatusReport) -> String {
    let job = if report.job.is_empty() {
        "(none)"
    } else {
        &report.job
    };
    format!(
        "{:<8} {:>3}%  layer {}/{}  {} min left  job {}",
        report.gcode_state,
        report.progress_percent,
        report.layer,
        report.total_layers,
        report.remaining_min,
        job
    )
}

fn emit(value: &impl serde::Serialize) {
    let Ok(text) = serde_json::to_string(value) else {
        return;
    };
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}
