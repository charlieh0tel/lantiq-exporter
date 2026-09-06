#!/usr/bin/env python3
"""
ont_exporter.py — Prometheus/OpenMetrics exporter for the Lantiq/Falcon
GPON ONT-on-a-stick (FS.com GPON-ONU-34-20BI / Nokia G-010G-A clone).

It runs on a host that can reach the stick, SSHes in read-only on its own
interval, caches the parsed values, and serves them at /metrics. Netdata's
go.d/prometheus collector (or Prometheus itself) scrapes that endpoint.

Design notes
------------
- The scrape is decoupled from the SSH poll: /metrics returns cached values
  instantly, and a background thread refreshes them every --interval seconds.
  This keeps SSH load off the little mips box regardless of how often Netdata
  scrapes (Netdata defaults to every 1s).
- Everything it runs on the stick is READ-ONLY (dd of the SFP A2h DDM image,
  `onu gtc*` getters). No fw_setenv, no reboots, no writes.
- No third-party Python deps. SSH auth uses OpenSSH's SSH_ASKPASS mechanism,
  so no sshpass/paramiko needed.

Optics come from the module's SFF-8472 A2h real-time diagnostics (bytes
96..105), which are internally calibrated — the decoded TX/RX power match the
OMCI ANI-G readings exactly, so the temperature and Vcc from the same page are
trustworthy too.

Usage
-----
    # one-shot: print metrics once (for testing / a textfile collector)
    ./ont_exporter.py --once

    # serve on :9909 (default), poll the stick every 30s
    ./ont_exporter.py --serve --port 9909 --interval 30

Config (flags override env override defaults):
    --host      ONT_HOST      (default 192.168.1.10)
    --user      ONT_USER      (default ONTUSER)
    --password-file ONT_PASSWORD_FILE (default ./.ont-secret)
                or  ONT_PASS  (password inline in env)
"""
import argparse
import math
import os
import re
import stat
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULTS = {
    "host": os.environ.get("ONT_HOST", "192.168.1.10"),
    "user": os.environ.get("ONT_USER", "ONTUSER"),
    "password_file": os.environ.get("ONT_PASSWORD_FILE", os.path.join(HERE, ".ont-secret")),
}

# One batched remote script: label each block so the parser is order-independent.
# PATH is extended because the lantiq tools are not in the login PATH.
REMOTE_SCRIPT = r"""
PATH=$PATH:/opt/lantiq/bin
printf '@ddm '
dd if=/dev/sfp_eeprom1 bs=1 skip=96 count=10 2>/dev/null | hexdump -C | head -1
printf '@gtcsg ';  onu gtcsg  2>/dev/null
printf '@gtcag ';  onu gtcag  2>/dev/null
printf '@gtcrg ';  onu gtcrg  2>/dev/null
printf '@gtctcg '; onu gtctcg 2>/dev/null
"""


# ----------------------------------------------------------------------------
# SSH transport (password via SSH_ASKPASS; no external tools required)
# ----------------------------------------------------------------------------
def load_password(cfg):
    if os.environ.get("ONT_PASS"):
        return os.environ["ONT_PASS"]
    pf = cfg["password_file"]
    try:
        with open(pf) as f:
            return f.read().strip()
    except OSError as e:
        sys.stderr.write(
            "cannot read password: set ONT_PASS, or put it in %s (chmod 600)\n  %s\n"
            % (pf, e)
        )
        sys.exit(2)


def ssh_run(cfg, password, script, timeout=25):
    """Run `script` on the stick, return (ok, stdout, stderr)."""
    askpass = tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False)
    try:
        askpass.write("#!/bin/sh\nprintf '%s\\n' \"$ONT_ASKPASS_VALUE\"\n")
        askpass.close()
        os.chmod(askpass.name, stat.S_IRWXU)
        env = dict(os.environ)
        env["ONT_ASKPASS_VALUE"] = password
        env["SSH_ASKPASS"] = askpass.name
        env["SSH_ASKPASS_REQUIRE"] = "force"
        env.pop("DISPLAY", None)
        cmd = [
            "setsid", "-w", "ssh",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=10",
            "-o", "NumberOfPasswordPrompts=1",
            "-o", "PreferredAuthentications=password,keyboard-interactive",
            "%s@%s" % (cfg["user"], cfg["host"]),
            script,
        ]
        p = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=timeout
        )
        return p.returncode == 0, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return False, "", "ssh timeout"
    finally:
        try:
            os.unlink(askpass.name)
        except OSError:
            pass


