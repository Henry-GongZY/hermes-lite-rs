//! Agent-loop tests against a scripted mock platform — the Rust port of
//! the C `test_agent.c` suite (same scenarios, same assertions).
#![allow(clippy::arc_with_non_send_sync)] // single-threaded tests; Rc-style sharing under Arc

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use hermes_lite_core::{Agent, Config, HlError, Level, Platform, Tool};
use serde_json::json;

const MAX_CAP: usize = 16;

#[derive(Default)]
pub struct MockState {
    captured: Vec<String>,
    buffered: Vec<String>,
    streams: Vec<String>,
    bpos: usize,
    spos: usize,
    frag_log: String,
    frag_calls: usize,
    /// transport failures before a scripted response is served
    fail_first: usize,
}

pub struct Mock {
    s: Rc<RefCell<MockState>>,
    pub with_stream: bool,
}

impl Mock {
    pub fn new(with_stream: bool) -> (Rc<RefCell<MockState>>, Self) {
        let s = Rc::new(RefCell::new(MockState::default()));
        (s.clone(), Mock { s, with_stream })
    }
}

fn collect_frag(state: &Rc<RefCell<MockState>>, frag: &str) {
    let mut s = state.borrow_mut();
    s.frag_log.push_str(frag);
    s.frag_calls += 1;
}

impl Platform for Mock {
    fn http_post(
        &self,
        _url: &str,
        _auth: &str,
        body: &str,
        resp: &mut Vec<u8>,
        _cap: usize,
    ) -> Result<u16, HlError> {
        let mut s = self.s.borrow_mut();
        assert!(s.captured.len() < MAX_CAP);
        s.captured.push(body.to_string());
        if s.fail_first > 0 {
            s.fail_first -= 1;
            return Err(HlError::Transport);
        }
        if s.bpos >= s.buffered.len() {
            return Err(HlError::Transport);
        }
        let r = s.buffered[s.bpos].clone();
        s.bpos += 1;
        resp.extend_from_slice(r.as_bytes());
        Ok(200)
    }

    fn supports_streaming(&self) -> bool {
        self.with_stream
    }

    fn http_post_stream(
        &self,
        _url: &str,
        _auth: &str,
        body: &str,
        on_line: &mut dyn FnMut(&str) -> bool,
        _errbuf: &mut String,
    ) -> Result<u16, HlError> {
        let mut s = self.s.borrow_mut();
        assert!(s.captured.len() < MAX_CAP);
        s.captured.push(body.to_string());
        if s.fail_first > 0 {
            s.fail_first -= 1;
            return Err(HlError::Transport);
        }
        if s.spos >= s.streams.len() {
            return Err(HlError::Transport);
        }
        let payload = s.streams[s.spos].clone();
        s.spos += 1;
        drop(s);
        for line in payload.split('\n') {
            on_line(line);
        }
        Ok(200)
    }

