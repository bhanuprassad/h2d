use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct SliceRequest {
    pub bin: String,
    pub input: PathBuf,
    pub output: PathBuf,
    pub printer_profile: PathBuf,
    pub process_profile: PathBuf,
    pub filament_profiles: Vec<PathBuf>,
    /// 0 slices every plate. A positive number slices that plate.
    pub plate: u32,
    pub arrange: bool,
    pub orient: bool,
    pub min_save: bool,
    pub allow_newer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicePlan {
    pub program: String,
    pub args: Vec<String>,
    pub output: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SliceOutcome {
    pub output: PathBuf,
    pub program: String,
}

pub fn plan_slice(req: &SliceRequest) -> Result<SlicePlan> {
    if req.filament_profiles.is_empty() {
        return Err(Error::msg(
            "pass at least one filament profile JSON, or set filament_profiles in the config",
        ));
    }
    if req.output.file_name().is_none() {
        return Err(Error::msg("slice output needs a file name"));
    }
    let output_dir = req
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let export_name = req
        .output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::msg("slice output file name is not valid UTF-8"))?;
    if export_name.contains('/') {
        return Err(Error::msg(
            "slice output file name must not contain a slash",
        ));
    }

    let settings = format!(
        "{};{}",
        req.printer_profile.display(),
        req.process_profile.display()
    );
    let filaments = req
        .filament_profiles
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(";");

    let mut args = vec![
        "--load-settings".to_string(),
        settings,
        "--load-filaments".to_string(),
        filaments,
        "--slice".to_string(),
        req.plate.to_string(),
        "--export-3mf".to_string(),
        export_name.to_string(),
        "--outputdir".to_string(),
        output_dir.display().to_string(),
    ];
    if req.min_save {
        args.push("--min-save".to_string());
    }
    if req.allow_newer {
        args.push("--allow-newer-file".to_string());
    }
    if req.arrange {
        args.push("--arrange".to_string());
        args.push("1".to_string());
    }
    if req.orient {
        args.push("--orient".to_string());
        args.push("1".to_string());
    }
    args.push(req.input.display().to_string());

    Ok(SlicePlan {
        program: req.bin.clone(),
        args,
        output: req.output.clone(),
    })
}

pub fn slice(req: &SliceRequest, capture: bool) -> Result<SliceOutcome> {
    require_file(&req.input, "model")?;
    require_file(&req.printer_profile, "printer profile")?;
    require_file(&req.process_profile, "process profile")?;
    for filament in &req.filament_profiles {
        require_file(filament, "filament profile")?;
    }
    if let Some(parent) = req.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let plan = plan_slice(req)?;
    let mut command = Command::new(&plan.program);
    command.args(&plan.args).stdin(Stdio::null());
    if capture {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }
    let child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            Error::msg(format!(
                "could not run '{}'. Install OrcaSlicer or set orca_bin in the config.",
                plan.program
            ))
        } else {
            Error::from(err)
        }
    })?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let detail = if capture {
            tail_output(&output.stderr, &output.stdout)
        } else {
            String::new()
        };
        return Err(Error::msg(format!(
            "OrcaSlicer exited with {:?}. {detail}",
            output.status.code()
        )));
    }
    if !plan.output.is_file() {
        return Err(Error::msg(format!(
            "OrcaSlicer finished but {} was not written. Check --outputdir handling in the slicer version you have.",
            plan.output.display()
        )));
    }
    Ok(SliceOutcome {
        output: plan.output,
        program: plan.program,
    })
}

fn tail_output(stderr: &[u8], stdout: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.is_empty() {
        text = String::from_utf8_lossy(stdout).trim().to_string();
    }
    let tail: String = text
        .chars()
        .rev()
        .take(1200)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    tail
}

fn require_file(path: &Path, label: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(Error::msg(format!("{label} not found: {}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_name_is_relative_to_outputdir() {
        let plan = plan_slice(&SliceRequest {
            bin: "orca-slicer".into(),
            input: PathBuf::from("/models/cube.stl"),
            output: PathBuf::from("/tmp/out/cube.gcode.3mf"),
            printer_profile: PathBuf::from("/profiles/h2d.json"),
            process_profile: PathBuf::from("/profiles/020.json"),
            filament_profiles: vec![PathBuf::from("/profiles/pla.json")],
            plate: 1,
            arrange: true,
            orient: false,
            min_save: true,
            allow_newer: true,
        })
        .unwrap();
        assert!(plan.args.contains(&"--min-save".to_string()));
        let export = plan
            .args
            .iter()
            .position(|arg| arg == "--export-3mf")
            .unwrap();
        assert_eq!(plan.args[export + 1], "cube.gcode.3mf");
        let dir = plan
            .args
            .iter()
            .position(|arg| arg == "--outputdir")
            .unwrap();
        assert_eq!(plan.args[dir + 1], "/tmp/out");
        let settings = plan
            .args
            .iter()
            .position(|arg| arg == "--load-settings")
            .unwrap();
        assert_eq!(
            plan.args[settings + 1],
            "/profiles/h2d.json;/profiles/020.json"
        );
        assert_eq!(
            plan.args.last().map(String::as_str),
            Some("/models/cube.stl")
        );
        assert!(plan.args.contains(&"--arrange".to_string()));
    }
}
