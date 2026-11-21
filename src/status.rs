use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AmsSlot {
    pub ams_id: String,
    pub slot_id: String,
    pub kind: String,
    pub color: String,
    pub name: String,
    pub remain: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StatusReport {
    pub gcode_state: String,
    pub stage: i64,
    pub progress_percent: i64,
    pub remaining_min: i64,
    pub layer: i64,
    pub total_layers: i64,
    pub job: String,
    pub nozzle_c: f64,
    pub nozzle_target_c: f64,
    pub bed_c: f64,
    pub bed_target_c: f64,
    pub chamber_c: Option<f64>,
    pub speed_level: i64,
    pub speed_name: String,
    pub print_error: i64,
    pub wifi_signal: String,
    pub sdcard: Option<bool>,
    pub hms: Vec<Value>,
    pub ams: Vec<AmsSlot>,
    /// Scalar nozzle-related fields kept so the first live report can be compared
    /// with the summary above. H2D firmware names the second nozzle inconsistently.
    pub nozzle_fields: Map<String, Value>,
}

impl StatusReport {
    pub fn is_terminal_failure(&self) -> bool {
        self.gcode_state == "FAILED"
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.gcode_state.as_str(),
            "PREPARE" | "RUNNING" | "PAUSE" | "SLICING"
        )
    }
}

pub fn speed_name(level: i64) -> String {
    match level {
        1 => "silent",
        2 => "standard",
        3 => "sport",
        4 => "ludicrous",
        _ => "unknown",
    }
    .to_string()
}

/// Fold a printer report into `state`. Command ACKs are not printer state.
pub fn merge_report(state: &mut Map<String, Value>, payload: &Value) {
    let Some(print) = payload.get("print").and_then(Value::as_object) else {
        return;
    };
    if print.contains_key("result") {
        return;
    }
    let looks_like_status = print.get("command").and_then(Value::as_str) == Some("push_status")
        || print.contains_key("gcode_state")
        || print.contains_key("mc_percent")
        || print.contains_key("nozzle_temper");
    if !looks_like_status {
        return;
    }
    for (key, value) in print {
        if matches!(
            key.as_str(),
            "command" | "msg" | "sequence_id" | "sequenceId"
        ) {
            continue;
        }
        state.insert(key.clone(), value.clone());
    }
}

pub fn report_from_state(state: &Map<String, Value>) -> StatusReport {
    let value = Value::Object(state.clone());
    report_from_print(&value)
}

pub fn report_from_print(print: &Value) -> StatusReport {
    let mut nozzle_fields = Map::new();
    if let Some(obj) = print.as_object() {
        for (key, value) in obj {
            if key.to_ascii_lowercase().contains("nozzle")
                && !value.is_object()
                && !value.is_array()
            {
                nozzle_fields.insert(key.clone(), value.clone());
            }
        }
    }
    StatusReport {
        gcode_state: json_string(print.get("gcode_state")),
        stage: json_i64(print.get("stg_cur")),
        progress_percent: json_i64(print.get("mc_percent")),
        remaining_min: json_i64(print.get("mc_remaining_time")),
        layer: json_i64(print.get("layer_num")),
        total_layers: json_i64(print.get("total_layer_num")),
        job: {
            let name = json_string(print.get("subtask_name"));
            if name.is_empty() {
                json_string(print.get("gcode_file"))
            } else {
                name
            }
        },
        nozzle_c: json_f64(print.get("nozzle_temper")),
        nozzle_target_c: json_f64(print.get("nozzle_target_temper")),
        bed_c: json_f64(print.get("bed_temper")),
        bed_target_c: json_f64(print.get("bed_target_temper")),
        chamber_c: print
            .get("chamber_temper")
            .map(|value| json_f64(Some(value))),
        speed_level: json_i64(print.get("spd_lvl")),
        speed_name: speed_name(json_i64(print.get("spd_lvl"))),
        print_error: json_i64(print.get("print_error")),
        wifi_signal: json_string(print.get("wifi_signal")),
        sdcard: print.get("sdcard").and_then(Value::as_bool),
        hms: print
            .get("hms")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        ams: ams_slots(print),
        nozzle_fields,
    }
}

fn ams_slots(print: &Value) -> Vec<AmsSlot> {
    let mut slots = Vec::new();
    if let Some(units) = print.pointer("/ams/ams").and_then(Value::as_array) {
        for unit in units {
            let ams_id = json_string(unit.get("id"));
            let Some(trays) = unit.get("tray").and_then(Value::as_array) else {
                continue;
            };
            for tray in trays {
                push_slot(&mut slots, &ams_id, tray);
            }
        }
    }
    if let Some(obj) = print.as_object() {
        let mut keys: Vec<_> = obj
            .keys()
            .filter(|key| key.starts_with("vt_tray"))
            .cloned()
            .collect();
        keys.sort();
        for key in keys {
            if let Some(tray) = obj.get(&key) {
                let ams_id = json_string(tray.get("id"));
                let ams_id = if ams_id.is_empty() {
                    key.trim_start_matches("vt_tray").to_string()
                } else {
                    ams_id
                };
                push_slot(&mut slots, &ams_id, tray);
            }
        }
    }
    slots
}

