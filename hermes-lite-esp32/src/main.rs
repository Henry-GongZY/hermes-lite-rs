//! Hermes-Lite on the AI-Thinker ESP32-S (plain ESP32, 4MB flash) — Rust
//! twin of `examples/esp32/main/main.c`.
//!
//! Serial commands:
//!   wifi <ssid> <password>   provision + connect (persisted to SPIFFS)
//!   skills                   list skill files
//!   sessions                 list session history files
//!   session <name>           switch to another session (rebuilds the agent)
//!   reset                    wipe the current session history and restart
//!   free                     show heap usage
//!   anything else            goes to the agent

extern crate alloc;

use std::io::Write;
use std::sync::Arc;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

use hermes_lite_core::{Agent, Config, Platform, Tool};
use platform::Esp32Platform;

mod platform;

const SPIFFS_BASE: &str = "/spiffs";
const SESSIONS_DIR: &str = "/spiffs/sessions";
const DEFAULT_SESSION: &str = "default";
const SKILLS_DIR: &str = "/spiffs/skills";

/// Per-name JSONL history file under the sessions dir.
fn session_path(name: &str) -> String {
    format!("{SESSIONS_DIR}/{name}.jsonl")
}

/// Session names must be plain single path components (no separators,
/// no traversal, not hidden).
fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name != ".."
}

fn mount_spiffs() {
    let conf = sys::esp_vfs_spiffs_conf_t {
        base_path: b"/spiffs\0".as_ptr() as *const _,
        partition_label: b"storage\0".as_ptr() as *const _,
        max_files: 4,
        format_if_mount_failed: true,
    };
    let r = unsafe { sys::esp_vfs_spiffs_register(&conf) };
    println!("[app] spiffs: {}", if r == sys::ESP_OK { "mounted" } else { "failed" });
}

/* ---------------- wifi (SPIFFS-backed credentials) ---------------- */

fn wifi_save(dir: &Esp32Platform, ssid: &str, pass: &str) {
    let body = format!("{ssid}\n{pass}\n");
    let _ = dir.fs_write("/spiffs/wifi.txt", body.as_bytes());
}

fn wifi_load(dir: &Esp32Platform) -> Option<(String, String)> {
    let raw = String::from_utf8_lossy(&dir.fs_read("/spiffs/wifi.txt", 256).ok()?).into_owned();
    let mut it = raw.lines();
    let ssid = it.next()?.to_string();
    let pass = it.next().unwrap_or("").to_string();
    if ssid.is_empty() {
        return None;
    }
    Some((ssid, pass))
}

fn wifi_connect_blocking(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    ssid: &str,
    pass: &str,
) -> anyhow::Result<()> {
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        ..Default::default()
    }))?;
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    Ok(())
}

/* runtime-configurable LLM settings (SPIFFS config file) */
const CONFIG_PATH: &str = "/spiffs/config.txt";

fn cfg_save(plat: &Esp32Platform, field: &str, value: &str) {
    // keep one line per field; values never contain newlines
    let mut lines: Vec<String> = plat
        .fs_read(CONFIG_PATH, 4096)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(&format!("{field}=")))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("{field}={value}"));
    let body = lines.join("\n") + "\n";
    let _ = plat.fs_write(CONFIG_PATH, body.as_bytes());
}

fn cfg_load(plat: &Esp32Platform, field: &str) -> Option<String> {
    let raw = String::from_utf8_lossy(&plat.fs_read(CONFIG_PATH, 4096).ok()?).into_owned();
    raw.lines().find_map(|l| {
        l.strip_prefix(&format!("{field}="))
            .map(|v| v.to_string())
    })
}

fn mask_secret(s: &str) -> String {
    if s.len() <= 10 {
        "***".into()
    } else {
        format!("{}***", &s[..10])
    }
}

/* ---------------- tools (raw sys calls, parity with the C port) ---- */

fn parse_args(args: &str) -> serde_json::Value {
    serde_json::from_str(if args.is_empty() { "{}" } else { args })
        .unwrap_or(serde_json::Value::Null)
}

