use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{self, Overrides};
use crate::doctor;
use crate::error::{Error, Result};
use crate::job::External;
use crate::ops::{self, PrintOptions};
use crate::slice::{self, SliceRequest};
use crate::threemf;
use crate::with_ok;

pub struct McpContext {
    pub overrides: Overrides,
}

pub async fn handle_line(line: &str, ctx: &McpContext) -> Option<Result<Value>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return Some(Ok(rpc_error(Value::Null, -32700, "parse error")));
        }
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    if method.starts_with("notifications/") || message.get("id").is_none() {
        return None;
    }
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    Some(dispatch(id, method, &params, ctx).await)
}

async fn dispatch(id: Value, method: &str, params: &Value, ctx: &McpContext) -> Result<Value> {
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "h2d", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Drive a Bambu Lab H2D on the LAN. The printer needs LAN Only and Developer Mode. h2d_print uploads and starts a job only when confirm is true."
                }
            }))
        }
        "ping" => Ok(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tool_specs()}
        })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(name, &args, ctx).await {
                Ok(text) => Ok(tool_result(&id, &text, false)),
                Err(err) => Ok(tool_result(&id, &err.to_string(), true)),
            }
        }
        other => Ok(rpc_error(id, -32601, &format!("unknown method {other}"))),
    }
}

fn tool_result(id: &Value, text: &str, is_error: bool) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": is_error
        }
    })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

