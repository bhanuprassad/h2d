# h2d

A Linux CLI for a Bambu Lab H2D. Slice a model, send it, and start the print without opening Studio. The same program can sit behind a local agent as an MCP server.

On the printer, turn on LAN Only and Developer Mode, then copy the access code from that screen. Handy over the internet stays disconnected while Developer Mode is on.

## Build

```bash
cargo build --release
```

Upload needs `curl`. Slicing needs `orca-slicer` on your PATH. A file that is already a sliced `.gcode.3mf` does not need the slicer.

## Setup

```bash
install -m 600 /dev/stdin ~/.config/h2d/access.code
# paste the access code, then Ctrl-D

h2d config set \
  --ip 192.168.1.50 \
  --serial YOUR_SERIAL \
  --access-code-file ~/.config/h2d/access.code

h2d doctor
h2d status
```

The access code stays in that file. It is not saved in the config.

## Print

```bash
h2d slice model.stl \
  --printer-profile printer.json \
  --process-profile process.json \
  --filament-profile pla.json \
  --output job.gcode.3mf

h2d print job.gcode.3mf --external right --dry-run
h2d print job.gcode.3mf --external right

h2d pause
h2d resume
h2d stop
```

`--external right` is the spool on the right nozzle. `--external left` is the left. For the AMS, give one slot per filament: `--ams 0:0`, or `--ams 0:0,0:2,-` when a filament is unused.

Read the dry run before the first real job, and make that job a small one. `h2d status` is the first command to try on the printer.

Add `--json` when a script or an agent should read the result. Add `--watch` to wait until the job finishes.

## Agents

```bash
h2d mcp
```

```json
{
  "mcpServers": {
    "h2d": {
      "command": "/path/to/h2d",
      "args": ["mcp"]
    }
  }
}
```

An agent cannot start a print unless it passes `confirm: true`.

## Notes

No cloud login. If Developer Mode is off, the printer will refuse the start. Camera, lights, and jogging are not in this version. Try `h2d status` on your own machine before you trust a print.