# ----------------------------------------------------------------------------
# Parsing
# ----------------------------------------------------------------------------
def _kv(line):
    """Parse 'a=1 b=2 c="x y"' into a dict of strings."""
    out = {}
    for m in re.finditer(r'(\w+)=("(?:[^"]*)"|\S+)', line):
        out[m.group(1)] = m.group(2).strip('"')
    return out


def parse_ddm(ddm_line):
    """@ddm line is a `hexdump -C` row: extract the 10 diagnostic bytes."""
    # strip leading offset and trailing |ascii|; keep the hex byte columns
    body = ddm_line.split("|", 1)[0]
    toks = body.split()
    # first token is the 8-hex-digit offset from hexdump -C
    if toks and re.fullmatch(r"[0-9a-fA-F]{8}", toks[0]):
        toks = toks[1:]
    hexbytes = [t for t in toks if re.fullmatch(r"[0-9a-fA-F]{2}", t)][:10]
    if len(hexbytes) < 10:
        return None
    b = [int(x, 16) for x in hexbytes]

    def u16(i):
        return (b[i] << 8) | b[i + 1]

    def s16(i):
        v = u16(i)
        return v - 0x10000 if v & 0x8000 else v

    temp_c = s16(0) / 256.0
    vcc_v = u16(2) * 0.0001
    txbias_a = u16(4) * 2e-6           # 2 uA / LSB
    tx_uw = u16(6) * 0.1               # 0.1 uW / LSB
    rx_uw = u16(8) * 0.1
    d = {
        "temperature_celsius": temp_c,
        "voltage_volts": vcc_v,
        "tx_bias_amperes": txbias_a,
        "tx_power_microwatts": tx_uw,
        "rx_power_microwatts": rx_uw,
    }
    if tx_uw > 0:
        d["tx_power_dbm"] = 10.0 * math.log10(tx_uw / 1000.0)
    if rx_uw > 0:
        d["rx_power_dbm"] = 10.0 * math.log10(rx_uw / 1000.0)
    return d


def parse(stdout):
    """Turn the batched remote output into a flat metrics dict + labels."""
    m = {"up": 1}
    labels = {}
    sections = {}
    for line in stdout.splitlines():
        line = line.strip()
        for tag in ("@ddm", "@gtcsg", "@gtcag", "@gtcrg", "@gtctcg"):
            if line.startswith(tag):
                sections[tag] = line[len(tag):].strip()

    if "@ddm" in sections:
        ddm = parse_ddm(sections["@ddm"])
        if ddm:
            m.update(ddm)

    if "@gtcsg" in sections:
        g = _kv(sections["@gtcsg"])
        if g.get("ds_state") is not None:
            m["gpon_ds_state"] = int(g["ds_state"])
            m["gpon_o5"] = 1 if int(g["ds_state"]) == 3 else 0
        for src, dst in (("onu_id", "onu_id"),
                         ("onu_response_time", "onu_response_time"),
                         ("gtc_ds_delay", "gtc_ds_delay")):
            if src in g:
                m[dst] = int(g[src])
        for src, direction in (("ds_fec_enable", "ds"), ("us_fec_enable", "us")):
            if src in g:
                m.setdefault("fec_enabled", {})[direction] = int(g[src])

    if "@gtcrg" in sections:
        g = _kv(sections["@gtcrg"])
        if "ranged_delay" in g:
            m["ranged_delay"] = int(g["ranged_delay"])

    if "@gtcag" in sections:
        g = _kv(sections["@gtcag"])
        alarms = {}
        for k, v in g.items():
            if k == "errorcode":
                continue
            try:
                alarms[k] = int(v)
            except ValueError:
                pass
        if alarms:
            m["alarm"] = alarms

    if "@gtctcg" in sections:
        g = _kv(sections["@gtctcg"])
        # (remote key, exported counter base name)
        cmap = {
            "tx_gem_frames_total": "tx_gem_frames_total",
            "tx_gem_bytes_total": "tx_gem_bytes_total",
            "tx_gem_idle_frames_total": "tx_gem_idle_frames_total",
            "rx_gem_frames_total": "rx_gem_frames_total",
            "rx_gem_bytes_total": "rx_gem_bytes_total",
            "rx_gem_frames_dropped": "rx_gem_frames_dropped_total",
            "rx_oversized_frames": "rx_oversized_frames_total",
            "hec_error_corr": "hec_errors_corrected_total",
            "hec_error_uncorr": "hec_errors_uncorrected_total",
            "bip": "bip_errors_total",
            "fec_words_corr": "fec_words_corrected_total",
            "fec_words_uncorr": "fec_words_uncorrected_total",
            "fec_words_total": "fec_words_total",
            "fec_seconds": "fec_seconds_total",
            "allocations_total": "allocations_total",
            "allocations_lost": "allocations_lost_total",
            "drop": "drop_total",
            "omci_drop": "omci_drop_total",
        }
        counters = {}
        for src, dst in cmap.items():
            if src in g:
                try:
                    counters[dst] = int(g[src])
                except ValueError:
                    pass
        if counters:
            m["counters"] = counters

    return m, labels


