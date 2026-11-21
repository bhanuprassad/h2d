use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use zip::ZipArchive;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FilamentInfo {
    pub id: String,
    pub kind: String,
    pub color: String,
    pub used_g: String,
    pub tray_info_idx: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlateInfo {
    pub plate: u32,
    pub gcode_present: bool,
    pub gcode_bytes: u64,
    pub bed_type: Option<String>,
    pub md5: Option<String>,
    pub filaments: Vec<FilamentInfo>,
}

pub fn inspect_path(path: &Path, plate: u32) -> Result<PlateInfo> {
    if plate == 0 {
        return Err(Error::msg(
            "plate numbers start at 1. Use the plate you sliced.",
        ));
    }
    let file = File::open(path)
        .map_err(|err| Error::msg(format!("could not open {}: {err}", path.display())))?;
    let mut archive = ZipArchive::new(file).map_err(|err| {
        Error::msg(format!(
            "{} is not a 3MF zip archive: {err}",
            path.display()
        ))
    })?;
    inspect_archive(&mut archive, plate)
}

fn inspect_archive<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    plate: u32,
) -> Result<PlateInfo> {
    let gcode_name = format!("Metadata/plate_{plate}.gcode");
    let gcode = read_named(archive, &gcode_name, Some(16 * 1024))?;
    let gcode_bytes = entry_size(archive, &gcode_name)?.unwrap_or(0);
    let json_bytes = read_named(archive, &format!("Metadata/plate_{plate}.json"), None)?;
    let md5_bytes = read_named(archive, &format!("Metadata/plate_{plate}.gcode.md5"), None)?;
    let slice_info = read_named(archive, "Metadata/slice_info.config", None)?;

    let mut filaments = Vec::new();
    if let Some(bytes) = &slice_info {
        let xml = String::from_utf8_lossy(bytes);
        filaments = filaments_from_slice_info(&xml, plate);
    }
    if filaments.is_empty()
        && let Some(bytes) = &gcode
    {
        filaments = filaments_from_gcode(&String::from_utf8_lossy(bytes));
    }

    let bed_type = json_bytes
        .as_ref()
        .and_then(|bytes| bed_type_from_plate_json(bytes));
    let md5 = md5_bytes.map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .trim()
            .trim_matches(char::from(0))
            .to_string()
    });

    Ok(PlateInfo {
        plate,
        gcode_present: gcode.is_some(),
        gcode_bytes,
        bed_type,
        md5: md5.filter(|text| !text.is_empty()),
        filaments,
    })
}

pub fn require_sliced(info: &PlateInfo, path: &Path) -> Result<()> {
    if info.gcode_present {
        return Ok(());
    }
    Err(Error::msg(format!(
        "{} has no Metadata/plate_{}.gcode. This is a project file, not a sliced plate. Slice it with `h2d slice` (OrcaSlicer --export-3mf --min-save) and print the .gcode.3mf.",
        path.display(),
        info.plate
    )))
}

fn entry_size<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Option<u64>> {
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| Error::msg(err.to_string()))?;
        if entry_name(entry.name()) == name {
            return Ok(Some(entry.size()));
        }
    }
    Ok(None)
}

fn read_named<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    limit: Option<u64>,
) -> Result<Option<Vec<u8>>> {
    let mut found = None;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| Error::msg(err.to_string()))?;
        if entry_name(entry.name()) == name {
            found = Some(index);
            break;
        }
    }
    let Some(index) = found else {
        return Ok(None);
    };
    let mut entry = archive
        .by_index(index)
        .map_err(|err| Error::msg(err.to_string()))?;
    let mut buf = Vec::new();
    if let Some(limit) = limit {
        (&mut entry).take(limit).read_to_end(&mut buf)?;
    } else {
        entry.read_to_end(&mut buf)?;
    }
    Ok(Some(buf))
}

fn entry_name(name: &str) -> &str {
    name.trim_start_matches('/')
}

fn bed_type_from_plate_json(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let bed = value.get("bed_type")?.as_str()?.trim();
    if bed.is_empty() {
        None
    } else {
        Some(bed.to_string())
    }
}

