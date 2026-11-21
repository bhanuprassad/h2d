# h2d

LAN client for a Bambu Lab H2D. It slices with the OrcaSlicer CLI, uploads the plate file over FTPS, and starts the job over MQTT. The same binary speaks stdio MCP, so a local agent can call it without Bambu Studio.

This is not a kernel driver. The printer is already a network device.

## Printer setup

On the H2D screen:

1. Settings, LAN Only, on.
2. Developer Mode, on.

Developer Mode is the third-party control path Bambu documents. While it is on, Bambu Cloud and the Handy app over the internet are disconnected. Read the access code on that same screen. It is the password for MQTT and FTPS. The username is `bblp`.

Unsigned print commands are rejected when Developer Mode is off. This tool does not sign them.

## Build

```bash
cargo build --release
# binary: target/release/h2d
```

Upload uses `curl` (implicit FTPS on port 990 needs TLS session reuse). Slicing uses `orca-slicer` on `PATH`, or `orca_bin` in the config.

## Configure

```bash
install -m 600 /dev/stdin ~/.config/h2d/access.code
# paste the access code, then Ctrl-D

h2d config set \
  --ip 192.168.1.50 \
  --serial 01P00C0EXAMPLE \
  --access-code-file ~/.config/h2d/access.code
```

The config file is `~/.config/h2d/config.toml` (or `$XDG_CONFIG_HOME/h2d/config.toml`). The access code is not written there.

Runtime overrides, highest first: flags, environment, config file.

- `H2D_IP`
- `H2D_SERIAL`
- `H2D_ACCESS_CODE_FILE`
- `H2D_ACCESS_CODE` (prefer the file)
- `H2D_ORCA`
- `H2D_CURL`
- `H2D_CONFIG`

`h2d doctor` checks the file, the code, `curl`, OrcaSlicer, and whether TCP connects to ports 8883 and 990.

## Commands

```bash
h2d status
h2d status --json

h2d inspect job.gcode.3mf --plate 1

h2d slice model.stl \
  --printer-profile ~/.config/h2d/profiles/h2d.json \
  --process-profile ~/.config/h2d/profiles/0.20.json \
  --filament-profile ~/.config/h2d/profiles/pla.json \
  --output job.gcode.3mf

h2d print job.gcode.3mf --external right --dry-run
h2d print job.gcode.3mf --ams 0:0 --watch

h2d pause
h2d resume
h2d stop
```

`--ams` is one `ams:slot` per sliced filament, comma-separated. `-` marks an unused filament: `--ams 0:0,0:2,-`. `--external right` is virtual tray 255 (right nozzle). `--external left` is 254. A multi-filament plate needs `--ams`.

`--json` prints one JSON object. With `--watch`, progress is JSON lines (`"event":"progress"`) and the last line is the result. Exit `0` when the job is accepted or finishes, `2` when the printer rejects the start or the job fails, `3` when `--watch` times out.

Profiles are OrcaSlicer preset JSON files (printer, process, filament). Export them from Orca. The slicer is invoked with `--slice`, `--export-3mf`, `--min-save`, and `--outputdir`. `--min-save` is what produces the plate file the printer accepts. A project `.3mf` that still contains meshes is refused.

The first real job should be a small single-filament print, after a `--dry-run` you have read.

## MCP

```bash
h2d mcp
```

The process reads and writes newline-delimited JSON-RPC on stdin and stdout. Tools: `h2d_doctor`, `h2d_status`, `h2d_inspect`, `h2d_slice`, `h2d_print`, `h2d_pause`, `h2d_resume`, `h2d_stop`.

`h2d_print` does not upload or start unless `confirm` is true. `dry_run: true` previews the MQTT payload.

```json
{
  "mcpServers": {
    "h2d": {
      "command": "/absolute/path/to/h2d",
      "args": ["mcp"]
    }
  }
}
```

## What a start sends

The sliced file is uploaded to `cache/<name>.gcode.3mf`. The MQTT command is `print.project_file` on `device/<serial>/request`, with `url` `ftp:///cache/<name>.gcode.3mf` and `param` `Metadata/plate_N.gcode`.

H2D mapping is project-length. `ams_mapping2` is `{ams_id, slot_id}` per filament. The legacy `ams_mapping` value is `ams_id * 4 + slot_id` for a physical AMS, and the id itself for virtual trays (128 and above). Calibration fields follow a captured H2 Studio start: `auto_bed_leveling` 2 and `extrude_cali_flag` 2 when those calibrations are requested. Confirm that on your machine with `--dry-run`, then one small print.

## Limits

- No cloud login, and no MQTT signing. Developer Mode stays the way in.
- The slicer is OrcaSlicer, not code in this repo. H2D presets have to come from the Orca build you install.
- Camera, lights, jogging, and file listing are not in this version.
- This tree was tested with `cargo test`. It has not been run against a live H2D here. `h2d status --json` is the first command to run on the printer.
