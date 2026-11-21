use serde_json::{Value, json};

use crate::error::{Error, Result};

/// One sliced filament, mapped to a physical AMS slot or a virtual tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilamentMap {
    pub ams_id: u16,
    pub slot_id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapEntry {
    Unused,
    Slot(FilamentMap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum External {
    /// Right nozzle external spool. Virtual tray 255.
    Right,
    /// Left nozzle external spool. Virtual tray 254.
    Left,
}

impl External {
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "right" | "main" | "r" => Ok(External::Right),
            "left" | "deputy" | "l" => Ok(External::Left),
            other => Err(Error::msg(format!(
                "unknown external spool '{other}'. Use left or right."
            ))),
        }
    }

    pub fn entry(self) -> MapEntry {
        let ams_id = match self {
            External::Right => 255,
            External::Left => 254,
        };
        MapEntry::Slot(FilamentMap { ams_id, slot_id: 0 })
    }
}

pub fn parse_ams_spec(spec: &str) -> Result<Vec<MapEntry>> {
    let mut out = Vec::new();
    for raw in spec.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            return Err(Error::msg(
                "empty AMS entry. Use ams:slot per filament, or - for an unused filament. Example: 0:0,0:2",
            ));
        }
        if token == "-" || token.eq_ignore_ascii_case("none") {
            out.push(MapEntry::Unused);
            continue;
        }
        let Some((ams, slot)) = token.split_once(':') else {
            return Err(Error::msg(format!(
                "AMS entry '{token}' is not ams:slot. Example: 0:1 or 254:0 or -"
            )));
        };
        let ams_id = parse_u16(ams, "ams id")?;
        let slot_id = parse_u16(slot, "slot id")?;
        if ams_id > 255 || slot_id > 255 {
            return Err(Error::msg("ams id and slot id must be 0-255"));
        }
        if ams_id == 255 && slot_id == 255 {
            return Err(Error::msg(
                "255:255 means unused. Write - for an unused filament.",
            ));
        }
        out.push(MapEntry::Slot(FilamentMap { ams_id, slot_id }));
    }
    if out.is_empty() {
        return Err(Error::msg("AMS mapping is empty"));
    }
    if out.iter().all(|entry| matches!(entry, MapEntry::Unused)) {
        return Err(Error::msg("every filament is unused"));
    }
    Ok(out)
}

fn parse_u16(text: &str, label: &str) -> Result<u16> {
    text.trim()
        .parse::<u16>()
        .map_err(|_| Error::msg(format!("{label} '{text}' is not an integer")))
}

/// Legacy flat index sent beside `ams_mapping2`.
///
/// A physical AMS uses `ams_id * 4 + slot_id` (AMS 0 slot 2 is 2, AMS 2 slot 0 is 8).
/// Virtual trays, ids 128 and above, use the id itself. 254 is the left external
/// spool and 255 is the right one. Unused filaments are -1.
pub fn legacy_id(entry: &MapEntry) -> i32 {
    match entry {
        MapEntry::Unused => -1,
        MapEntry::Slot(slot) if slot.ams_id >= 128 => i32::from(slot.ams_id),
        MapEntry::Slot(slot) => i32::from(slot.ams_id) * 4 + i32::from(slot.slot_id),
    }
}

pub fn resolve_mapping(
    spec: Option<&str>,
    external: Option<External>,
    filament_count: usize,
) -> Result<Vec<MapEntry>> {
    match (spec, external) {
        (Some(_), Some(_)) => Err(Error::msg("pass either --ams or --external, not both")),
        (Some(spec), None) => {
            let mapping = parse_ams_spec(spec)?;
            if filament_count > 0 && mapping.len() != filament_count {
                return Err(Error::msg(format!(
                    "this plate has {filament_count} filament(s), but the AMS map has {}. One entry per filament, use - for an unused one.",
                    mapping.len()
                )));
            }
            Ok(mapping)
        }
        (None, Some(external)) => {
            let count = if filament_count == 0 {
                1
            } else {
                filament_count
            };
            if count != 1 {
                return Err(Error::msg(format!(
                    "this plate has {count} filaments. --external maps a single filament. Pass --ams ams:slot,... instead."
                )));
            }
            Ok(vec![external.entry()])
        }
        (None, None) => Err(Error::msg(
            "pass --ams ams:slot,... or --external left|right. The printer pauses if the map does not match the sliced filaments.",
        )),
    }
}

