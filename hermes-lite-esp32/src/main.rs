//! Hermes-Lite on the AI-Thinker ESP32-S (plain ESP32, 4MB flash) — Rust
//! twin of `examples/esp32/main/main.c`.
//!
//! Serial commands:
//!   wifi <ssid> <password>   provision + connect (persisted to SPIFFS)
//!   skills                   list skill files
//!   reset                    wipe conversation history and restart the agent
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
const HISTORY_PATH: &str = "/spiffs/history.jsonl";
const SKILLS_DIR: &str = "/spiffs/skills";

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
    // CA for https:// goes here: plat.set_ca(include_bytes!("ca.pem"));

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
    )?;

    if let Some((ssid, pass)) = wifi_load(&plat) {
        println!("[app] connecting to '{ssid}'...");
        match wifi_connect_blocking(&mut wifi, &ssid, &pass) {
            Ok(()) => println!("[app] wifi connected"),
            Err(_) => println!("[app] wifi connect failed; use 'wifi <ssid> <pass>'"),
        }
    } else {
        println!("[app] no wifi credentials; send 'wifi <ssid> <pass>' over serial");
    }

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
        history_path: Some(HISTORY_PATH.into()),
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
        vec![
            tool_gpio_write(),
            tool_gpio_read(),
            tool_adc_read(),
            tool_sys_info(),
        ]
    };
    let mut agent = Agent::new(cfg.clone(), plat.clone(), tools());

    println!(
        "commands: 'wifi <ssid> <pass>' | 'key <api-key>' | 'url <endpoint>' | \
         'model <name>' | 'reconnect' | 'skills' | 'reset' | 'free'"
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
            println!(
                "[app] {}",
                if wifi_connect_blocking(&mut wifi, ssid, pass).is_ok() {
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
                    println!(
                        "[app] {}",
                        if wifi_connect_blocking(&mut wifi, &ssid, &pass).is_ok() {
                            "wifi connected"
                        } else {
                            "connect failed"
                        }
                    );
                }
                None => println!("[app] no saved credentials; use 'wifi <ssid> <pass>'"),
            }
        } else if msg == "skills" {
            println!("skills in {SKILLS_DIR}:");
            for name in platform_fs_list(&plat) {
                skills_file_cb(&name);
            }
        } else if msg == "reset" {
            let _ = plat.fs_write(HISTORY_PATH, b"");
            agent = Agent::new(cfg.clone(), plat.clone(), tools());
            println!("[app] agent restarted, history wiped");
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
