// lantiq-exporter — Prometheus/OpenMetrics exporter for a Lantiq (Falcon) GPON
// ONT-on-a-stick (FS.com GPON-ONU-34-20BI / Nokia G-010G-A clone, OpenWrt/Lantiq
// 7.5.x firmware).
//
// Runs on a nearby host, reaches the stick READ-ONLY over SSH on its own
// interval, caches the parsed values, and serves them at /metrics for a remote
// Prometheus/Netdata to scrape. Nothing is written to the stick.
//
// Pure std — no external crates. SSH is done by shelling out to the system
// `ssh` client: with an SSH key (recommended for the service) it just works;
// with a password it uses sshpass (a runtime dependency).
//
//   lantiq-exporter --once
//   lantiq-exporter --serve --addr 0.0.0.0 --port 9909 --interval 30
//
// Config precedence: flag > env > default.
//   --host           ONT_HOST            192.168.1.10
//   --user           ONT_USER            ONTUSER
//   --password-file  ONT_PASSWORD_FILE   (unset -> SSH key auth)
//                    ONT_PASS            (inline password, overrides the file)

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// One batched remote script; each block is labelled so parsing is order-free.
// PATH is extended because the lantiq tools are not in the login PATH.
const REMOTE_SCRIPT: &str = r#"
PATH=$PATH:/opt/lantiq/bin
printf '@ddm '
dd if=/dev/sfp_eeprom1 bs=1 skip=96 count=10 2>/dev/null | hexdump -C | head -1
printf '@gtcsg ';  onu gtcsg  2>/dev/null
printf '@gtcag ';  onu gtcag  2>/dev/null
printf '@gtcrg ';  onu gtcrg  2>/dev/null
printf '@gtctcg '; onu gtctcg 2>/dev/null
"#;

