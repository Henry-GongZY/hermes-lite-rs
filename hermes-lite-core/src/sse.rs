//! SSE delta accumulation — Rust port of the C `sacc_t`/`sse_on_line`.

use alloc::string::String;
use alloc::vec::Vec;

use serde_json::Value;

pub const MAX_STREAM_CALLS: usize = 8;

pub struct Slot {
    pub id: String,
    pub name: String,
    pub args: String,
}

/// Accumulates one streamed assistant message: content plus up to
/// `MAX_STREAM_CALLS` tool-call slots keyed by the wire `index`.
pub struct SseAcc {
    pub content: String,
    pub slots: Vec<Slot>,
    /// byte budget for content+args; exceeded -> `err`
    pub budget: usize,
    pub used: usize,
    pub err: bool,
}

impl SseAcc {
    pub fn new(budget: usize) -> Self {
        SseAcc {
            content: String::new(),
            slots: Vec::new(),
            budget,
            used: 0,
            err: false,
        }
    }

    fn slot(&mut self, idx: usize) -> &mut Slot {
        while self.slots.len() <= idx {
            self.slots.push(Slot {
                id: String::new(),
                name: String::new(),
                args: String::new(),
            });
        }
        &mut self.slots[idx]
    }

    fn charge(&mut self, n: usize) -> bool {
        self.used = self.used.saturating_add(n);
        if self.used > self.budget {
            self.err = true;
            return false;
        }
        true
    }
}

/// Handle one SSE `data:` payload. Returns true to abort the transfer
/// (the agent never aborts; kept for platform-API parity).
pub fn on_line(acc: &mut SseAcc, data: &str) -> bool {
    if data == "[DONE]" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return false; // keepalives / comments / junk
    };
    let Some(delta) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
    else {
        return false;
    };

    if let Some(c) = delta.get("content").and_then(|x| x.as_str()) {
        if !c.is_empty() && acc.charge(c.len()) {
            acc.content.push_str(c);
        }
    }

    if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            if idx >= MAX_STREAM_CALLS {
                continue;
            }
            if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                let s = acc.slot(idx);
                s.id.push_str(id);
                acc.charge(id.len());
            }
            if let Some(f) = tc.get("function") {
                if let Some(nm) = f.get("name").and_then(|x| x.as_str()) {
                    let s = acc.slot(idx);
                    s.name.push_str(nm);
                    acc.charge(nm.len());
                }
                if let Some(ar) = f.get("arguments").and_then(|x| x.as_str()) {
                    if !ar.is_empty() {
                        let s = acc.slot(idx);
                        s.args.push_str(ar);
                        acc.charge(ar.len());
                    }
                }
            }
            if acc.err {
                return false;
            }
        }
    }
    false
}