    fn fs_read(&self, path: &str, cap: usize) -> Result<Vec<u8>, HlError> {
        match std::fs::read(path) {
            Ok(mut b) => {
                b.truncate(cap);
                Ok(b)
            }
            Err(_) => Err(HlError::Fs),
        }
    }
    fn fs_write(&self, path: &str, data: &[u8]) -> Result<(), HlError> {
        std::fs::write(path, data).map_err(|_| HlError::Fs)
    }
    fn fs_append(&self, path: &str, data: &[u8]) -> Result<(), HlError> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| HlError::Fs)?;
        f.write_all(data).map_err(|_| HlError::Fs)
    }
    fn fs_list(&self, dir: &str) -> Result<Vec<String>, HlError> {
        let mut names = Vec::new();
        for e in std::fs::read_dir(dir).map_err(|_| HlError::Fs)? {
            let e = e.map_err(|_| HlError::Fs)?;
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn time_ms(&self) -> u64 {
        0
    }
    fn log(&self, _level: Level, _msg: &str) {}
}

fn resp_reply(text: &str) -> String {
    json!({"choices":[{"message":{"role":"assistant","content":text}}]}).to_string()
}

fn resp_tool_call(id: &str, name: &str, args: &str) -> String {
    json!({"choices":[{"message":{"role":"assistant","content":null,
        "tool_calls":[{"id":id,"type":"function",
        "function":{"name":name,"arguments":args}}]}}]})
    .to_string()
}

fn resp_two_calls(id1: &str, args1: &str, id2: &str, args2: &str) -> String {
    json!({"choices":[{"message":{"role":"assistant","content":null,
    "tool_calls":[
        {"id":id1,"type":"function","function":{"name":"add","arguments":args1}},
        {"id":id2,"type":"function","function":{"name":"add","arguments":args2}}
    ]}}]})
    .to_string()
}

fn tool_add() -> Tool {
    Tool {
        name: "add",
        description: "add two ints",
        parameters_json: "{\"type\":\"object\"}",
        f: Box::new(|args, out| {
            let v: serde_json::Value =
                serde_json::from_str(if args.is_empty() { "{}" } else { args }).unwrap();
            match (
                v.get("a").and_then(|x| x.as_i64()),
                v.get("b").and_then(|x| x.as_i64()),
            ) {
                (Some(a), Some(b)) => out.push_str(&(a + b).to_string()),
                _ => out.push_str("error: need a and b"),
            }
        }),
    }
}

fn tool_boom() -> Tool {
    Tool {
        name: "boom",
        description: "",
        parameters_json: "{}",
        f: Box::new(|_args, out| out.push_str("error: exploded on purpose")),
    }
}

static TMP_N: AtomicU32 = AtomicU32::new(0);

fn tmpdir(tag: &str) -> String {
    let n = TMP_N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("hl_rs_test_{}_{}_{}", std::process::id(), tag, n));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

fn base_cfg(dir: &str) -> Config {
    Config {
        api_url: "https://mock/v1/chat/completions".into(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        system_prompt: Some("be brief".into()),
        history_path: Some(format!("{dir}/history.jsonl")),
        ..Config::default()
    }
}

fn frag_of(state: &Rc<RefCell<MockState>>) -> (String, usize) {
    let s = state.borrow();
    (s.frag_log.clone(), s.frag_calls)
}

macro_rules! reset {
    ($s:expr) => {{
        let mut st = $s.borrow_mut();
        st.captured.clear();
        st.buffered.clear();
        st.streams.clear();
        st.bpos = 0;
        st.spos = 0;
        st.frag_log.clear();
        st.frag_calls = 0;
    }};
}

#[test]
fn simple_reply() {
    let (s, mock) = Mock::new(false);
    s.borrow_mut().buffered.push(resp_reply("hello there"));
    let cfg = base_cfg(&tmpdir("simple"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    let r = a.chat("hi", &mut reply);
    assert!(r.is_ok());
    assert_eq!(reply, "hello there");
    let st = s.borrow();
    assert_eq!(st.captured.len(), 1);
    assert!(st.captured[0].contains("\"content\":\"hi\""));
    assert!(st.captured[0].contains("be brief"));
    assert!(!st.captured[0].contains("\"tools\""));
}

#[test]
fn tool_loop() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered
            .push(resp_tool_call("call_1", "add", "{\"a\":1,\"b\":2}"));
        st.buffered.push(resp_reply("the sum is 3"));
    }
    let cfg = base_cfg(&tmpdir("tool"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![tool_add()]);
    let mut reply = String::new();
    assert!(a.chat("what is 1+2", &mut reply).is_ok());
    assert_eq!(reply, "the sum is 3");
    let st = s.borrow();
    assert_eq!(st.captured.len(), 2);
    assert!(st.captured[1].contains("\"tool_call_id\":\"call_1\""));
    assert!(st.captured[1].contains("\"content\":\"3\""));
    let asst = st.captured[1].find("\"tool_calls\":[{").unwrap();
    let toolm = st.captured[1].find("\"role\":\"tool\"").unwrap();
    assert!(asst < toolm, "assistant block precedes result");
}

#[test]
fn parallel_calls() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered.push(resp_two_calls(
            "c1",
            "{\"a\":1,\"b\":2}",
            "c2",
            "{\"a\":10,\"b\":20}",
        ));
        st.buffered.push(resp_reply("both done"));
    }
    let cfg = base_cfg(&tmpdir("parallel"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![tool_add()]);
    let mut reply = String::new();
    assert!(a.chat("add twice", &mut reply).is_ok());
    let st = s.borrow();
    assert!(st.captured[1].contains("\"content\":\"3\""));
    assert!(st.captured[1].contains("\"content\":\"30\""));
    assert!(st.captured[1].contains("\"tool_call_id\":\"c2\""));
}

#[test]
fn unknown_tool() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered.push(resp_tool_call("call_x", "nope", "{}"));
        st.buffered.push(resp_reply("ok"));
    }
    let cfg = base_cfg(&tmpdir("unknown"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    assert!(a.chat("use nope", &mut reply).is_ok());
    assert!(s.borrow().captured[1].contains("unknown tool 'nope'"));
}

#[test]
fn tool_failure() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered.push(resp_tool_call("call_f", "boom", "{}"));
        st.buffered.push(resp_reply("recovered"));
    }
    let cfg = base_cfg(&tmpdir("failure"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![tool_boom()]);
    let mut reply = String::new();
    assert!(a.chat("explode", &mut reply).is_ok());
    assert!(s.borrow().captured[1].contains("exploded on purpose"));
}

#[test]
fn iteration_limit() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        for _ in 0..12 {
            st.buffered.push(resp_tool_call("call_i", "add", "{}"));
        }
    }
    let cfg = base_cfg(&tmpdir("limit"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![tool_add()]);
    a.config_mut().max_tool_iters = 3;
    let mut reply = String::new();
    assert!(a.chat("loop forever", &mut reply).is_err());
    assert!(reply.contains("iteration limit"));
    assert_eq!(s.borrow().captured.len(), 3);
}

