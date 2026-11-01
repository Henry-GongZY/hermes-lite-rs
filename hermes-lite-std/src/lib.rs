//! std platform backend for hermes-lite-core: ureq (rustls) HTTP +
//! std::fs filesystem. The Rust twin of the C `platform_posix.c`.

use std::io::{BufRead, Read, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hermes_lite_core::{HlError, HlResult, Level, Platform};

pub struct StdPlatform {
    agent: ureq::Agent,
    start: OnceLock<Instant>,
}

impl Default for StdPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl StdPlatform {
    pub fn new() -> Self {
        StdPlatform {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout(Duration::from_secs(300))
                .redirects(3)
                .build(),
            start: OnceLock::new(),
        }
    }

    fn log_line(level: Level, msg: &str) {
        let tag = match level {
            Level::Error => "E",
            Level::Info => "I",
            Level::Debug => "D",
        };
        eprintln!("[hl:{tag}] {msg}");
    }
}

fn split_sse_line(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:")?;
    let payload = payload.strip_prefix(' ').unwrap_or(payload);
    if payload.is_empty() {
        None
    } else {
        Some(payload)
    }
}

impl Platform for StdPlatform {
    fn http_post(
        &self,
        url: &str,
        auth_bearer: &str,
        body: &str,
        resp: &mut Vec<u8>,
        resp_cap: usize,
    ) -> HlResult<u16> {
        let r = self
            .agent
            .post(url)
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {auth_bearer}"))
            .send_string(body);
        let (status, mut reader) = match r {
            Ok(r) => (r.status(), r.into_reader()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_reader()),
            Err(e) => {
                Self::log_line(Level::Error, &format!("curl: {e}"));
                return Err(HlError::Transport);
            }
        };
        // read up to cap+1 so overflow is detectable
        let mut buf = vec![0u8; resp_cap.min(64 * 1024)];
        let mut total = 0usize;
        loop {
            let want = (resp_cap + 1 - total).min(buf.len());
            if want == 0 {
                return Err(HlError::Overflow);
            }
            match reader.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if total > resp_cap {
                        return Err(HlError::Overflow);
                    }
                }
                Err(e) => {
                    Self::log_line(Level::Error, &format!("read: {e}"));
                    return Err(HlError::Transport);
                }
            }
        }
        resp.clear();
        resp.extend_from_slice(&buf[..total]);
        Ok(status)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn http_post_stream(
        &self,
        url: &str,
        auth_bearer: &str,
        body: &str,
        on_line: &mut dyn FnMut(&str) -> bool,
        errbuf: &mut String,
    ) -> HlResult<u16> {
        let r = self
            .agent
            .post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .set("Authorization", &format!("Bearer {auth_bearer}"))
            .send_string(body);
        let (status, reader) = match r {
            Ok(r) => (r.status(), r.into_reader()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_reader()),
            Err(e) => {
                Self::log_line(Level::Error, &format!("curl: {e}"));
                return Err(HlError::Transport);
            }
        };
        if status != 200 {
            let mut raw = String::new();
            let mut br = std::io::BufReader::new(reader);
            // capped error-body capture
            let mut byte = [0u8; 1];
            let mut n = 0usize;
            while n < 4096 {
                match br.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        raw.push(byte[0] as char);
                        n += 1;
                    }
                }
            }
            *errbuf = raw;
            return Ok(status);
        }
        let br = std::io::BufReader::new(reader);
        for line in br.lines() {
            let Ok(line) = line else {
                break;
            };
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if let Some(payload) = split_sse_line(line) {
                if on_line(payload) {
                    break; // caller-requested abort
                }
            }
        }
        Ok(status)
    }

    fn fs_read(&self, path: &str, cap: usize) -> HlResult<Vec<u8>> {
        let mut b = std::fs::read(path).map_err(|_| HlError::Fs)?;
        b.truncate(cap);
        Ok(b)
    }

    fn fs_write(&self, path: &str, data: &[u8]) -> HlResult<()> {
        std::fs::write(path, data).map_err(|_| HlError::Fs)
    }

    fn fs_append(&self, path: &str, data: &[u8]) -> HlResult<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| HlError::Fs)?;
        f.write_all(data).map_err(|_| HlError::Fs)
    }

    fn fs_list(&self, dir: &str) -> HlResult<Vec<String>> {
        let mut names = Vec::new();
        for e in std::fs::read_dir(dir).map_err(|_| HlError::Fs)? {
            let e = e.map_err(|_| HlError::Fs)?;
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn time_ms(&self) -> u64 {
        self.start.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn log(&self, level: Level, msg: &str) {
        Self::log_line(level, msg);
    }
}

pub fn platform() -> std::sync::Arc<dyn Platform> {
    std::sync::Arc::new(StdPlatform::new())
}