async fn call_tool(name: &str, args: &Value, ctx: &McpContext) -> Result<String> {
    match name {
        "h2d_doctor" => {
            let (path, file) = config::load_effective(&ctx.overrides)?;
            let checks = doctor::doctor(&path, &file);
            Ok(serde_json::to_string_pretty(&with_ok(
                &checks,
                !doctor::failed(&checks),
            )?)?)
        }
        "h2d_status" => {
            let (_path, file) = config::load_effective(&ctx.overrides)?;
            let conn = config::connection_from(&file)?;
            let report = ops::fetch_status(&conn).await?;
            Ok(serde_json::to_string_pretty(&with_ok(&report, true)?)?)
        }
        "h2d_inspect" => {
            let file = required_str(args, "file")?;
            let plate = args.get("plate").and_then(Value::as_u64).unwrap_or(1) as u32;
            let info = threemf::inspect_path(std::path::Path::new(&file), plate)?;
            Ok(serde_json::to_string_pretty(&with_ok(&info, true)?)?)
        }
        "h2d_slice" => {
            let (_path, file) = config::load_effective(&ctx.overrides)?;
            let input = required_str(args, "input")?;
            let output = required_str(args, "output")?;
            let request = SliceRequest {
                bin: arg_str(args, "orca_bin").unwrap_or(file.orca_bin),
                input: input.into(),
                output: output.into(),
                printer_profile: arg_str(args, "printer_profile")
                    .unwrap_or(file.printer_profile)
                    .into(),
                process_profile: arg_str(args, "process_profile")
                    .unwrap_or(file.process_profile)
                    .into(),
                filament_profiles: filament_args(args)
                    .unwrap_or(file.filament_profiles)
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                plate: args.get("plate").and_then(Value::as_u64).unwrap_or(1) as u32,
                arrange: args.get("arrange").and_then(Value::as_bool).unwrap_or(true),
                orient: args.get("orient").and_then(Value::as_bool).unwrap_or(false),
                min_save: true,
                allow_newer: true,
            };
            let outcome = slice::slice(&request, true)?;
            Ok(serde_json::to_string_pretty(&with_ok(&outcome, true)?)?)
        }
        "h2d_print" => {
            let confirm = args
                .get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let dry_run = args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !confirm && !dry_run {
                return Err(Error::msg(
                    "refusing to start a print. Call h2d_print again with confirm true after the user has agreed, or set dry_run true to preview the MQTT payload.",
                ));
            }
            let (_path, file) = config::load_effective(&ctx.overrides)?;
            let conn = if dry_run {
                config::connection_from(&file).ok()
            } else {
                Some(config::connection_from(&file)?)
            };
            let external = match arg_str(args, "external") {
                Some(value) => Some(External::parse(&value)?),
                None => None,
            };
            let watch_minutes = args
                .get("watch_minutes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let watch = if watch_minutes == 0 {
                None
            } else {
                Some(Duration::from_secs(watch_minutes.min(24 * 60) * 60))
            };
            let no_ams = args.get("no_ams").and_then(Value::as_bool).unwrap_or(false);
            let outcome = ops::submit(
                conn.as_ref(),
                PrintOptions {
                    file: required_str(args, "file")?.into(),
                    plate: args.get("plate").and_then(Value::as_u64).unwrap_or(1) as u32,
                    ams: arg_str(args, "ams"),
                    external,
                    bed: arg_str(args, "bed"),
                    use_ams: if no_ams { Some(false) } else { None },
                    timelapse: args
                        .get("timelapse")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    bed_leveling: !args
                        .get("no_bed_level")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    flow_cali: !args
                        .get("no_flow_cali")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    vibration_cali: !args
                        .get("no_vibration_cali")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    layer_inspect: !args
                        .get("no_layer_inspect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    dry_run,
                    watch,
                    ftp_dir: arg_str(args, "ftp_dir"),
                },
                |_| {},
            )
            .await?;
            let ok = outcome.dry_run || outcome.exit_code() == 0;
            Ok(serde_json::to_string_pretty(&with_ok(&outcome, ok)?)?)
        }
        "h2d_pause" => control_text(ctx, "pause").await,
        "h2d_resume" => control_text(ctx, "resume").await,
        "h2d_stop" => control_text(ctx, "stop").await,
        other => Err(Error::msg(format!("unknown tool {other}"))),
    }
}

async fn control_text(ctx: &McpContext, command: &str) -> Result<String> {
    let (_path, file) = config::load_effective(&ctx.overrides)?;
    let conn = config::connection_from(&file)?;
    let ack = ops::control(&conn, command).await?;
    Ok(serde_json::to_string_pretty(&with_ok(
        ops::ack_summary(&ack),
        true,
    )?)?)
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    arg_str(args, key).ok_or_else(|| Error::msg(format!("missing argument {key}")))
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn filament_args(args: &Value) -> Option<Vec<String>> {
    let list = args.get("filament_profiles")?.as_array()?;
    let profiles: Vec<String> = list
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if profiles.is_empty() {
        None
    } else {
        Some(profiles)
    }
}

fn tool_specs() -> Vec<Value> {
    vec![
        tool(
            "h2d_doctor",
            "Check config, access-code file, curl, OrcaSlicer, and whether MQTT and FTPS ports accept a TCP connection.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "h2d_status",
            "Read nozzle, bed, chamber, progress, AMS trays, and gcode_state over LAN MQTT.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "h2d_inspect",
            "Read a sliced .gcode.3mf: plate gcode, bed type, and filament list. Does not touch the printer.",
            json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "plate": {"type": "integer", "minimum": 1}
                },
                "required": ["file"]
            }),
        ),
        tool(
            "h2d_slice",
            "Slice a model with OrcaSlicer into a plate .gcode.3mf. Profiles come from arguments or the h2d config.",
            json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"},
                    "output": {"type": "string"},
                    "plate": {"type": "integer"},
                    "arrange": {"type": "boolean"},
                    "orient": {"type": "boolean"},
                    "printer_profile": {"type": "string"},
                    "process_profile": {"type": "string"},
                    "filament_profiles": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["input", "output"]
            }),
        ),
        tool(
            "h2d_print",
            "Upload a sliced .gcode.3mf and start it. Refuses unless confirm is true. Set dry_run true to preview the payload without sending it.",
            json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "plate": {"type": "integer", "minimum": 1},
                    "ams": {"type": "string", "description": "Comma-separated ams:slot entries, one per filament. Use - for unused. Example: 0:0,0:2"},
                    "external": {"type": "string", "enum": ["left", "right"]},
                    "bed": {"type": "string"},
                    "confirm": {"type": "boolean"},
                    "dry_run": {"type": "boolean"},
                    "watch_minutes": {"type": "integer"},
                    "no_ams": {"type": "boolean"},
                    "timelapse": {"type": "boolean"},
                    "ftp_dir": {"type": "string"}
                },
                "required": ["file"]
            }),
        ),
        tool(
            "h2d_pause",
            "Pause the active print.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "h2d_resume",
            "Resume a paused print.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "h2d_stop",
            "Stop the active print.",
            json!({"type": "object", "properties": {}}),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

pub async fn serve(ctx: McpContext) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = std::io::stdout().lock();
    use std::io::Write;
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_line(&line, &ctx).await {
            let value = match response {
                Ok(value) => value,
                Err(err) => tool_result(&Value::Null, &err.to_string(), true),
            };
            writeln!(stdout, "{}", serde_json::to_string(&value)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> McpContext {
        McpContext {
            overrides: Overrides::default(),
        }
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let init = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            &ctx(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "h2d");
        let list = handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &ctx())
            .await
            .unwrap()
            .unwrap();
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"h2d_print"));
        assert!(names.contains(&"h2d_status"));
    }

    #[tokio::test]
    async fn print_without_confirm_does_not_claim_success() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"h2d_print","arguments":{"file":"job.gcode.3mf"}}}"#,
            &ctx(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("confirm"));
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let response = handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &ctx(),
        )
        .await;
        assert!(response.is_none());
    }
}