fn jnum<'a>(v: &'a serde_json::Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn tool_gpio_write() -> Tool {
    Tool {
        name: "gpio_write",
        description: "Set an output GPIO level. Args: pin (0-39, not 6/11-17), level (0/1).",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\"},\
                          \"level\":{\"type\":\"integer\"}},\"required\":[\"pin\",\"level\"]}",
        f: Box::new(|args, out| {
            let v = parse_args(args);
            let (Some(pin), Some(level)) = (jnum(&v, "pin"), jnum(&v, "level")) else {
                out.push_str("error: need pin (int) and level (0/1)");
                return;
            };
            let p = pin as i32;
            if !(0..=39).contains(&p) || p == 6 || (11..=17).contains(&p) {
                out.push_str(&format!("error: pin {p} not usable (flash/PSRAM pins)"));
                return;
            }
            unsafe {
                sys::gpio_reset_pin(p);
                sys::gpio_set_direction(p, sys::gpio_mode_t_GPIO_MODE_OUTPUT);
                sys::gpio_set_level(p, (level != 0.0) as u32);
            }
            out.push_str(&format!("ok: GPIO{p} = {}", (level != 0.0) as i32));
        }),
    }
}

fn tool_gpio_read() -> Tool {
    Tool {
        name: "gpio_read",
        description: "Read a GPIO as input (pull-up enabled). Args: pin.",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\"}},\
                          \"required\":[\"pin\"]}",
        f: Box::new(|args, out| {
            let v = parse_args(args);
            let Some(pin) = jnum(&v, "pin") else {
                out.push_str("error: need pin (int)");
                return;
            };
            let p = pin as i32;
            if !(0..=39).contains(&p) || p == 6 || (11..=17).contains(&p) {
                out.push_str(&format!("error: pin {p} not usable"));
                return;
            }
            let cfg = sys::gpio_config_t {
                pin_bit_mask: 1u64 << p,
                mode: sys::gpio_mode_t_GPIO_MODE_INPUT,
                pull_up_en: sys::gpio_pullup_t_GPIO_PULLUP_ENABLE,
                pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
                intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
            };
            let lvl = unsafe {
                sys::gpio_config(&cfg);
                sys::gpio_get_level(p)
            };
            out.push_str(&format!("GPIO{p} reads {lvl}"));
        }),
    }
}

fn tool_adc_read() -> Tool {
    Tool {
        name: "adc_read",
        description: "Read ADC1 channel 0-7 (GPIO32-39), 12-bit with 11dB atten.",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"channel\":{\"type\":\"integer\"}},\
                          \"required\":[\"channel\"]}",
        f: Box::new(|args, out| {
            let v = parse_args(args);
            let Some(ch) = jnum(&v, "channel").map(|c| c as i32) else {
                out.push_str("error: need channel 0-7 (ADC1, GPIO32-39)");
                return;
            };
            if !(0..=7).contains(&ch) {
                out.push_str("error: need channel 0-7 (ADC1, GPIO32-39)");
                return;
            }
            unsafe {
                sys::adc1_config_width(sys::adc_bits_width_t_ADC_WIDTH_BIT_12);
                let chan = (sys::adc1_channel_t_ADC1_CHANNEL_0 as i32 + ch) as sys::adc1_channel_t;
                sys::adc1_config_channel_atten(chan, sys::adc_atten_t_ADC_ATTEN_DB_11);
                let raw = sys::adc1_get_raw(chan);
                let mv = raw * 2450 / 4095;
                out.push_str(&format!("ADC1 ch{ch} raw={raw} (~{mv} mV)"));
            }
        }),
    }
}

fn tool_pwm_write() -> Tool {
    Tool {
        name: "pwm_write",
        description: "Write a PWM duty cycle to a pin via LEDC timer0/channel0 \
                      (5 kHz, 10-bit). Args: pin (0-39, not 6/11-17), duty (0-1023).",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\"},\
                          \"duty\":{\"type\":\"integer\"}},\"required\":[\"pin\",\"duty\"]}",
        f: Box::new(|args, out| {
            let v = parse_args(args);
            let (Some(pin), Some(duty)) = (jnum(&v, "pin"), jnum(&v, "duty")) else {
                out.push_str("error: need pin (int) and duty (0-1023)");
                return;
            };
            let p = pin as i32;
            if !(0..=39).contains(&p) || p == 6 || (11..=17).contains(&p) {
                out.push_str(&format!("error: pin {p} not usable (flash/PSRAM pins)"));
                return;
            }
            let d = duty as i32;
            if !(0..=1023).contains(&d) {
                out.push_str("error: duty must be 0-1023");
                return;
            }
            unsafe {
                let timer = sys::ledc_timer_config_t {
                    speed_mode: sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
                    duty_resolution: sys::ledc_timer_bit_t_LEDC_TIMER_10_BIT,
                    timer_num: sys::ledc_timer_t_LEDC_TIMER_0,
                    freq_hz: 5000,
                    clk_cfg: sys::soc_periph_ledc_clk_src_legacy_t_LEDC_AUTO_CLK,
                    deconfigure: false,
                };
                sys::ledc_timer_config(&timer);
                let ch = sys::ledc_channel_config_t {
                    gpio_num: p,
                    speed_mode: sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
                    channel: sys::ledc_channel_t_LEDC_CHANNEL_0,
                    intr_type: sys::ledc_intr_type_t_LEDC_INTR_DISABLE,
                    timer_sel: sys::ledc_timer_t_LEDC_TIMER_0,
                    duty: d as u32,
                    hpoint: 0,
                    flags: Default::default(),
                };
                sys::ledc_channel_config(&ch);
            }
            out.push_str(&format!("ok: GPIO{p} PWM duty {d}/1023 (5 kHz, LEDC ch0)"));
        }),
    }
}