#[test]
fn skills() {
    let dir = tmpdir("skills");
    let sdir = format!("{dir}/skills");
    std::fs::create_dir_all(format!("{sdir}/demo")).unwrap();
    std::fs::write(
        format!("{sdir}/demo/SKILL.md"),
        "---\nname: demo\ndescription: demo skill for tests\n---\nDo the demo thing.",
    )
    .unwrap();

    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered.push(resp_tool_call(
            "call_s",
            "read_skill",
            "{\"name\":\"demo\"}",
        ));
        st.buffered.push(resp_reply("did the skill"));
    }
    let mut cfg = base_cfg(&dir);
    cfg.skills_dir = Some(sdir);
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    assert!(a.chat("use demo", &mut reply).is_ok());
    let st = s.borrow();
    assert!(st.captured[0].contains("demo: demo skill for tests"));
    assert!(st.captured[1].contains("Do the demo thing."));
}

#[test]
fn context_trim() {
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.buffered.push(resp_reply("first ok"));
        st.buffered.push(resp_reply("second ok"));
    }
    let big = "A".repeat(9000);
    let mut cfg = base_cfg(&tmpdir("trim"));
    cfg.max_ctx_chars = 500;
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    a.chat(&format!("FIRSTMARKER {big}"), &mut reply).unwrap();
    a.chat(&format!("SECONDMARKER {big}"), &mut reply).unwrap();
    let st = s.borrow();
    assert_eq!(st.captured.len(), 2);
    assert!(st.captured[1].contains("SECONDMARKER"));
    assert!(!st.captured[1].contains("FIRSTMARKER"));
}

#[test]
fn history_reload() {
    let dir = tmpdir("reload");
    let (s, mock) = Mock::new(false);
    let mock = Arc::new(mock);
    {
        s.borrow_mut().buffered.push(resp_reply("ack"));
        let cfg = base_cfg(&dir);
        let mut a = Agent::new(cfg, mock.clone(), vec![]);
        let mut reply = String::new();
        a.chat("PERSISTED user line", &mut reply).unwrap();
    }
    reset!(s);
    s.borrow_mut().buffered.push(resp_reply("ack2"));
    let cfg = base_cfg(&dir);
    let mut b = Agent::new(cfg, mock.clone(), vec![]);
    let mut reply = String::new();
    b.chat("new question", &mut reply).unwrap();
    let st = s.borrow();
    assert!(st.captured[0].contains("PERSISTED user line"));
    assert!(st.captured[0].contains("new question"));
}

