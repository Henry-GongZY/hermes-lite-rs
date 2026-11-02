//! The agent loop — Rust port of the C `agent.c`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use serde_json::{json, Value};

use crate::history::{emit_msg, json_quote, msg_chars};
use crate::platform::{HlError, HlResult, Level, Platform};
use crate::skills;
use crate::sse::{self, SseAcc};

/// Tool callback: (raw arguments JSON, output buffer).
pub type ToolFn = dyn Fn(&str, &mut String);

const DEF_MAX_ITERS: usize = 6;
const HL_HTTP_ATTEMPTS: u32 = 3; // 1 initial + 2 retries on transport/429/5xx
const DEF_CTX_CHARS: usize = 16000;

/// A locally executable tool. `f` receives the raw `arguments` JSON text
/// and writes its result (or an "error: ..." message) into the output
/// string — same contract as the C `hl_tool_fn`.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema, embedded verbatim.
    pub parameters_json: &'static str,
    pub f: Box<ToolFn>,
}

#[derive(Clone)]
pub struct Config {
    /// "https://host/v1/chat/completions"
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub system_prompt: Option<String>,
    /// enables skills + the built-in read_skill tool
    pub skills_dir: Option<String>,
    /// optional JSONL persistence
    pub history_path: Option<String>,
    /// per-request message char budget; 0 -> 16000
    pub max_ctx_chars: usize,
    /// tool rounds per turn; 0 -> 6
    pub max_tool_iters: usize,
    /// response byte budget (enforced by platforms); 0 -> 65536
    pub max_response_bytes: usize,
    /// history load scratch / compaction trigger; 0 -> 32768
    pub buf_hist: usize,
    /// skill file read cap; 0 -> 8192
    pub skill_file_cap: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            system_prompt: None,
            skills_dir: None,
            history_path: None,
            max_ctx_chars: DEF_CTX_CHARS,
            max_tool_iters: DEF_MAX_ITERS,
            max_response_bytes: 65536,
            buf_hist: 32768,
            skill_file_cap: 8192,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Msg {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<String>, // raw JSON array
    pub tool_call_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct TcRef {
    pub id: String,
    pub name: String,
    pub args: String,
}

pub struct Agent {
    pub(crate) cfg: Config,
    pub(crate) platform: Arc<dyn Platform>,
    pub(crate) tools: Vec<Tool>,
    pub(crate) msgs: Vec<Msg>,
    pub(crate) hist_size: usize,
}

fn tool_calls_json(tcs: &[TcRef]) -> String {
    let arr: Vec<Value> = tcs
        .iter()
        .map(|tc| {
            json!({
                "id": tc.id,
                "type": "function",
                "function": { "name": tc.name, "arguments": tc.args }
            })
        })
        .collect();
    Value::Array(arr).to_string()
}

impl Agent {
    /// `cfg` strings are owned; `tools` is copied. If `cfg.skills_dir` is
    /// set, a built-in read_skill tool is appended.
    pub fn new(cfg: Config, platform: Arc<dyn Platform>, mut tools: Vec<Tool>) -> Self {
        if let Some(dir) = cfg.skills_dir.clone() {
            let plat = platform.clone();
            let skill_cap = cfg.skill_file_cap;
            tools.push(Tool {
                name: "read_skill",
                description: "Load the full instructions of a skill by name. \
                              Call this before acting on any skill.",
                parameters_json: "{\"type\":\"object\",\"properties\":{\"name\":\
                                  {\"type\":\"string\",\"description\":\"skill name\"}},\
                                  \"required\":[\"name\"]}",
                f: Box::new(move |args, out| {
                    let v: Value = serde_json::from_str(if args.is_empty() { "{}" } else { args })
                        .unwrap_or(Value::Null);
                    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    if name.is_empty() {
                        out.push_str("error: missing skill name");
                        return;
                    }
                    match skills::load(&*plat, &dir, name, skill_cap) {
                        Ok(body) => out.push_str(&body),
                        Err(_) => out.push_str(&format!("error: skill '{name}' not found")),
                    }
                }),
            });
        }
        let mut agent = Agent {
            cfg,
            platform,
            tools,
            msgs: Vec::new(),
            hist_size: 0,
        };
        agent.load_history();
        agent
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.cfg
    }

    fn push_msg(
        &mut self,
        role: &str,
        content: Option<String>,
        tool_calls: Option<String>,
        tool_call_id: Option<String>,
    ) {
        self.msgs.push(Msg {
            role: role.into(),
            content,
            tool_calls,
            tool_call_id,
        });
    }

    fn log(&self, level: Level, msg: &str) {
        self.platform.log(level, msg);
    }

    /// First message to send: drop oldest until under budget, and never
    /// start with a dangling tool/assistant-tool_calls block (rewind to the
    /// first user message). C `trim_start` port.
    fn trim_start(&self) -> &[Msg] {
        let budget = if self.cfg.max_ctx_chars > 0 {
            self.cfg.max_ctx_chars
        } else {
            DEF_CTX_CHARS
        };
        let mut total: usize = self.msgs.iter().map(msg_chars).sum();
        let mut i = 0;
        while i < self.msgs.len() && total > budget {
            total -= msg_chars(&self.msgs[i]);
            i += 1;
        }
        while i < self.msgs.len() && self.msgs[i].role != "user" {
            i += 1;
        }
        if i >= self.msgs.len() {
            return &self.msgs[self.msgs.len() - 1..];
        }
        &self.msgs[i..]
    }

    fn build_request(&self, stream: bool) -> HlResult<String> {
        let mut sys = String::new();
        sys.push_str(
            self.cfg
                .system_prompt
                .as_deref()
                .unwrap_or("You are Hermes Lite, a helpful assistant."),
        );
        if let Some(dir) = &self.cfg.skills_dir {
            sys.push_str(
                "\n\n# Skills\nBefore following instructions from a skill, \
                          load it with the read_skill tool. Available skills:",
            );
            skills::build_index(&*self.platform, dir, &mut sys, self.cfg.skill_file_cap);
        }

        let mut s = String::with_capacity(4096);
        s.push_str("{\"model\":");
        s.push_str(&json_quote(&self.cfg.model));
        s.push_str(",\"stream\":");
        s.push_str(if stream { "true" } else { "false" });
        s.push_str(",\"messages\":[{\"role\":\"system\",\"content\":");
        s.push_str(&json_quote(&sys));
        s.push('}');
        for m in self.trim_start() {
            s.push(',');
            emit_msg(&mut s, m);
        }
        s.push(']');
        if !self.tools.is_empty() {
            s.push_str(",\"tools\":[");
            for (i, t) in self.tools.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"type\":\"function\",\"function\":{\"name\":");
                s.push_str(&json_quote(t.name));
                s.push_str(",\"description\":");
                s.push_str(&json_quote(t.description));
                s.push_str(",\"parameters\":");
                s.push_str(t.parameters_json);
                s.push_str("}}");
            }
            s.push(']');
        }
        s.push('}');
        Ok(s)
    }