fn tool_i2c_scan() -> Tool {
    Tool {
        name: "i2c_scan",
        description: "Scan the I2C bus (SDA=21, SCL=22) and list found device addresses.",
        parameters_json: "{\"type\":\"object\",\"properties\":{}}",
        f: Box::new(|_args, out| {
            // lazy one-time bus init on the common devkit pins (SDA=21, SCL=22);
            // the init result is remembered so failures are reported on every call
            static I2C_INIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let ready = *I2C_INIT.get_or_init(|| unsafe {
                let conf = sys::i2c_config_t {
                    mode: sys::i2c_mode_t_I2C_MODE_MASTER,
                    sda_io_num: 21,
                    scl_io_num: 22,
                    sda_pullup_en: true,
                    scl_pullup_en: true,
                    __bindgen_anon_1: sys::i2c_config_t__bindgen_ty_1 {
                        master: sys::i2c_config_t__bindgen_ty_1__bindgen_ty_1 {
                            clk_speed: 100_000,
                        },
                    },
                    clk_flags: 0,
                };
                sys::i2c_param_config(sys::i2c_port_t_I2C_NUM_0, &conf) == sys::ESP_OK
                    && sys::i2c_driver_install(
                        sys::i2c_port_t_I2C_NUM_0,
                        sys::i2c_mode_t_I2C_MODE_MASTER,
                        0,
                        0,
                        0,
                    ) == sys::ESP_OK
            });
            if !ready {
                out.push_str("error: i2c init failed (SDA=21, SCL=22)");
                return;
            }
            let mut found: Vec<String> = Vec::new();
            for addr in 0x03u8..=0x77 {
                // 1-byte probe: an ACK of the address means a device responded
                let dummy = 0u8;
                let ok = unsafe {
                    sys::i2c_master_write_to_device(
                        sys::i2c_port_t_I2C_NUM_0,
                        addr,
                        &dummy,
                        1,
                        100, // pdMS_TO_TICKS(100) at the default 1000 Hz tick
                    ) == sys::ESP_OK
                };
                if ok {
                    found.push(format!("0x{addr:02X}"));
                }
            }
            if found.is_empty() {
                out.push_str("no I2C devices found (SDA=21, SCL=22)");
            } else {
                out.push_str(&format!(
                    "found device(s) on SDA=21/SCL=22: {}",
                    found.join(", ")
                ));
            }
        }),
    }
}

/// file_read/file_write paths must stay inside SPIFFS.
fn check_spiffs_path(path: &str) -> Result<(), String> {
    if !path.starts_with(&format!("{SPIFFS_BASE}/")) {
        return Err("error: path must be under /spiffs/".into());
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err("error: path must not contain '..'".into());
    }
    Ok(())
}

const FILE_TOOL_CAP: usize = 2048;

fn tool_file_read(plat: std::sync::Arc<Esp32Platform>) -> Tool {
    Tool {
        name: "file_read",
        description: "Read a text file from SPIFFS (max 2 KB). Args: path (under /spiffs/).",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"path\":\
                          {\"type\":\"string\"}},\"required\":[\"path\"]}",
        f: Box::new(move |args, out| {
            let v = parse_args(args);
            let Some(path) = v.get("path").and_then(|x| x.as_str()) else {
                out.push_str("error: need path (string)");
                return;
            };
            if let Err(e) = check_spiffs_path(path) {
                out.push_str(&e);
                return;
            }
            match plat.fs_read(path, FILE_TOOL_CAP) {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        out.push_str(&format!("{path}: (empty)"));
                    } else {
                        out.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                Err(_) => out.push_str(&format!("error: cannot read {path}")),
            }
        }),
    }
}