struct Config {
    host: String,
    user: String,
    password_file: Option<String>,
    once: bool,
    serve: bool,
    addr: String,
    port: u16,
    interval: u64,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_args() -> Config {
    let mut cfg = Config {
        host: env_or("ONT_HOST", "192.168.1.10"),
        user: env_or("ONT_USER", "ONTUSER"),
        password_file: std::env::var("ONT_PASSWORD_FILE").ok(),
        once: false,
        serve: false,
        addr: "0.0.0.0".to_string(),
        port: 9909,
        interval: 30,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let mut next = || {
            i += 1;
            args.get(i).cloned().unwrap_or_else(|| {
                eprintln!("missing value for {}", a);
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--host" => cfg.host = next(),
            "--user" => cfg.user = next(),
            "--password-file" => cfg.password_file = Some(next()),
            "--addr" => cfg.addr = next(),
            "--port" => cfg.port = next().parse().unwrap_or(9909),
            "--interval" => cfg.interval = next().parse().unwrap_or(30),
            "--once" => cfg.once = true,
            "--serve" => cfg.serve = true,
            "-h" | "--help" => {
                print!("{}", HELP);
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {}", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }
    cfg
}

const HELP: &str = "\
lantiq-exporter — Prometheus exporter for a Lantiq/Falcon GPON ONT

USAGE:
  lantiq-exporter --once
  lantiq-exporter --serve [--addr 0.0.0.0] [--port 9909] [--interval 30]

OPTIONS:
  --host <ip>            ONT address            (env ONT_HOST, default 192.168.1.10)
  --user <name>          SSH user               (env ONT_USER, default ONTUSER)
  --password-file <p>    SSH password file      (env ONT_PASSWORD_FILE; omit for key auth)
  --addr <ip>            HTTP bind address      (default 0.0.0.0)
  --port <n>             HTTP port              (default 9909)
  --interval <s>         poll interval seconds  (default 30)
  --once                 print metrics once and exit
  --serve                run the HTTP /metrics server
";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load_password(cfg: &Config) -> Option<String> {
    if let Ok(p) = std::env::var("ONT_PASS") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    if let Some(pf) = &cfg.password_file {
        match std::fs::read_to_string(pf) {
            Ok(s) => return Some(s.trim().to_string()),
            Err(e) => {
                eprintln!("cannot read password file {}: {}", pf, e);
            }
        }
    }
    None
}

// Run the remote script; return stdout on success.
fn ssh_run(cfg: &Config, password: &Option<String>) -> Result<String, String> {
    let target = format!("{}@{}", cfg.user, cfg.host);
    let ssh_opts = [
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "ConnectTimeout=10",
        "-o", "NumberOfPasswordPrompts=1",
    ];

    let mut cmd = if let Some(pw) = password {
        // Password auth via sshpass: -e reads the password from the SSHPASS
        // env var, so it never appears in the process argv.
        let mut c = Command::new("sshpass");
        c.arg("-e").arg("ssh");
        c.args(ssh_opts);
        c.arg("-o").arg("PreferredAuthentications=password,keyboard-interactive");
        c.arg("-o").arg("PubkeyAuthentication=no");
        c.arg(&target).arg(REMOTE_SCRIPT);
        c.env("SSHPASS", pw);
        c
    } else {
        // Key auth: no prompts.
        let mut c = Command::new("ssh");
        c.args(ssh_opts);
        c.arg("-o").arg("BatchMode=yes");
        c.arg(&target).arg(REMOTE_SCRIPT);
        c
    };

    let out = cmd.output().map_err(|e| format!("spawn ssh: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "ssh exit {}: {}",
            out.status.code().unwrap_or(-1),
            err.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// --- parsing -------------------------------------------------------------

#[derive(Default)]
struct Snapshot {
    up: i32,
    temp_c: Option<f64>,
    vcc: Option<f64>,
    tx_bias_a: Option<f64>,
    tx_dbm: Option<f64>,
    rx_dbm: Option<f64>,
    tx_uw: Option<f64>,
    rx_uw: Option<f64>,
    ds_state: Option<i64>,
    o5: Option<i32>,
    onu_id: Option<i64>,
    onu_resp: Option<i64>,
    gtc_ds_delay: Option<i64>,
    ranged_delay: Option<i64>,
    fec: Vec<(&'static str, i64)>,
    alarms: Vec<(String, i64)>,
    counters: Vec<(&'static str, i64)>,
    scrape_dur: Option<f64>,
    ts: Option<i64>,
}

// key=value pairs separated by whitespace (no quoted values in these getters).
fn kv(line: &str) -> Vec<(String, String)> {
    line.split_whitespace()
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn geti(pairs: &[(String, String)], key: &str) -> Option<i64> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}

// A `hexdump -C` row -> the 10 diagnostic bytes at A2h offset 96.
fn parse_ddm(line: &str) -> Option<[u8; 10]> {
    let body = line.split('|').next().unwrap_or("");
    let mut toks: Vec<&str> = body.split_whitespace().collect();
    if let Some(first) = toks.first() {
        if first.len() == 8 && first.chars().all(|c| c.is_ascii_hexdigit()) {
            toks.remove(0); // drop the offset column
        }
    }
    let bytes: Vec<u8> = toks
        .iter()
        .filter(|t| t.len() == 2 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .filter_map(|t| u8::from_str_radix(t, 16).ok())
        .take(10)
        .collect();
    if bytes.len() < 10 {
        return None;
    }
    let mut arr = [0u8; 10];
    arr.copy_from_slice(&bytes[..10]);
    Some(arr)
}

fn decode_ddm(b: &[u8; 10], s: &mut Snapshot) {
    let u16be = |i: usize| ((b[i] as u16) << 8) | b[i + 1] as u16;
    let s16be = |i: usize| {
        let v = u16be(i);
        if v & 0x8000 != 0 {
            v as i32 - 0x10000
        } else {
            v as i32
        }
    };
    s.temp_c = Some(s16be(0) as f64 / 256.0);
    s.vcc = Some(u16be(2) as f64 * 0.0001);
    s.tx_bias_a = Some(u16be(4) as f64 * 2e-6);
    let tx_uw = u16be(6) as f64 * 0.1;
    let rx_uw = u16be(8) as f64 * 0.1;
    s.tx_uw = Some(tx_uw);
    s.rx_uw = Some(rx_uw);
    if tx_uw > 0.0 {
        s.tx_dbm = Some(10.0 * (tx_uw / 1000.0).log10());
    }
    if rx_uw > 0.0 {
        s.rx_dbm = Some(10.0 * (rx_uw / 1000.0).log10());
    }
}

// exported counter names, in output order
const COUNTER_MAP: &[(&str, &str)] = &[
    ("tx_gem_frames_total", "tx_gem_frames_total"),
    ("tx_gem_bytes_total", "tx_gem_bytes_total"),
    ("tx_gem_idle_frames_total", "tx_gem_idle_frames_total"),
    ("rx_gem_frames_total", "rx_gem_frames_total"),
    ("rx_gem_bytes_total", "rx_gem_bytes_total"),
    ("rx_gem_frames_dropped", "rx_gem_frames_dropped_total"),
    ("rx_oversized_frames", "rx_oversized_frames_total"),
    ("hec_error_corr", "hec_errors_corrected_total"),
    ("hec_error_uncorr", "hec_errors_uncorrected_total"),
    ("bip", "bip_errors_total"),
    ("fec_words_corr", "fec_words_corrected_total"),
    ("fec_words_uncorr", "fec_words_uncorrected_total"),
    ("fec_words_total", "fec_words_total"),
    ("fec_seconds", "fec_seconds_total"),
    ("allocations_total", "allocations_total"),
    ("allocations_lost", "allocations_lost_total"),
    ("drop", "drop_total"),
    ("omci_drop", "omci_drop_total"),
];

fn parse(stdout: &str) -> Snapshot {
    let mut s = Snapshot {
        up: 1,
        ..Default::default()
    };
    for raw in stdout.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("@ddm") {
            if let Some(b) = parse_ddm(rest.trim()) {
                decode_ddm(&b, &mut s);
            }
        } else if let Some(rest) = line.strip_prefix("@gtcsg") {
            let p = kv(rest);
            if let Some(st) = geti(&p, "ds_state") {
                s.ds_state = Some(st);
                s.o5 = Some(if st == 3 { 1 } else { 0 });
            }
            s.onu_id = geti(&p, "onu_id");
            s.onu_resp = geti(&p, "onu_response_time");
            s.gtc_ds_delay = geti(&p, "gtc_ds_delay");
            if let Some(v) = geti(&p, "ds_fec_enable") {
                s.fec.push(("ds", v));
            }
            if let Some(v) = geti(&p, "us_fec_enable") {
                s.fec.push(("us", v));
            }
        } else if let Some(rest) = line.strip_prefix("@gtcrg") {
            let p = kv(rest);
            s.ranged_delay = geti(&p, "ranged_delay");
        } else if let Some(rest) = line.strip_prefix("@gtcag") {
            for (k, v) in kv(rest) {
                if k == "errorcode" {
                    continue;
                }
                if let Ok(n) = v.parse::<i64>() {
                    s.alarms.push((k, n));
                }
            }
        } else if let Some(rest) = line.strip_prefix("@gtctcg") {
            let p = kv(rest);
            for (src, dst) in COUNTER_MAP {
                if let Some(v) = geti(&p, src) {
                    s.counters.push((dst, v));
                }
            }
        }
    }
    s
}

// --- rendering -----------------------------------------------------------

fn fmt_f(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{:.6}", v);
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn render(s: &Snapshot) -> String {
    let mut o = String::new();
    macro_rules! g {
        ($name:expr, $help:expr, $val:expr) => {{
            o.push_str(&format!("# HELP {} {}\n", $name, $help));
            o.push_str(&format!("# TYPE {} gauge\n", $name));
            o.push_str(&format!("{} {}\n", $name, fmt_f($val)));
        }};
    }
    macro_rules! gi {
        ($name:expr, $help:expr, $val:expr) => {{
            o.push_str(&format!("# HELP {} {}\n", $name, $help));
            o.push_str(&format!("# TYPE {} gauge\n", $name));
            o.push_str(&format!("{} {}\n", $name, $val));
        }};
    }

    gi!("ont_up", "1 if the last poll of the ONT succeeded, else 0", s.up);

    if let Some(v) = s.temp_c {
        g!("ont_temperature_celsius", "Module/BOSA temperature (SFF-8472 A2h)", v);
    }
    if let Some(v) = s.vcc {
        g!("ont_voltage_volts", "Module supply voltage Vcc (SFF-8472 A2h)", v);
    }
    if let Some(v) = s.tx_bias_a {
        g!("ont_tx_bias_amperes", "Laser TX bias current", v);
    }
    if let Some(v) = s.tx_dbm {
        g!("ont_tx_power_dbm", "Transmit optical power", v);
    }
    if let Some(v) = s.rx_dbm {
        g!("ont_rx_power_dbm", "Receive optical power", v);
    }
    if let Some(v) = s.tx_uw {
        g!("ont_tx_power_microwatts", "Transmit optical power (linear)", v);
    }
    if let Some(v) = s.rx_uw {
        g!("ont_rx_power_microwatts", "Receive optical power (linear)", v);
    }
    if let Some(v) = s.ds_state {
        gi!("ont_gpon_ds_state", "GTC downstream state (3 = O5 operational)", v);
    }
    if let Some(v) = s.o5 {
        gi!("ont_gpon_o5", "1 if the ONT is in O5 (operational), else 0", v);
    }
    if let Some(v) = s.onu_id {
        gi!("ont_onu_id", "ONU-ID assigned by the OLT (255 = unassigned)", v);
    }
    if let Some(v) = s.onu_resp {
        gi!("ont_onu_response_time", "ONU response time (equalization)", v);
    }
    if let Some(v) = s.gtc_ds_delay {
        gi!("ont_gtc_ds_delay", "GTC downstream delay", v);
    }
    if let Some(v) = s.ranged_delay {
        gi!("ont_ranged_delay", "Ranged (equalization) delay set by the OLT", v);
    }

    if !s.fec.is_empty() {
        o.push_str("# HELP ont_fec_enabled FEC enabled (1) per direction\n");
        o.push_str("# TYPE ont_fec_enabled gauge\n");
        for (dir, v) in &s.fec {
            o.push_str(&format!("ont_fec_enabled{{direction=\"{}\"}} {}\n", dir, v));
        }
    }

    if !s.alarms.is_empty() {
        o.push_str("# HELP ont_alarm GTC alarm bit state (1 = active)\n");
        o.push_str("# TYPE ont_alarm gauge\n");
        for (name, v) in &s.alarms {
            o.push_str(&format!("ont_alarm{{name=\"{}\"}} {}\n", name, v));
        }
    }

    for (name, v) in &s.counters {
        let full = format!("ont_{}", name);
        o.push_str(&format!("# TYPE {} counter\n", full));
        o.push_str(&format!("{} {}\n", full, v));
    }

    if let Some(v) = s.scrape_dur {
        g!("ont_scrape_duration_seconds", "Duration of the last successful SSH poll", v);
    }
    if let Some(v) = s.ts {
        gi!("ont_last_scrape_timestamp_seconds", "Unix time of the last successful poll", v);
    }

    o
}

fn poll_once(cfg: &Config, password: &Option<String>) -> (Snapshot, Option<String>) {
    let t0 = Instant::now();
    match ssh_run(cfg, password) {
        Ok(stdout) => {
            let mut s = parse(&stdout);
            s.scrape_dur = Some(t0.elapsed().as_secs_f64());
            s.ts = Some(now_secs());
            (s, None)
        }
        Err(e) => (
            Snapshot {
                up: 0,
                ..Default::default()
            },
            Some(e),
        ),
    }
}

// --- HTTP server (minimal, std only) -------------------------------------

fn handle_conn(mut stream: TcpStream, body: &str) {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf); // read (and ignore) the request headers
    let req = String::from_utf8_lossy(&buf);
    let path = req
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");
    let resp = if path == "/metrics" || path == "/" {
        format!(
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else {
        "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    };
    let _ = stream.write_all(resp.as_bytes());
}

fn serve(cfg: Config, password: Option<String>) -> i32 {
    let cache = Arc::new(Mutex::new(String::from("ont_up 0\n")));
    let interval = cfg.interval.max(1);
    let bind = format!("{}:{}", cfg.addr, cfg.port);

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {}: {}", bind, e);
            return 1;
        }
    };

    // background poller (owns cfg + password)
    {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || loop {
            let (snap, err) = poll_once(&cfg, &password);
            if let Some(e) = err {
                eprintln!("[poll] error: {}", e);
            }
            let text = render(&snap);
            if let Ok(mut c) = cache.lock() {
                *c = text;
            }
            std::thread::sleep(Duration::from_secs(interval));
        });
    }

    eprintln!("serving ONT metrics on http://{}/metrics (poll {}s)", bind, interval);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let body = cache.lock().map(|c| c.clone()).unwrap_or_default();
                std::thread::spawn(move || handle_conn(s, &body));
            }
            Err(e) => eprintln!("accept: {}", e),
        }
    }
    0
}

fn main() {
    let cfg = parse_args();
    let password = load_password(&cfg);

    if cfg.serve && !cfg.once {
        std::process::exit(serve(cfg, password));
    }

    // default / --once: poll and print
    let (snap, err) = poll_once(&cfg, &password);
    if let Some(e) = err {
        eprintln!("[poll] error: {}", e);
    }
    print!("{}", render(&snap));
    std::process::exit(if snap.up == 1 { 0 } else { 1 });
}