fn filaments_from_slice_info(xml: &str, plate: u32) -> Vec<FilamentInfo> {
    let blocks: Vec<&str> = xml
        .split("<plate>")
        .skip(1)
        .map(|chunk| chunk.split("</plate>").next().unwrap_or(chunk))
        .collect();
    if blocks.is_empty() {
        return filament_tags(xml);
    }
    for block in &blocks {
        if plate_index(block) == Some(plate) {
            return filament_tags(block);
        }
    }
    // A single plate block with no index still belongs to the requested plate.
    if blocks.len() == 1 && plate_index(blocks[0]).is_none() {
        return filament_tags(blocks[0]);
    }
    Vec::new()
}

fn plate_index(block: &str) -> Option<u32> {
    let key = block.find("key=\"index\"")?;
    let rest = &block[key..];
    let value = attr(rest, "value")?;
    value.parse().ok()
}

fn filament_tags(block: &str) -> Vec<FilamentInfo> {
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(start) = rest.find("<filament ") {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..end];
        rest = &rest[end..];
        out.push(FilamentInfo {
            id: attr(tag, "id").unwrap_or_default().to_string(),
            kind: attr(tag, "type").unwrap_or_default().to_string(),
            color: attr(tag, "color").unwrap_or_default().to_string(),
            used_g: attr(tag, "used_g").unwrap_or_default().to_string(),
            tray_info_idx: attr(tag, "tray_info_idx").unwrap_or_default().to_string(),
        });
    }
    out
}

fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let index = tag.find(&needle)?;
    let rest = &tag[index + needle.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn filaments_from_gcode(header: &str) -> Vec<FilamentInfo> {
    let mut kinds: Vec<&str> = Vec::new();
    let mut colors: Vec<&str> = Vec::new();
    for line in header.lines().take(400) {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("; filament_type =") {
            kinds = split_list(value);
        } else if let Some(value) = line.strip_prefix("; filament_colour =") {
            colors = split_list(value);
        } else if let Some(value) = line.strip_prefix("; filament_color =") {
            colors = split_list(value);
        }
    }
    let count = kinds.len().max(colors.len());
    let mut out = Vec::new();
    for index in 0..count {
        out.push(FilamentInfo {
            id: (index + 1).to_string(),
            kind: kinds.get(index).copied().unwrap_or("").to_string(),
            color: colors.get(index).copied().unwrap_or("").to_string(),
            used_g: String::new(),
            tray_info_idx: String::new(),
        });
    }
    out
}

fn split_list(value: &str) -> Vec<&str> {
    value
        .trim()
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn sample_archive() -> ZipArchive<Cursor<Vec<u8>>> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut cursor);
            let opts = SimpleFileOptions::default();
            writer.start_file("Metadata/plate_1.gcode", opts).unwrap();
            writer
                .write_all(b"; filament_type = PLA\nG1 X0 Y0\n")
                .unwrap();
            writer.start_file("Metadata/plate_1.json", opts).unwrap();
            writer
                .write_all(br##"{"bed_type":"textured_plate","filament_colors":["#FFFFFF"]}"##)
                .unwrap();
            writer
                .start_file("Metadata/plate_1.gcode.md5", opts)
                .unwrap();
            writer.write_all(b"ABCDEF\n").unwrap();
            writer
                .start_file("Metadata/slice_info.config", opts)
                .unwrap();
            writer
                .write_all(
                    br##"<config><plate>
                    <metadata key="index" value="1"/>
                    <filament id="1" tray_info_idx="GFA00" type="PLA" color="#FFFFFF" used_g="1.25"/>
                    </plate></config>"##,
                )
                .unwrap();
            writer.finish().unwrap();
        }
        cursor.set_position(0);
        ZipArchive::new(cursor).unwrap()
    }

    #[test]
    fn reads_sliced_plate() {
        let mut archive = sample_archive();
        let info = inspect_archive(&mut archive, 1).unwrap();
        assert!(info.gcode_present);
        assert_eq!(info.bed_type.as_deref(), Some("textured_plate"));
        assert_eq!(info.md5.as_deref(), Some("ABCDEF"));
        assert_eq!(info.filaments.len(), 1);
        assert_eq!(info.filaments[0].kind, "PLA");
        assert_eq!(info.filaments[0].used_g, "1.25");
    }

    #[test]
    fn missing_plate_is_not_an_error() {
        let mut archive = sample_archive();
        let info = inspect_archive(&mut archive, 2).unwrap();
        assert!(!info.gcode_present);
        assert!(info.filaments.is_empty());
    }
}