fn tool_file_write(plat: std::sync::Arc<Esp32Platform>) -> Tool {
    Tool {
        name: "file_write",
        description: "Write text to a file on SPIFFS (max 2 KB, overwrites). \
                      Args: path (under /spiffs/), content.",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"path\":\
                          {\"type\":\"string\"},\"content\":{\"type\":\"string\"}},\
                          \"required\":[\"path\",\"content\"]}",
        f: Box::new(move |args, out| {
            let v = parse_args(args);
            let (Some(path), Some(content)) = (
                v.get("path").and_then(|x| x.as_str()),
                v.get("content").and_then(|x| x.as_str()),
            ) else {
                out.push_str("error: need path (string) and content (string, max 2048)");
                return;
            };
            if let Err(e) = check_spiffs_path(path) {
                out.push_str(&e);
                return;
            }
            if content.len() > FILE_TOOL_CAP {
                out.push_str("error: content too large (max 2048 bytes)");
                return;
            }
            match plat.fs_write(path, content.as_bytes()) {
                Ok(()) => out.push_str(&format!("ok: wrote {} bytes to {path}", content.len())),
                Err(_) => out.push_str(&format!("error: cannot write {path}")),
            }
        }),
    }
}

fn tool_sys_info() -> Tool {
    Tool {
        name: "sys_info",
        description: "Device status: free heap, minimum heap, uptime, IDF version, wifi RSSI.",
        parameters_json: "{\"type\":\"object\",\"properties\":{}}",
        f: Box::new(|_args, out| {
            let free = unsafe { sys::esp_get_free_heap_size() };
            let min = unsafe { sys::esp_get_minimum_free_heap_size() };
            let up = unsafe { sys::esp_timer_get_time() } / 1_000_000;
            out.push_str(&format!(
                "free_heap={free} min_free={min} uptime={up}s idf={}",
                unsafe {
                    core::ffi::CStr::from_ptr(sys::esp_get_idf_version()).to_string_lossy()
                }
            ));
        }),
    }
}

/// OTA skill sync: GET <base>/index.json -> {"skills":[{"name","file"}]}
/// then GET <base>/<file> for each entry, saved as <skills_dir>/<name>.md
fn do_sync(plat: &Esp32Platform, base: &str, skills_dir: &str) {
    use hermes_lite_core::Platform;
    let mut idx: Vec<u8> = Vec::new();
    let join = |base: &str, tail: &str| {
        format!(
            "{}{}{}",
            base,
            if base.ends_with('/') { "" } else { "/" },
            tail
        )
    };
    let idx_url = join(base, "index.json");
    let st = plat.http_get(&idx_url, "", &mut idx, 4096);
    if !matches!(st, Ok(200)) {
        println!("[app] sync: index fetch failed ({st:?})");
        return;
    }
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&idx))
    else {
        println!("[app] sync: bad index.json");
        return;
    };
    let Some(entries) = root.get("skills").and_then(|s| s.as_array()) else {
        println!("[app] sync: bad index.json");
        return;
    };
    let mut ok = 0;
    for e in entries {
        let Some(name) = e.get("name").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(file) = e.get("file").and_then(|x| x.as_str()) else {
            continue;
        };
        if name.is_empty() || name.starts_with('.') || name.contains('/') {
            continue;
        }
        let mut body: Vec<u8> = Vec::new();
        let st = plat.http_get(&join(base, file), "", &mut body, 4096);
        if !matches!(st, Ok(200)) || body.is_empty() {
            println!("[app] sync: {name} failed ({st:?})");
            continue;
        }
        let path = format!("{skills_dir}/{name}.md");
        if plat.fs_write(&path, &body).is_ok() {
            println!("[app] synced skill '{name}' ({} bytes)", body.len());
            ok += 1;
        }
    }
    println!("[app] sync done: {ok}/{} skills", entries.len());
}

fn skills_file_cb(name: &str) -> bool {
    if !name.starts_with('.') {
        println!("  {name}");
    }
    false
}

fn on_fragment(frag: &str) {
    print!("{frag}");
    let _ = std::io::stdout().flush();
}

