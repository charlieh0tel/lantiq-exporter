# Lantiq/Falcon GPON ONT exporter

A dependency-free Prometheus/OpenMetrics exporter for a **Lantiq (Falcon) GPON
ONT-on-a-stick** — the FS.com `GPON-ONU-34-20BI` / Nokia `G-010G-A` class module
running the OpenWrt/Lantiq 7.5.x firmware.

It runs on a nearby host (a Raspberry Pi, a server — anything with Python 3 and
SSH), reaches the stick **read-only** over SSH on its own interval, caches the
values, and serves them at `/metrics`. Point Netdata (or Prometheus) at that
endpoint to trend optical power, temperature, PON state, and GEM/error counters
over time.

```
  Netdata / Prometheus  ──scrape──▶  ont_exporter.py  ──ssh (read-only)──▶  ONT @ 192.168.1.10
       (every 1s)          /metrics   (polls every 30s, caches)              gtc* getters + A2h DDM
```

The scrape rate is decoupled from the SSH poll, so hammering `/metrics` never
hammers the little mips box.

## Why these numbers

Optics are read from the module's **SFF-8472 A2h real-time diagnostics** (bytes
96–105 of `/dev/sfp_eeprom1`), which are internally calibrated. The decoded TX/RX
optical power match the ONT's own OMCI ANI-G readings exactly, which is the
cross-check that the temperature and Vcc from the same page are trustworthy. PON
state, alarms, and counters come from the Falcon `onu gtc*` getters. Nothing is
written to the stick — no `fw_setenv`, no reboot, no `onu` writes — so it is safe
to run against a stick that is carrying a live WAN.

## Setup

```sh
cp .ont-secret.example .ont-secret
chmod 600 .ont-secret
$EDITOR .ont-secret        # one line: the stick's SSH password

# smoke test — prints metrics once
./ont_exporter.py --once

# run the server (polls every 30s, serves cached /metrics on :9909)
./ont_exporter.py --serve --addr 127.0.0.1 --port 9909 --interval 30
```

Config precedence is flag > env > default:

| flag | env | default |
|---|---|---|
| `--host` | `ONT_HOST` | `192.168.1.10` |
| `--user` | `ONT_USER` | `ONTUSER` |
| `--password-file` | `ONT_PASSWORD_FILE` | `./.ont-secret` |
| — | `ONT_PASS` | (inline password, overrides the file) |

Auth uses OpenSSH's built-in `SSH_ASKPASS` mechanism, so no `sshpass`, `expect`,
or `paramiko` is required. If you prefer key auth, drop a public key in the
stick's `~/.ssh/authorized_keys` and the password file becomes unused.

## Run it as a service

```sh
sudo cp deploy/lantiq-exporter.service /etc/systemd/system/
# edit User / WorkingDirectory / ExecStart paths to match your checkout
sudo systemctl daemon-reload
sudo systemctl enable --now lantiq-exporter
curl -s http://127.0.0.1:9909/metrics | head
```

## Wire into Netdata

Netdata's `go.d/prometheus` collector auto-charts every `ont_*` series.

```sh
sudo cp deploy/netdata-go.d-prometheus.conf /etc/netdata/go.d/prometheus.conf
# or, the netdata-managed way:
#   cd /etc/netdata && sudo ./edit-config go.d/prometheus.conf
sudo systemctl restart netdata
```

Then look under **Prometheus → lantiq_ont** in the Netdata dashboard.

## Wire into Prometheus

```yaml
scrape_configs:
  - job_name: lantiq_ont
    static_configs:
      - targets: ['127.0.0.1:9909']
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
| `ont_tx_gem_bytes_total` | counter | **32-bit, wraps** — use `rate()` (handles the wrap as a reset) |
| `ont_hec_errors_{corrected,uncorrected}_total`, `ont_bip_errors_total` | counter | line error counters |
| `ont_fec_words_*_total`, `ont_fec_seconds_total` | counter | FEC stats (0 while FEC disabled) |
| `ont_allocations_total`, `ont_allocations_lost_total` | counter | upstream bandwidth allocations |
| `ont_rx_gem_frames_dropped_total`, `ont_drop_total`, `ont_omci_drop_total`, `ont_rx_oversized_frames_total` | counter | drops |
| `ont_scrape_duration_seconds`, `ont_last_scrape_timestamp_seconds` | gauge | poll health |

### The most useful things to alert on
- `ont_rx_power_dbm` drifting toward the module's low threshold (fiber/OLT budget).
- `ont_temperature_celsius` — these sticks run hot (~69 °C is normal); warn ~85 °C.
- `ont_gpon_o5 == 0` — the ONT dropped out of the operational state.
- `rate(ont_hec_errors_uncorrected_total[5m]) > 0` — line integrity.

## Security note

`.ont-secret` holds a device password and is gitignored. The exporter only ever
reads from the stick. Bind the HTTP server to `127.0.0.1` (as the systemd unit
does) unless you intend to expose it.

## Other firmware / the GC1601 clone

This targets the SSH-managed Falcon firmware. The sibling `gc1601-ont-clone`
project also has a telnet-managed Nokia clone (`gccli` / `gc_omcicli` on
`192.168.101.1`); adding a telnet backend here would be a natural extension.