# ----------------------------------------------------------------------------
# Prometheus text rendering
# ----------------------------------------------------------------------------
GAUGE_HELP = {
    "ont_up": "1 if the last poll of the ONT succeeded, else 0",
    "ont_temperature_celsius": "Module/BOSA temperature (SFF-8472 A2h)",
    "ont_voltage_volts": "Module supply voltage Vcc (SFF-8472 A2h)",
    "ont_tx_bias_amperes": "Laser TX bias current",
    "ont_tx_power_dbm": "Transmit optical power",
    "ont_rx_power_dbm": "Receive optical power",
    "ont_tx_power_microwatts": "Transmit optical power (linear)",
    "ont_rx_power_microwatts": "Receive optical power (linear)",
    "ont_gpon_ds_state": "GPON GTC downstream state (3 = O5 operational)",
    "ont_gpon_o5": "1 if the ONT is in O5 (operational), else 0",
    "ont_onu_id": "ONU-ID assigned by the OLT (255 = unassigned)",
    "ont_onu_response_time": "ONU response time (equalization)",
    "ont_gtc_ds_delay": "GTC downstream delay",
    "ont_ranged_delay": "Ranged (equalization) delay set by the OLT",
    "ont_fec_enabled": "FEC enabled (1) per direction",
    "ont_alarm": "GTC alarm bit state (1 = active)",
    "ont_scrape_duration_seconds": "How long the last successful SSH poll took",
    "ont_last_scrape_timestamp_seconds": "Unix time of the last successful poll",
}