#[test]
fn stream_reply() {
    let (s, mock) = Mock::new(true);
    s.borrow_mut().streams.push(
        "{\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\
         {\"choices\":[{\"delta\":{\"content\":\"lo w\"}}]}\n\
         \n\
         {\"choices\":[{\"delta\":{\"content\":\"orld!\"}}]}\n\
         [DONE]"
            .into(),
    );
    let cfg = base_cfg(&tmpdir("stream"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    let r = a.chat_stream("hi", &mut |f| collect_frag(&s, f), &mut reply);
    assert!(r.is_ok());
    assert_eq!(reply, "Hello world!");
    let (log, calls) = frag_of(&s);
    assert_eq!(log, "Hello world!");
    assert_eq!(calls, 3);
    assert!(s.borrow().captured[0].contains("\"stream\":true"));
}

#[test]
fn stream_tool_call() {
    let (s, mock) = Mock::new(true);
    {
        let mut st = s.borrow_mut();
        // arguments split across two deltas
        st.streams.push(
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\
             \"function\":{\"name\":\"add\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\
             {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\":1,\\\"b\\\":2}\"}}]}}]}\n\
             [DONE]"
                .into(),
        );
        st.streams
            .push("{\"choices\":[{\"delta\":{\"content\":\"3 it is\"}}]}\n[DONE]".into());
    }
    let cfg = base_cfg(&tmpdir("streamtool"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![tool_add()]);
    let mut reply = String::new();
    assert!(a
        .chat_stream("add 1 2", &mut |f| collect_frag(&s, f), &mut reply)
        .is_ok());
    assert_eq!(reply, "3 it is");
    let st = s.borrow();
    assert_eq!(st.captured.len(), 2);
    assert!(st.captured[1].contains("\"tool_call_id\":\"c1\""));
    assert!(st.captured[1].contains("\"content\":\"3\""));
    assert!(st.captured[1].contains("\\\"a\\\":1,\\\"b\\\":2"));
}

#[test]
fn stream_fallback() {
    let (s, mock) = Mock::new(false); // platform without streaming
    s.borrow_mut().buffered.push(resp_reply("fallback text"));
    let cfg = base_cfg(&tmpdir("fallback"));
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    assert!(a
        .chat_stream("hi", &mut |f| collect_frag(&s, f), &mut reply)
        .is_ok());
    assert_eq!(reply, "fallback text");
    let (log, calls) = frag_of(&s);
    assert_eq!(calls, 1);
    assert_eq!(log, "fallback text");
    assert!(s.borrow().captured[0].contains("\"stream\":false"));
}

#[test]
fn http_retry() {
    let dir = tmpdir("retry");
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        st.fail_first = 2; // two transport failures, then the scripted reply
        st.buffered.push(resp_reply("recovered after retries"));
    }
    let cfg = base_cfg(&dir);
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    assert!(a.chat("hello", &mut reply).is_ok());
    assert_eq!(reply, "recovered after retries");
    assert_eq!(s.borrow().captured.len(), 3);
}

#[test]
fn stream_retry_no_duplicate_fragments() {
    let dir = tmpdir("streamretry");
    let (s, mock) = Mock::new(true);
    {
        let mut st = s.borrow_mut();
        st.fail_first = 1; // stream transport fails once, before any data
        st.streams
            .push("{\"choices\":[{\"delta\":{\"content\":\"only once\"}}]}\n[DONE]".into());
    }
    let cfg = base_cfg(&dir);
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    assert!(a
        .chat_stream("hi", &mut |f| collect_frag(&s, f), &mut reply)
        .is_ok());
    assert_eq!(reply, "only once");
    let (log, calls) = frag_of(&s);
    assert_eq!(log, "only once", "no duplicated fragments across retry");
    assert_eq!(calls, 1);
    assert_eq!(s.borrow().captured.len(), 2);
}

#[test]
fn history_compact() {
    let dir = tmpdir("compact");
    let hpath = format!("{dir}/compact.jsonl");
    let (s, mock) = Mock::new(false);
    {
        let mut st = s.borrow_mut();
        for _ in 0..6 {
            st.buffered.push(resp_reply("ok"));
        }
    }
    let mut cfg = base_cfg(&dir);
    cfg.history_path = Some(hpath.clone());
    cfg.buf_hist = 2048; // compact triggers past 1536 bytes
    cfg.max_ctx_chars = 900; // keep rule: min(buf_hist/2)=1024

    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let big = "x".repeat(400);
    let mut reply = String::new();
    for i in 0..6 {
        s.borrow_mut().buffered.push(resp_reply("ok"));
        a.chat(&format!("MSG{i} {big}"), &mut reply).unwrap();
    }
    drop(a);

    let meta = std::fs::metadata(&hpath).unwrap();
    assert!(meta.len() < 8192, "file bounded (<8KB), got {}", meta.len());

    let buf = std::fs::read(&hpath).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("MSG5"), "latest user msg kept");
    assert!(!text.contains("MSG0"), "oldest msg dropped");
}

#[test]
fn stream_overflow_budget() {
    let (s, mock) = Mock::new(true);
    s.borrow_mut().streams.push(
        "{\"choices\":[{\"delta\":{\"content\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]}\n[DONE]"
            .into(),
    );
    let mut cfg = base_cfg(&tmpdir("overflow"));
    cfg.max_response_bytes = 16; // tiny budget -> acc overflow
    let mut a = Agent::new(cfg, Arc::new(mock), vec![]);
    let mut reply = String::new();
    let r = a.chat_stream("hi", &mut |_f| {}, &mut reply);
    assert!(matches!(r, Err(HlError::Overflow)));
}
