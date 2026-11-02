//! Terminal demo — the Rust twin of `examples/linux/main.c`:
//! chat with exec/time tools, skills dir, streaming output, named sessions.

use std::io::Write;
use std::process::Command;

use hermes_lite_core::{Agent, Config, Tool};
use hermes_lite_std::platform;

const SESSIONS_DIR: &str = "sessions";

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

fn tool_exec() -> Tool {
    Tool {
        name: "exec",
        description: "Run a shell command and return its combined stdout. \
                      Working directory is the process cwd.",
        parameters_json: "{\"type\":\"object\",\"properties\":{\"command\":\
                          {\"type\":\"string\",\"description\":\"the shell command to run\"}},\
                          \"required\":[\"command\"]}",
        f: Box::new(|args, out| {
            let v: serde_json::Value =
                serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
            let Some(cmd) = v.get("command").and_then(|x| x.as_str()) else {
                out.push_str("error: missing 'command'");
                return;
            };
            if cmd.is_empty() {
                out.push_str("error: missing 'command'");
                return;
            }
            match Command::new("sh").arg("-c").arg(cmd).output() {
                Ok(o) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    if !o.stderr.is_empty() {
                        text.push_str(&String::from_utf8_lossy(&o.stderr));
                    }
                    if text.trim().is_empty() {
                        out.push_str(&format!("(no output, exit={})", o.status));
                    } else {
                        out.push_str(text.trim_end());
                        if out.len() > 4096 {
                            out.truncate(4096);
                            out.push_str("\n... (truncated)");
                        }
                    }
                }
                Err(e) => out.push_str(&format!("error: popen failed: {e}")),
            }
        }),
    }
}

/// days-from-civil inverse (Howard Hinnant's algorithm); UTC date for a
/// unix timestamp, no chrono dependency.
fn civil_from_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

fn tool_time_now() -> Tool {
    Tool {
        name: "time_now",
        description: "Returns the current UTC date and time.",
        parameters_json: "{\"type\":\"object\",\"properties\":{}}",
        f: Box::new(|_args, out| {
            let now = std::time::SystemTime::now();
            match now.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => out.push_str(&civil_from_unix(d.as_secs())),
                Err(_) => out.push_str("error: clock before epoch"),
            }
        }),
    }
}

fn on_fragment(frag: &str) {
    print!("{frag}");
    let _ = std::io::stdout().flush();
}

fn main() {
    let url = std::env::var("HL_API_URL")
        .unwrap_or_else(|_| "https://www.sui-xiang.net/v1/chat/completions".into());
    let key = std::env::var("HL_API_KEY").unwrap_or_default();
    let model = std::env::var("HL_MODEL").unwrap_or_else(|_| "gpt-6.1-terra".into());
    let skills = std::env::var("HL_SKILLS_DIR").unwrap_or_else(|_| "skills".into());
    let session = std::env::var("HL_SESSION").unwrap_or_else(|_| "default".into());

    if key.is_empty() {
        eprintln!("usage: HL_API_KEY=sk-... hermes-lite");
        std::process::exit(1);
    }

    // session storage: one JSONL history file per session name under ./sessions/
    let _ = std::fs::create_dir_all(SESSIONS_DIR);
    let mut history_path = session_path(&session);

    let mut cfg = Config {
        api_url: url,
        api_key: key,
        model,
        system_prompt: Some(
            "You are Hermes Lite, a concise assistant running on the user's machine. \
             When a task needs a shell command or current time, use your tools \
             instead of guessing."
                .into(),
        ),
        skills_dir: Some(skills),
        history_path: Some(history_path.clone()),
        max_ctx_chars: 24000,
        ..Config::default()
    };

    let tools = || vec![tool_exec(), tool_time_now()];
    let mut agent = Agent::new(cfg.clone(), platform(), tools());

    println!(
        "hermes-lite (rust) | model={} | session='{session}' | \
         commands: sessions | session <name> | quit",
        agent.config().model
    );

    let mut line = String::new();
    let mut reply = String::new();
    loop {
        print!("\x1b[36myou>\x1b[0m ");
        let _ = std::io::stdout().flush();
        line.clear();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let msg = line.trim().to_string();
        if msg.is_empty() {
            continue;
        }
        if msg == "quit" || msg == "exit" {
            break;
        }
        if msg == "sessions" {
            println!("sessions in {SESSIONS_DIR}/:");
            let names: Vec<String> = std::fs::read_dir(SESSIONS_DIR)
                .map(|it| {
                    it.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !n.starts_with('.'))
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                println!("  (none yet)");
            } else {
                for name in names {
                    println!("  {name}");
                }
            }
            continue;
        }
        if msg == "session" {
            println!("usage: session <name>");
            continue;
        }
        if let Some(rest) = msg.strip_prefix("session ") {
            let name = rest.trim();
            if !valid_session_name(name) {
                println!("usage: session <name> (plain name, no '/', '..' or leading '.')");
            } else {
                history_path = session_path(name);
                cfg.history_path = Some(history_path.clone());
                agent = Agent::new(cfg.clone(), platform(), tools());
                println!("session '{name}' active ({history_path})");
            }
            continue;
        }
        let t0 = std::time::Instant::now();
        print!("\x1b[33mhermes>\x1b[0m ");
        let _ = std::io::stdout().flush();
        match agent.chat_stream(&msg, &mut on_fragment, &mut reply) {
            Ok(()) => println!("\x1b[2m({} ms)\x1b[0m", t0.elapsed().as_millis()),
            Err(e) => {
                let why = if reply.is_empty() {
                    format!("{e:?}")
                } else {
                    reply.clone()
                };
                println!("\n\x1b[31merror\x1b[0m: {why}");
            }
        }
        reply.clear();
    }
}