fn push_slot(slots: &mut Vec<AmsSlot>, ams_id: &str, tray: &Value) {
    let kind = json_string(tray.get("tray_type"));
    let color = json_string(tray.get("tray_color"));
    let name = json_string(tray.get("tray_id_name"));
    let info = json_string(tray.get("tray_info_idx"));
    if kind.is_empty()
        && name.is_empty()
        && (color.is_empty() || color.chars().all(|c| c == '0'))
        && info.is_empty()
    {
        return;
    }
    slots.push(AmsSlot {
        ams_id: ams_id.to_string(),
        slot_id: json_string(tray.get("id")),
        kind,
        color,
        name,
        remain: tray.get("remain").map(|value| json_i64(Some(value))),
    });
}

pub fn json_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

pub fn json_i64(value: Option<&Value>) -> i64 {
    let Some(value) = value else {
        return 0;
    };
    if let Some(number) = value.as_i64() {
        return number;
    }
    if let Some(number) = value.as_u64() {
        return number as i64;
    }
    if let Some(number) = value.as_f64() {
        return number as i64;
    }
    let Some(text) = value.as_str() else {
        return 0;
    };
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
        && let Ok(number) = i64::from_str_radix(hex, 16)
    {
        return number;
    }
    text.parse::<i64>().unwrap_or(0)
}

pub fn json_f64(value: Option<&Value>) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    if let Some(number) = value.as_f64() {
        return number;
    }
    if let Some(number) = value.as_i64() {
        return number as f64;
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0.0)
}

pub fn ack_fields<'a>(payload: &'a Value, category: &str) -> Option<&'a Map<String, Value>> {
    payload.get(category)?.as_object()
}

pub fn is_ack(payload: &Value, category: &str, sequence_id: &str) -> bool {
    let Some(obj) = ack_fields(payload, category) else {
        return false;
    };
    if !obj.contains_key("result") {
        return false;
    }
    let seq = obj
        .get("sequence_id")
        .or_else(|| obj.get("sequenceId"))
        .and_then(Value::as_str);
    seq == Some(sequence_id)
}

pub fn ack_result(obj: &Map<String, Value>) -> String {
    json_string(obj.get("result"))
}

pub fn ack_reason(obj: &Map<String, Value>) -> String {
    let reason = json_string(obj.get("reason"));
    if reason.is_empty() {
        ack_result(obj)
    } else {
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_skips_acks_and_keeps_earlier_fields() {
        let mut state = Map::new();
        merge_report(
            &mut state,
            &json!({"print": {"command": "push_status", "msg": 0, "gcode_state": "IDLE", "nozzle_temper": 25.0, "sequence_id": "9"}}),
        );
        merge_report(
            &mut state,
            &json!({"print": {"sequence_id": "1", "result": "success", "reason": "success", "command": "gcode_line"}}),
        );
        merge_report(
            &mut state,
            &json!({"print": {"command": "push_status", "msg": 1, "mc_percent": 12, "gcode_state": "RUNNING"}}),
        );
        let report = report_from_state(&state);
        assert_eq!(report.gcode_state, "RUNNING");
        assert_eq!(report.progress_percent, 12);
        assert_eq!(report.nozzle_c, 25.0);
        assert!(!state.contains_key("result"));
    }

    #[test]
    fn ams_and_virtual_tray_are_listed() {
        let print = json!({
            "gcode_state": "IDLE",
            "print_error": "0x0500C010",
            "spd_lvl": 2,
            "ams": {"ams": [{"id": "0", "tray": [
                {"id": "0"},
                {"id": "1", "tray_type": "PLA", "tray_color": "FF0000FF", "remain": 80}
            ]}]},
            "vt_tray": {"id": "255", "tray_type": "PETG", "tray_color": "FFFFFFFF"}
        });
        let report = report_from_print(&print);
        assert_eq!(report.print_error, 0x0500_C010);
        assert_eq!(report.speed_name, "standard");
        assert_eq!(report.ams.len(), 2);
        assert_eq!(report.ams[0].slot_id, "1");
        assert_eq!(report.ams[1].ams_id, "255");
        assert_eq!(report.ams[1].kind, "PETG");
    }

    #[test]
    fn ack_match_requires_sequence_and_result() {
        let ack = json!({"print": {"sequence_id": "4", "result": "fail", "reason": "mqtt message verify failed"}});
        assert!(is_ack(&ack, "print", "4"));
        assert!(!is_ack(&ack, "print", "5"));
        let status =
            json!({"print": {"command": "push_status", "sequence_id": "4", "gcode_state": "IDLE"}});
        assert!(!is_ack(&status, "print", "4"));
    }
}
