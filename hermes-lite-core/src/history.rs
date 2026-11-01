//! JSONL conversation persistence with budget trimming and in-place
//! compaction (flash-wear friendly). One JSON object per line, same shape
//! as the wire format so replay is trivial.

use alloc::format;
use alloc::string::{String, ToString};

use serde_json::{json, Value};

use crate::agent::{Agent, Msg};
use crate::platform::Level;

/// Serialize one message as a JSON object (no trailing newline).
pub fn emit_msg(out: &mut String, m: &Msg) {
    if m.role == "assistant" && m.tool_calls.is_some() {
        out.push_str("{\"role\":\"assistant\",\"content\":");
        match &m.content {
            Some(c) if !c.is_empty() => out.push_str(&json_quote(c)),
            _ => out.push_str("null"),
        }
        out.push_str(",\"tool_calls\":");
        out.push_str(m.tool_calls.as_deref().unwrap_or("[]"));
        out.push('}');
    } else if m.role == "tool" {
        out.push_str("{\"role\":\"tool\",\"tool_call_id\":");
        out.push_str(&json_quote(m.tool_call_id.as_deref().unwrap_or("")));
        out.push_str(",\"content\":");
        out.push_str(&json_quote(m.content.as_deref().unwrap_or("")));
        out.push('}');
    } else {
        out.push_str("{\"role\":");
        out.push_str(&json_quote(&m.role));
        out.push_str(",\"content\":");
        out.push_str(&json_quote(m.content.as_deref().unwrap_or("")));
        out.push('}');
    }
}

/// Quote + escape a string as a JSON string literal (including quotes).
pub fn json_quote(s: &str) -> String {
    // Serialize for &str is infallible in practice; default on the
    // impossible error keeps the crate unwrap-free.
    serde_json::to_string(s).unwrap_or_default()
}

pub(crate) fn msg_chars(m: &Msg) -> usize {
    m.role.len()
        + 16
        + m.content.as_deref().map_or(0, str::len)
        + m.tool_calls.as_deref().map_or(0, str::len)
        + m.tool_call_id.as_deref().map_or(0, str::len)
}

impl Agent {
    pub(crate) fn history_append_tail(&mut self) {
        let Some(path) = self.cfg.history_path.clone() else {
            return;
        };
        let mut line = String::new();
        if let Some(last) = self.msgs.last() {
            emit_msg(&mut line, last);
        }
        line.push('\n');
        if self.platform.fs_append(&path, line.as_bytes()).is_err() {
            return;
        }
        self.hist_size = self.hist_size.saturating_add(line.len());

        // compact when the file outgrows 3/4 of its load scratch; keep at
        // most half the scratch so the next trigger needs real growth
        if self.hist_size > self.cfg.buf_hist * 3 / 4 {
            let mut keep = self.cfg.max_ctx_chars / 2;
            if keep > self.cfg.buf_hist / 2 {
                keep = self.cfg.buf_hist / 2;
            }
            self.compact(keep);
        }
    }

    /// Rewrite the history file keeping only a recent tail.
    pub fn compact(&mut self, keep_chars: usize) {
        let n = self.msgs.len();
        if n == 0 {
            return;
        }
        // keep the newest tail that fits keep_chars
        let mut total = 0usize;
        let mut start = n;
        while start > 0 && total + msg_chars(&self.msgs[start - 1]) <= keep_chars {
            start -= 1;
            total += msg_chars(&self.msgs[start]);
        }
        // a replayable thread starts at a user message
        while start < n && self.msgs[start].role != "user" {
            start += 1;
        }
        if start >= n {
            // nothing fit: fall back to the final user turn
            start = match (0..n).rev().find(|i| self.msgs[*i].role == "user") {
                Some(i) => i,
                None => return,
            };
        }

        let mut body = String::new();
        for m in &self.msgs[start..] {
            emit_msg(&mut body, m);
            body.push('\n');
        }
        if let Some(path) = self.cfg.history_path.clone() {
            if self.platform.fs_write(&path, body.as_bytes()).is_ok() {
                self.hist_size = body.len();
            }
        }
    }

    pub(crate) fn load_history(&mut self) {
        let Some(path) = self.cfg.history_path.clone() else {
            return;
        };
        let Ok(bytes) = self.platform.fs_read(&path, self.cfg.buf_hist) else {
            return; // no history yet
        };
        self.hist_size = bytes.len();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue; // possibly truncated head; skip the line
            };
            let Some(role) = v.get("role").and_then(|x| x.as_str()) else {
                continue;
            };
            let content = v
                .get("content")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let tool_calls = match v.get("tool_calls") {
                Some(t) if t.is_array() => Some(t_raw(t)),
                _ => None,
            };
            let tool_call_id = v
                .get("tool_call_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            self.msgs.push(Msg {
                role: role.to_string(),
                content,
                tool_calls,
                tool_call_id,
            });
        }
        self.platform.log(
            Level::Debug,
            &format!("history: {} msgs loaded", self.msgs.len()),
        );
    }
}

/// Re-serialize a parsed tool_calls array to raw wire text.
fn t_raw(arr: &Value) -> String {
    let mut out = String::from("[");
    if let Some(items) = arr.as_array() {
        for (i, tc) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let f = tc.get("function");
            let name = f
                .and_then(|x| x.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let args = f
                .and_then(|x| x.get("arguments"))
                .and_then(|x| x.as_str())
                .unwrap_or("{}");
            out.push_str(
                &json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                })
                .to_string(),
            );
        }
    }
    out.push(']');
    out
}