pub fn uses_physical_ams(mapping: &[MapEntry]) -> bool {
    mapping.iter().any(|entry| match entry {
        MapEntry::Slot(slot) => slot.ams_id < 128,
        MapEntry::Unused => false,
    })
}

#[derive(Debug, Clone)]
pub struct PrintRequest {
    pub sequence_id: String,
    pub remote_name: String,
    pub ftp_dir: String,
    pub plate: u32,
    pub subtask_name: String,
    pub bed_type: String,
    pub mapping: Vec<MapEntry>,
    pub use_ams: bool,
    pub timelapse: bool,
    pub bed_leveling: bool,
    pub flow_cali: bool,
    pub vibration_cali: bool,
    pub layer_inspect: bool,
}

pub fn project_file_payload(req: &PrintRequest) -> Value {
    let ams_mapping: Vec<i32> = req.mapping.iter().map(legacy_id).collect();
    let ams_mapping2: Vec<Value> = req
        .mapping
        .iter()
        .map(|entry| match entry {
            MapEntry::Unused => json!({"ams_id": 255, "slot_id": 255}),
            MapEntry::Slot(slot) => json!({"ams_id": slot.ams_id, "slot_id": slot.slot_id}),
        })
        .collect();
    json!({
        "print": {
            "sequence_id": req.sequence_id,
            "command": "project_file",
            "param": format!("Metadata/plate_{}.gcode", req.plate),
            "url": mqtt_file_url(&req.ftp_dir, &req.remote_name),
            "file": req.remote_name,
            "md5": "",
            "project_id": "0",
            "profile_id": "0",
            "task_id": "0",
            "subtask_id": "0",
            "subtask_name": req.subtask_name,
            "bed_type": req.bed_type,
            "timelapse": if req.timelapse { 1 } else { 0 },
            // H2 Studio sends the legacy bool false and the mode in auto_bed_leveling.
            "bed_leveling": false,
            "bed_levelling": false,
            "auto_bed_leveling": if req.bed_leveling { 2 } else { 0 },
            "flow_cali": req.flow_cali,
            "vibration_cali": req.vibration_cali,
            "layer_inspect": req.layer_inspect,
            "use_ams": req.use_ams,
            "ams_mapping": ams_mapping,
            "ams_mapping2": ams_mapping2,
            "cfg": "0",
            "extrude_cali_flag": if req.flow_cali { 2 } else { 0 },
            "extrude_cali_manual_mode": 0,
            "nozzle_offset_cali": if req.bed_leveling { 2 } else { 0 },
        }
    })
}

pub fn mqtt_file_url(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        format!("ftp:///{name}")
    } else {
        format!("ftp:///{dir}/{name}")
    }
}

pub fn ftp_url(ip: &str, port: u16, dir: &str, name: &str) -> String {
    if dir.is_empty() {
        format!("ftps://{ip}:{port}/{name}")
    } else {
        format!("ftps://{ip}:{port}/{dir}/{name}")
    }
}

pub fn sanitize_ftp_dir(dir: &str) -> Result<String> {
    let dir = dir.trim().trim_matches('/');
    if dir.is_empty() || dir == "." {
        return Ok(String::new());
    }
    if dir
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
        || !dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
    {
        return Err(Error::msg(
            "ftp directory must be a relative path such as cache, without '..'",
        ));
    }
    Ok(dir.to_string())
}