def render(metrics):
    out = []
    seen_help = set()

    def emit(name, value, labels=None, mtype="gauge"):
        if name not in seen_help:
            if name in GAUGE_HELP:
                out.append("# HELP %s %s" % (name, GAUGE_HELP[name]))
            out.append("# TYPE %s %s" % (name, mtype))
            seen_help.add(name)
        lbl = ""
        if labels:
            lbl = "{%s}" % ",".join(
                '%s="%s"' % (k, str(v).replace('\\', '\\\\').replace('"', '\\"'))
                for k, v in labels.items()
            )
        if isinstance(value, float):
            sval = "%.6g" % value
        else:
            sval = str(value)
        out.append("%s%s %s" % (name, lbl, sval))

    emit("ont_up", metrics.get("up", 0))

    scalar = [
        ("temperature_celsius", "ont_temperature_celsius"),
        ("voltage_volts", "ont_voltage_volts"),
        ("tx_bias_amperes", "ont_tx_bias_amperes"),
        ("tx_power_dbm", "ont_tx_power_dbm"),
        ("rx_power_dbm", "ont_rx_power_dbm"),
        ("tx_power_microwatts", "ont_tx_power_microwatts"),
        ("rx_power_microwatts", "ont_rx_power_microwatts"),
        ("gpon_ds_state", "ont_gpon_ds_state"),
        ("gpon_o5", "ont_gpon_o5"),
        ("onu_id", "ont_onu_id"),
        ("onu_response_time", "ont_onu_response_time"),
        ("gtc_ds_delay", "ont_gtc_ds_delay"),
        ("ranged_delay", "ont_ranged_delay"),
    ]
    for key, name in scalar:
        if key in metrics:
            emit(name, metrics[key])

    for direction, val in metrics.get("fec_enabled", {}).items():
        emit("ont_fec_enabled", val, {"direction": direction})

    for name, val in metrics.get("alarm", {}).items():
        emit("ont_alarm", val, {"name": name})

    for name, val in metrics.get("counters", {}).items():
        emit("ont_" + name, val, mtype="counter")

    if "scrape_duration_seconds" in metrics:
        emit("ont_scrape_duration_seconds", metrics["scrape_duration_seconds"])
    if "last_scrape_timestamp_seconds" in metrics:
        emit("ont_last_scrape_timestamp_seconds", metrics["last_scrape_timestamp_seconds"])

    return "\n".join(out) + "\n"


# ----------------------------------------------------------------------------
# Poll + serve
# ----------------------------------------------------------------------------
def poll_once(cfg, password):
    t0 = time.time()
    ok, stdout, stderr = ssh_run(cfg, password, REMOTE_SCRIPT)
    if not ok:
        return {"up": 0}, stderr.strip()
    metrics, _ = parse(stdout)
    metrics["scrape_duration_seconds"] = round(time.time() - t0, 3)
    metrics["last_scrape_timestamp_seconds"] = int(time.time())
    return metrics, ""


class Cache:
    def __init__(self):
        self.lock = threading.Lock()
        self.text = "ont_up 0\n"

    def set(self, text):
        with self.lock:
            self.text = text

    def get(self):
        with self.lock:
            return self.text


def poller(cfg, password, cache, interval, stop):
    while not stop.is_set():
        metrics, err = poll_once(cfg, password)
        if err:
            sys.stderr.write("[poll] error: %s\n" % err)
        cache.set(render(metrics))
        stop.wait(interval)


def make_handler(cache):
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path.split("?")[0] not in ("/metrics", "/"):
                self.send_error(404)
                return
            body = cache.get().encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    return H


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default=DEFAULTS["host"])
    ap.add_argument("--user", default=DEFAULTS["user"])
    ap.add_argument("--password-file", default=DEFAULTS["password_file"])
    ap.add_argument("--once", action="store_true", help="print metrics once and exit")
    ap.add_argument("--serve", action="store_true", help="run the HTTP /metrics server")
    ap.add_argument("--port", type=int, default=9909)
    ap.add_argument("--addr", default="0.0.0.0")
    ap.add_argument("--interval", type=int, default=30, help="poll interval (s)")
    args = ap.parse_args()

    cfg = {"host": args.host, "user": args.user, "password_file": args.password_file}
    password = load_password(cfg)

    if args.once or not args.serve:
        metrics, err = poll_once(cfg, password)
        if err:
            sys.stderr.write("[poll] error: %s\n" % err)
        sys.stdout.write(render(metrics))
        return 0 if metrics.get("up") else 1

    cache = Cache()
    stop = threading.Event()
    t = threading.Thread(target=poller, args=(cfg, password, cache, args.interval, stop),
                         daemon=True)
    t.start()
    httpd = ThreadingHTTPServer((args.addr, args.port), make_handler(cache))
    sys.stderr.write("serving ONT metrics on http://%s:%d/metrics (poll %ds)\n"
                     % (args.addr, args.port, args.interval))
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
    return 0


if __name__ == "__main__":
    sys.exit(main())