    fn exec_and_record(&mut self, tcs: &[TcRef]) {
        for tc in tcs {
            self.log(Level::Info, &format!("tool {}({})", tc.name, tc.args));
            let mut out = String::new();
            match self.tools.iter().find(|t| t.name == tc.name) {
                None => out = format!("error: unknown tool '{}'", tc.name),
                Some(t) => {
                    (t.f)(&tc.args, &mut out);
                    if out.is_empty() {
                        out = format!("error: tool '{}' failed", tc.name);
                    }
                }
            }
            let preview: String = out.chars().take(120).collect();
            self.log(Level::Info, &format!("-> {preview}"));
            self.push_msg("tool", Some(out), None, Some(tc.id.clone()));
            self.history_append_tail();
        }
    }

    /// One user turn; runs the tool loop until a final assistant message.
    pub fn chat(&mut self, user_msg: &str, reply: &mut String) -> HlResult<()> {
        self.turn(user_msg, None, false, reply)
    }

    /// One user turn with SSE streaming. `on_text` sees content fragments
    /// live; `reply` still receives the complete final text. Falls back to
    /// buffered mode when the platform has no streaming support (on_text
    /// then fires once with the whole message).
    pub fn chat_stream(
        &mut self,
        user_msg: &str,
        on_text: &mut (dyn FnMut(&str) + '_),
        reply: &mut String,
    ) -> HlResult<()> {
        self.turn(user_msg, Some(on_text), true, reply)
    }

    fn turn(
        &mut self,
        user_msg: &str,
        mut on_text: Option<&mut (dyn FnMut(&str) + '_)>,
        prefer_stream: bool,
        reply: &mut String,
    ) -> HlResult<()> {
        reply.clear();
        self.push_msg("user", Some(user_msg.to_string()), None, None);
        self.history_append_tail();

        let iters = if self.cfg.max_tool_iters > 0 {
            self.cfg.max_tool_iters
        } else {
            DEF_MAX_ITERS
        };

        for _ in 0..iters {
            let stream = prefer_stream && self.platform.supports_streaming();
            let req = self.build_request(stream)?;

            let content;
            let mut tcs: Vec<TcRef> = Vec::new();
            let mut truncated = false;

            if stream {
                let mut acc = SseAcc::new(self.cfg.max_response_bytes);
                let mut delivered = 0usize;
                let mut errbuf = String::new();
                let mut attempt: u32 = 0;
                #[allow(unused_assignments)] // loop always re-assigns before break
                let mut st = Err(HlError::Transport);
                loop {
                    if attempt > 0 {
                        // fresh accumulator: a retried stream must not double-
                        // deliver fragments (only retried when nothing arrived)
                        acc = SseAcc::new(self.cfg.max_response_bytes);
                        delivered = 0;
                    }
                    let res = {
                        // deliver content fragments incrementally, one callback
                        // per completed SSE line (agent.rs C port parity)
                        let mut cb = |data: &str| -> bool {
                            if sse::on_line(&mut acc, data) {
                                return true;
                            }
                            #[allow(clippy::option_as_ref_deref)]
                            // clippy suggests as_deref_mut, unavailable for dyn traits
                            if let Some(f) = on_text.as_mut().map(|f| &mut **f) {
                                let cur = acc.content.len();
                                if cur > delivered {
                                    f(&acc.content[delivered..cur]);
                                    delivered = cur;
                                }
                            }
                            false
                        };
                        self.platform.http_post_stream(
                            &self.cfg.api_url,
                            &self.cfg.api_key,
                            &req,
                            &mut cb,
                            &mut errbuf,
                        )
                    };
                    let has_data = !acc.content.is_empty() || !acc.slots.is_empty();
                    let retryable = match &res {
                        Err(HlError::Transport) => !has_data,
                        Ok(c) => (*c == 429 || (500..600).contains(c)) && !has_data,
                        _ => false,
                    };
                    st = res;
                    if !retryable || attempt >= HL_HTTP_ATTEMPTS - 1 {
                        break;
                    }
                    attempt += 1;
                    self.log(
                        Level::Info,
                        &format!("http stream attempt {attempt} failed; backing off"),
                    );
                    self.platform.sleep_ms(500u64 << (attempt - 1));
                }
                let truncated_here = acc.err;
                truncated = truncated_here;
                if truncated_here {
                    self.log(Level::Info, "stream exceeded budget; truncating response");
                }
                match st {
                    Err(e) => {
                        self.log(Level::Error, "http stream transport error");
                        return Err(e);
                    }
                    Ok(200) => {}
                    Ok(code) => {
                        let snippet: String = errbuf.chars().take(200).collect();
                        self.log(Level::Error, &format!("http {code}: {snippet}"));
                        return Err(HlError::Status(code, snippet));
                    }
                }
                content = acc.content;
                tcs = acc
                    .slots
                    .into_iter()
                    .map(|s| TcRef {
                        id: s.id,
                        name: s.name,
                        args: if s.args.is_empty() {
                            "{}".into()
                        } else {
                            s.args
                        },
                    })
                    .collect();
            } else {
                let mut attempt: u32 = 0;
                let mut outcome: Result<(u16, Vec<u8>), HlError>;
                loop {
                    let mut resp = Vec::new();
                    let res = self.platform.http_post(
                        &self.cfg.api_url,
                        &self.cfg.api_key,
                        &req,
                        &mut resp,
                        self.cfg.max_response_bytes,
                    );
                    let retryable = match &res {
                        Err(HlError::Transport) => true,
                        Ok(c) => *c == 429 || (500..600).contains(c),
                        _ => false,
                    };
                    outcome = res.map(|code| (code, resp));
                    if !retryable || attempt >= HL_HTTP_ATTEMPTS - 1 {
                        break;
                    }
                    attempt += 1;
                    self.log(
                        Level::Info,
                        &format!("http attempt {attempt} failed; backing off"),
                    );
                    self.platform.sleep_ms(500u64 << (attempt - 1));
                }
                let (st, resp) = match outcome {
                    Ok(pair) => pair,
                    Err(e) => {
                        self.log(Level::Error, "http transport error");
                        return Err(e);
                    }
                };
                if st != 200 {
                    let snippet: String =
                        String::from_utf8_lossy(&resp).chars().take(200).collect();
                    self.log(Level::Error, &format!("http {st}: {snippet}"));
                    return Err(HlError::Status(st, snippet));
                }
                let v: Value = serde_json::from_str(&String::from_utf8_lossy(&resp))
                    .map_err(|_| HlError::BadJson)?;
                let Some(msg) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                else {
                    self.log(Level::Error, "no choices[0].message in response");
                    return Err(HlError::BadJson);
                };
                content = msg
                    .get("content")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(arr) = msg.get("tool_calls").and_then(|x| x.as_array()) {
                    for tc in arr.iter().take(crate::sse::MAX_STREAM_CALLS) {
                        let f = tc.get("function");
                        tcs.push(TcRef {
                            id: tc.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                            name: f
                                .and_then(|x| x.get("name"))
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .into(),
                            args: f
                                .and_then(|x| x.get("arguments"))
                                .and_then(|x| x.as_str())
                                .filter(|a| !a.is_empty())
                                .unwrap_or("{}")
                                .into(),
                        });
                    }
                }
            }

            let tc_raw = if tcs.is_empty() {
                None
            } else {
                Some(tool_calls_json(&tcs))
            };
            let has_content = !content.is_empty();
            self.push_msg(
                "assistant",
                has_content.then(|| content.clone()),
                tc_raw,
                None,
            );
            self.history_append_tail();

            if tcs.is_empty() {
                reply.push_str(&content);
                if truncated {
                    reply.push_str("\n\n[response truncated: exceeded memory budget]");
                }
                if !stream {
                    if let Some(f) = on_text.as_deref_mut() {
                        // buffered fallback: one fragment with the whole text
                        f(&reply.clone());
                    }
                }
                return Ok(());
            }
            self.exec_and_record(&tcs);
        }

        let msg = "error: tool iteration limit reached";
        reply.push_str(msg);
        Err(HlError::TooLarge)
    }
}
