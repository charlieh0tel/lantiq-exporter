# lantiq-exporter

A tiny Prometheus/OpenMetrics exporter for a **Lantiq (Falcon) GPON
ONT-on-a-stick** — the FS.com `GPON-ONU-34-20BI` / Nokia `G-010G-A` class module
running the OpenWrt/Lantiq 7.5.x firmware.

It runs on a nearby host, reaches the stick **read-only** over SSH on its own
interval, caches the values, and serves them at `/metrics`. A remote
Prometheus or Netdata scrapes that endpoint and trends optical power,
temperature, PON state, and GEM/error counters over time.

```
  Prometheus / Netdata  --scrape-->  lantiq-exporter  --ssh (read-only)-->  ONT @ 192.168.1.10
   (elsewhere on LAN)      /metrics   (polls every 30s, caches)             gtc* getters + A2h DDM
```

The scrape is decoupled from the SSH poll, so hammering `/metrics` never
hammers the little mips box.

- **Single static aarch64 binary, ~390 KB, no runtime deps** beyond libc and the
  system `ssh` client. Written in pure Rust `std` — zero external crates.
- Everything it runs on the stick is **read-only** (a `dd` of the SFP A2h DDM
  image and `onu gtc*` getters). No `fw_setenv`, no reboot, no writes — safe to
  run against a stick carrying a live WAN.

## Why these numbers

Optics come from the module's **SFF-8472 A2h real-time diagnostics** (bytes
96-105 of `/dev/sfp_eeprom1`), which are internally calibrated. The decoded
TX/RX optical power match the ONT's own OMCI ANI-G readings exactly - the
cross-check that the temperature and Vcc from the same page are trustworthy.
PON state, alarms, and counters come from the Falcon `onu gtc*` getters.

## Build

```sh
cargo build --release          # target/release/lantiq-exporter
```

Build the Debian package (needs `cargo-deb`: `cargo install cargo-deb`):

```sh
cargo deb                      # target/debian/lantiq-exporter_<ver>_<arch>.deb
```

## Install

```sh
sudo apt install ./target/debian/lantiq-exporter_*.deb
```

The package creates a system user `lantiq-exporter`, installs the systemd unit,
drops a config file at `/etc/lantiq-exporter/config`, and enables + starts the
service (bound to `0.0.0.0:9909`). It reports `ont_up 0` until you configure
authentication (below) and `systemctl restart lantiq-exporter`.

## Configure

Edit `/etc/lantiq-exporter/config` - `ONT_HOST`, `ONT_USER`, and the poll/serve
args. Then pick one auth method:

**SSH key (recommended for the service):**

```sh
sudo -u lantiq-exporter ssh-keygen -t ed25519 -N '' \
    -f /var/lib/lantiq-exporter/.ssh/id_ed25519
sudo -u lantiq-exporter ssh-copy-id ONTUSER@192.168.1.10
sudo systemctl restart lantiq-exporter
```

**Password (fallback):** set `ONT_PASS=...` in the config, or point
`ONT_PASSWORD_FILE=` at a `chmod 600` file. The exporter feeds it to `ssh` via
OpenSSH's `SSH_ASKPASS` mechanism.

Check it:

```sh
systemctl status lantiq-exporter
curl -s http://127.0.0.1:9909/metrics | grep -E '^ont_(up|rx_power_dbm|gpon_o5) '
```

## Run without installing

```sh
# one-shot: print metrics once (for testing)
./target/release/lantiq-exporter --once --password-file ./.ont-secret

# serve (polls every 30s, serves cached /metrics on :9909)
./target/release/lantiq-exporter --serve --addr 0.0.0.0 --port 9909 --interval 30
```

Config precedence is flag > env > default:

| flag | env | default |
|---|---|---|
| `--host` | `ONT_HOST` | `192.168.1.10` |
| `--user` | `ONT_USER` | `ONTUSER` |
| `--password-file` | `ONT_PASSWORD_FILE` | (unset -> SSH key auth) |
| - | `ONT_PASS` | (inline password, overrides the file) |
| `--addr` / `--port` / `--interval` | - | `0.0.0.0` / `9909` / `30` |

## Scrape it from your monitoring host

The exporter host serves `/metrics` on the LAN at **`<exporter-host>:9909`**.
Nothing runs Prometheus or Netdata here - point whatever does, elsewhere on the
network, at that address. The exporter serves cached values, so scraping it
often does not add SSH load on the stick.

**Prometheus:**

```yaml
scrape_configs:
  - job_name: lantiq_ont
    static_configs:
      - targets: ['<exporter-host>:9909']
```

**Netdata** (`go.d/prometheus` - auto-charts every `ont_*` series):

```yaml
jobs:
  - name: lantiq_ont
    url: http://<exporter-host>:9909/metrics
```

## Metrics

| metric | type | notes |
|---|---|---|
| `ont_up` | gauge | 1 if the last SSH poll succeeded |
| `ont_temperature_celsius` | gauge | module/BOSA temp (A2h) |
| `ont_voltage_volts` | gauge | Vcc supply (A2h) |
| `ont_tx_bias_amperes` | gauge | laser bias current |
| `ont_tx_power_dbm` / `ont_tx_power_microwatts` | gauge | transmit optical power |
| `ont_rx_power_dbm` / `ont_rx_power_microwatts` | gauge | receive optical power |
| `ont_gpon_ds_state` | gauge | GTC downstream state; **3 = O5** (operational) |
| `ont_gpon_o5` | gauge | 1 when in O5 |
| `ont_onu_id` | gauge | ONU-ID from the OLT (255 = unassigned) |
| `ont_ranged_delay`, `ont_onu_response_time`, `ont_gtc_ds_delay` | gauge | ranging/equalization |
| `ont_fec_enabled{direction}` | gauge | FEC on/off per direction |
| `ont_alarm{name}` | gauge | one series per GTC alarm bit (1 = active) |
| `ont_tx_gem_frames_total`, `ont_rx_gem_frames_total` | counter | GEM frame counts |
| `ont_tx_gem_bytes_total` | counter | **32-bit, wraps** - use `rate()` (handles the wrap as a reset) |
| `ont_hec_errors_{corrected,uncorrected}_total`, `ont_bip_errors_total` | counter | line error counters |
| `ont_fec_words_*_total`, `ont_fec_seconds_total` | counter | FEC stats (0 while FEC disabled) |
| `ont_allocations_total`, `ont_allocations_lost_total` | counter | upstream bandwidth allocations |
| `ont_rx_gem_frames_dropped_total`, `ont_drop_total`, `ont_omci_drop_total`, `ont_rx_oversized_frames_total` | counter | drops |
| `ont_scrape_duration_seconds`, `ont_last_scrape_timestamp_seconds` | gauge | poll health |

### The most useful things to alert on
- `ont_rx_power_dbm` drifting toward the module's low threshold (fiber/OLT budget).
- `ont_temperature_celsius` - these sticks run hot (~69 C is normal); warn ~85 C.
- `ont_gpon_o5 == 0` - the ONT dropped out of the operational state.
- `rate(ont_hec_errors_uncorrected_total[5m]) > 0` - line integrity.

## Security notes

- The service runs as the unprivileged `lantiq-exporter` user and only ever
  reads from the stick.
- The `/metrics` endpoint is unauthenticated but exposes only operational
  telemetry (no credentials, no line identity).