pub fn remote_name(file_name: &str) -> Result<String> {
    let file_name = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = file_name
        .strip_suffix(".gcode.3mf")
        .or_else(|| file_name.strip_suffix(".3mf"))
        .unwrap_or(file_name);
    let mut clean = String::new();
    let mut prev_underscore = false;
    for ch in stem.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.';
        if ok {
            clean.push(ch);
            prev_underscore = false;
        } else if !prev_underscore {
            clean.push('_');
            prev_underscore = true;
        }
    }
    let clean = clean.trim_matches(|c| c == '_' || c == '.').to_string();
    if clean.is_empty() {
        return Ok("job.gcode.3mf".to_string());
    }
    Ok(format!("{clean}.gcode.3mf"))
}

pub fn subtask_name(remote_name: &str) -> String {
    remote_name
        .strip_suffix(".gcode.3mf")
        .unwrap_or(remote_name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_ids_match_observed_h2_starts() {
        let mapping = parse_ams_spec("128:0,-").unwrap();
        assert_eq!(
            mapping.iter().map(legacy_id).collect::<Vec<_>>(),
            vec![128, -1]
        );
        let mixed = parse_ams_spec("128:0,0:2").unwrap();
        assert_eq!(
            mixed.iter().map(legacy_id).collect::<Vec<_>>(),
            vec![128, 2]
        );
        let second_ams = parse_ams_spec("2:0,0:1,-").unwrap();
        assert_eq!(
            second_ams.iter().map(legacy_id).collect::<Vec<_>>(),
            vec![8, 1, -1]
        );
    }

    #[test]
    fn payload_uses_project_length_mapping() {
        let mapping = parse_ams_spec("0:0,-").unwrap();
        let payload = project_file_payload(&PrintRequest {
            sequence_id: "7".into(),
            remote_name: "cube.gcode.3mf".into(),
            ftp_dir: "cache".into(),
            plate: 1,
            subtask_name: "cube".into(),
            bed_type: "textured_plate".into(),
            mapping,
            use_ams: true,
            timelapse: false,
            bed_leveling: true,
            flow_cali: true,
            vibration_cali: true,
            layer_inspect: true,
        });
        let print = &payload["print"];
        assert_eq!(print["command"], "project_file");
        assert_eq!(print["sequence_id"], "7");
        assert_eq!(print["param"], "Metadata/plate_1.gcode");
        assert_eq!(print["url"], "ftp:///cache/cube.gcode.3mf");
        assert_eq!(print["md5"], "");
        assert_eq!(print["project_id"], "0");
        assert_eq!(print["ams_mapping"], json!([0, -1]));
        assert_eq!(print["ams_mapping2"][0]["ams_id"], 0);
        assert_eq!(print["ams_mapping2"][0]["slot_id"], 0);
        assert_eq!(print["ams_mapping2"][1]["ams_id"], 255);
        assert_eq!(print["ams_mapping2"][1]["slot_id"], 255);
        assert_eq!(print["auto_bed_leveling"], 2);
        assert_eq!(print["bed_leveling"], false);
        assert_eq!(print["extrude_cali_flag"], 2);
    }

    #[test]
    fn external_right_is_virtual_tray_255() {
        let mapping = resolve_mapping(None, Some(External::Right), 1).unwrap();
        assert_eq!(legacy_id(&mapping[0]), 255);
        assert!(resolve_mapping(None, Some(External::Right), 2).is_err());
        assert!(resolve_mapping(None, None, 1).is_err());
    }

    #[test]
    fn remote_name_is_ftp_safe() {
        assert_eq!(remote_name("My Job.gcode.3mf").unwrap(), "My_Job.gcode.3mf");
        assert_eq!(remote_name("a/b.3mf").unwrap(), "b.gcode.3mf");
        assert_eq!(remote_name("***").unwrap(), "job.gcode.3mf");
    }

    #[test]
    fn ftp_dir_rejects_traversal() {
        assert_eq!(sanitize_ftp_dir("cache").unwrap(), "cache");
        assert_eq!(sanitize_ftp_dir("/").unwrap(), "");
        assert!(sanitize_ftp_dir("../cache").is_err());
    }
}