fn main() -> anyhow::Result<()> {
    println!("[app] rust main entered");
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    println!("=== Hermes-Lite ESP32 / Rust (AI-Thinker ESP32-S) ===");
    mount_spiffs();

    let plat = Arc::new(Esp32Platform::new());
    // per-session JSONL histories live here; make sure the dir exists
    let _ = std::fs::create_dir_all(SESSIONS_DIR);
    // CA for https:// goes here: plat.set_ca(include_bytes!("ca.pem"));

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let wifi = Arc::new(std::sync::Mutex::new(BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop.clone(),
    )?));

    if let Some((ssid, pass)) = wifi_load(&plat) {
        println!("[app] connecting to '{ssid}'...");
        let ok = wifi
            .lock()
            .map(|mut w| wifi_connect_blocking(&mut w, &ssid, &pass).is_ok())
            .unwrap_or(false);
        println!("[app] {}", if ok { "wifi connected" } else { "wifi connect failed; use 'wifi <ssid> <pass>'" });
    } else {
        println!("[app] no wifi credentials; send 'wifi <ssid> <pass>' over serial");
    }

    // background watchdog: poll every 5s; on disconnect, retry with
    // exponential backoff (2s, 4s, ... capped at 30s), reset on success
    {
        let wifi = wifi.clone();
        let plat = plat.clone();
        std::thread::Builder::new()
            .name("wifi_watchdog".into())
            .stack_size(8192)
            .spawn(move || {
                let mut fails: u32 = 0;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    let Ok(mut w) = wifi.lock() else { continue };
                    let connected = matches!(w.is_connected(), Ok(true));
                    if connected {
                        fails = 0;
                        continue;
                    }
                    let Some((ssid, pass)) = wifi_load(&plat) else { continue };
                    fails = fails.saturating_add(1);
                    let wait = std::cmp::min(30, 2u64 << fails.min(4).saturating_sub(1));
                    drop(w); // let the REPL use the radio while we back off
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                    let Ok(mut w) = wifi.lock() else { continue };
                    if matches!(w.is_connected(), Ok(true)) {
                        continue;
                    }
                    let _ = wifi_connect_blocking(&mut w, &ssid, &pass);
                }
            })
            .expect("spawn wifi watchdog");
    }

    // session storage: one JSONL history file per session name, owned here so
    // it stays alive as long as the REPL runs
    let mut history_path = session_path(DEFAULT_SESSION);

    let cfg = Config {
        api_url: env_or("HL_API_URL", "https://www.sui-xiang.net/v1/chat/completions"),
        api_key: env_or("HL_API_KEY", ""),
        model: env_or("HL_MODEL", "gpt-6.1-terra"),
        system_prompt: Some(
            "You are Hermes Lite running on an ESP32 microcontroller. \
             Answer briefly in the user's language. Use tools for hardware state."
                .into(),
        ),
        skills_dir: Some(SKILLS_DIR.into()),
        history_path: Some(history_path.clone()),
        max_ctx_chars: 8000,
        max_response_bytes: 24576,
        buf_hist: 8192,
        ..Config::default()
    };
    let mut cfg = cfg;
    // runtime overrides (serial-provisioned, persisted in SPIFFS)
    if let Some(k) = cfg_load(&plat, "api_key") {
        if !k.is_empty() {
            cfg.api_key = k;
        }
    }
    if let Some(u) = cfg_load(&plat, "api_url") {
        if !u.is_empty() {
            cfg.api_url = u;
        }
    }
    if let Some(m) = cfg_load(&plat, "model") {
        if !m.is_empty() {
            cfg.model = m;
        }
    }

    let tools = || {
        let plat = Arc::new(Esp32Platform::new());
        vec![
            tool_gpio_write(),
            tool_gpio_read(),
            tool_adc_read(),
            tool_sys_info(),
            tool_pwm_write(),
            tool_i2c_scan(),
            tool_file_read(plat.clone()),
            tool_file_write(plat),
        ]
    };
    let mut agent = Agent::new(cfg.clone(), plat.clone(), tools());

    println!(
        "commands: 'wifi <ssid> <pass>' | 'key <api-key>' | 'url <endpoint>' | \
         'model <name>' | 'reconnect' | 'skills' | 'sessions' | 'session <name>' | \
         'reset' | 'free'"
    );

    let mut line = String::new();
    loop {
        print!("\nhermes> ");
        let _ = std::io::stdout().flush();
        line.clear();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        let msg = line.trim();
        if msg.is_empty() {
            continue;
        }

        if let Some(rest) = msg.strip_prefix("wifi ") {
            let Some((ssid, pass)) = rest.split_once(' ') else {
                println!("usage: wifi <ssid> <pass>");
                continue;
            };
            wifi_save(&plat, ssid, pass);
            println!("[app] connecting to '{ssid}'...");
            let ok = wifi
                .lock()
                .map(|mut w| wifi_connect_blocking(&mut w, ssid, pass).is_ok())
                .unwrap_or(false);
            println!(
                "[app] {}",
                if ok {
                    "wifi connected"
                } else {
                    "connect failed"
                }
            );
        } else if let Some(rest) = msg.strip_prefix("key ") {
            let key = rest.trim_start();
            cfg_save(&plat, "api_key", key);
            cfg.api_key = key.to_string();
            println!("[app] api key set ({}); 'reset' to apply", mask_secret(key));
        } else if let Some(rest) = msg.strip_prefix("url ") {
            let u = rest.trim_start();
            cfg_save(&plat, "api_url", u);
            cfg.api_url = u.to_string();
            println!("[app] endpoint set: {u}; 'reset' to apply");
        } else if let Some(rest) = msg.strip_prefix("model ") {
            let m = rest.trim_start();
            cfg_save(&plat, "model", m);
            cfg.model = m.to_string();
            println!("[app] model set: {m}; 'reset' to apply");
        } else if msg == "reconnect" {
            match wifi_load(&plat) {
                Some((ssid, pass)) => {
                    println!("[app] reconnecting to '{ssid}'...");
                    let ok = wifi
                        .lock()
                        .map(|mut w| wifi_connect_blocking(&mut w, &ssid, &pass).is_ok())
                        .unwrap_or(false);
                    println!(
                        "[app] {}",
                        if ok {
                            "wifi connected"
                        } else {
                            "connect failed"
                        }
                    );
                }
                None => println!("[app] no saved credentials; use 'wifi <ssid> <pass>'"),
            }
        } else if let Some(rest) = msg.strip_prefix("sync") {
            let url = rest.trim_start();
            let url = if url.is_empty() {
                match cfg_load(&plat, "sync_url") {
                    Some(u) if !u.is_empty() => u,
                    _ => {
                        println!("[app] no sync url saved; use 'sync <base-url>'");
                        continue;
                    }
                }
            } else {
                cfg_save(&plat, "sync_url", url);
                url.to_string()
            };
            do_sync(&plat, &url, SKILLS_DIR);
        } else if msg == "skills" {
            println!("skills in {SKILLS_DIR}:");
            for name in platform_fs_list(&plat) {
                skills_file_cb(&name);
            }
        } else if msg == "sessions" {
            println!("sessions in {SESSIONS_DIR}:");
            match plat.fs_list(SESSIONS_DIR) {
                Ok(names) if !names.is_empty() => {
                    for name in names {
                        if !name.starts_with('.') {
                            println!("  {name}");
                        }
                    }
                }
                _ => println!("  (none yet)"),
            }
        } else if msg == "session" {
            println!("usage: session <name>");
        } else if let Some(rest) = msg.strip_prefix("session ") {
            let name = rest.trim();
            if !valid_session_name(name) {
                println!("usage: session <name> (plain name, no '/', '..' or leading '.')");
            } else {
                history_path = session_path(name);
                cfg.history_path = Some(history_path.clone());
                agent = Agent::new(cfg.clone(), plat.clone(), tools());
                println!("[app] session '{name}' active ({history_path})");
            }
        } else if msg == "reset" {
            // wipe the CURRENT session's history file, then rebuild the agent
            let _ = plat.fs_write(&history_path, b"");
            agent = Agent::new(cfg.clone(), plat.clone(), tools());
            println!("[app] agent restarted, history wiped ({history_path})");
        } else if msg == "free" {
            println!(
                "free_heap={} min_free={}",
                unsafe { sys::esp_get_free_heap_size() },
                unsafe { sys::esp_get_minimum_free_heap_size() }
            );
        } else {
            let t0 = plat.time_ms();
            let mut reply = String::new();
            let r = agent.chat_stream(msg, &mut on_fragment, &mut reply);
            let dt = plat.time_ms() - t0;
            if r.is_ok() {
                println!("\n({dt} ms)");
            } else {
                println!(
                    "\nerror: {}",
                    if reply.is_empty() { "see log above" } else { &reply }
                );
            }
        }
    }
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn platform_fs_list(plat: &Esp32Platform) -> Vec<String> {
    use hermes_lite_core::Platform;
    plat.fs_list(SKILLS_DIR).unwrap_or_default()
}
