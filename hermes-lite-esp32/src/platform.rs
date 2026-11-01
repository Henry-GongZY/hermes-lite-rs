//! ESP32 platform backend — the Rust twin of the C `platform_esp32.c`,
//! using the raw esp_http_client C API (same event-driven design) so the
//! behavior matches the C port exactly, independent of wrapper APIs.

extern crate alloc;

use core::ffi::c_void;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use esp_idf_sys as sys;

use hermes_lite_core::{HlError, HlResult, Level, Platform};

// The C event handler is a plain fn pointer; the Rust routing closure lives
// in a thread-local for the duration of one HTTP call (the REPL is
// single-threaded, which is all this demo needs).
thread_local! {
    // raw pointer: the closure outlives the HTTP call that uses it, and the
    // REPL is single-threaded; no ownership is transferred.
    static ROUTE: core::cell::RefCell<Option<*mut dyn FnMut(u16, &[u8]) -> bool>> =
        const { core::cell::RefCell::new(None) };
}

unsafe extern "C" fn evh(evt: *mut sys::esp_http_client_event_t) -> sys::esp_err_t {
    let e = &*evt;
    if e.event_id == sys::esp_http_client_event_id_t_HTTP_EVENT_ON_DATA && e.data_len > 0 {
        let status = sys::esp_http_client_get_status_code(e.client) as u16;
        let chunk = core::slice::from_raw_parts(e.data as *const u8, e.data_len as usize);
        // The agent never aborts a transfer, so the handler always reports
        // success and lets the route decide what to keep.
        let _ = ROUTE.with(|r| {
            r.borrow_mut()
                .as_mut()
                // SAFETY: the route pointer is valid for the whole
                // run_http() call and the REPL is single-threaded.
                .is_some_and(|f| unsafe { (&mut **f)(status, chunk) })
        });
    }
    sys::ESP_OK
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

pub struct Esp32Platform {
    /// PEM CA certificate for HTTPS; none = TLS verification fails on
    /// https:// URLs (use http:// for lab runs).
    ca: std::sync::Mutex<Option<Vec<u8>>>,
}

impl Esp32Platform {
    pub fn new() -> Self {
        Esp32Platform {
            ca: std::sync::Mutex::new(None),
        }
    }

    pub fn set_ca(&self, pem: &[u8]) {
        *self.ca.lock().unwrap() = Some(pem.to_vec());
    }

    /// One POST through esp_http_client. `route(status, chunk)` receives
    /// every body chunk; returning true requests an abort (unused by the
    /// agent; kept for API parity). With `sse=true` and a 200 status,
    /// chunks are reassembled into complete SSE `data:` payloads first.
    fn run_http(
        &self,
        url: &str,
        auth_bearer: &str,
        body: &str,
        sse: bool,
        route: &mut dyn FnMut(u16, &[u8]) -> bool,
    ) -> HlResult<u16> {
        let headers = [
            cstr("Content-Type"),
            cstr("application/json"),
            cstr("Authorization"),
            cstr(&format!("Bearer {auth_bearer}")),
        ];
        let ca = self.ca.lock().unwrap().clone();
        let cfg = sys::esp_http_client_config_t {
            url: cstr(url).as_ptr() as *const _,
            method: sys::esp_http_client_method_t_HTTP_METHOD_POST,
            event_handler: Some(evh),
            timeout_ms: 120_000,
            buffer_size: 4096,
            buffer_size_tx: 2048,
            crt_bundle_attach: if ca.is_some() {
                Some(sys::esp_crt_bundle_attach)
            } else {
                None
            },
            ..Default::default()
        };

        let h = unsafe { sys::esp_http_client_init(&cfg) };
        if h.is_null() {
            return Err(HlError::Transport);
        }
        // SAFETY: `route` is borrowed for the whole run_http() call and is
        // cleared from ROUTE before that borrow ends; single-threaded REPL.
        ROUTE.with(|r| {
            let route_static: &'static mut dyn FnMut(u16, &[u8]) -> bool =
                unsafe { core::mem::transmute(route) };
            *r.borrow_mut() = Some(route_static as *mut dyn FnMut(u16, &[u8]) -> bool);
        });
        unsafe {
            sys::esp_http_client_set_header(
                h,
                headers[0].as_ptr() as *const _,
                headers[1].as_ptr() as *const _,
            );
            sys::esp_http_client_set_header(
                h,
                headers[2].as_ptr() as *const _,
                headers[3].as_ptr() as *const _,
            );
            sys::esp_http_client_set_post_field(h, body.as_ptr() as *const _, body.len() as i32);
        }
        let err = unsafe { sys::esp_http_client_perform(h) };
        let status = unsafe { sys::esp_http_client_get_status_code(h) } as u16;
        unsafe { sys::esp_http_client_cleanup(h) };
        ROUTE.with(|r| *r.borrow_mut() = None);

        if err != sys::ESP_OK && status == 0 {
            log::error!("http transport: {err}");
            return Err(HlError::Transport);
        }
        Ok(status)
    }

    /// SSE line reassembly shared by the streaming route.
    fn feed_sse(carry: &mut Vec<u8>, chunk: &[u8], on_line: &mut dyn FnMut(&str) -> bool) -> bool {
        let mut start = 0usize;
        for i in 0..chunk.len() {
            if chunk[i] == b'\n' {
                carry.extend_from_slice(&chunk[start..i]);
                start = i + 1;
                if carry.last() == Some(&b'\r') {
                    carry.pop();
                }
                let line = String::from_utf8_lossy(carry).into_owned();
                carry.clear();
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.strip_prefix(' ').unwrap_or(payload);
                    if !payload.is_empty() && on_line(payload) {
                        return true;
                    }
                }
            }
        }
        carry.extend_from_slice(&chunk[start..]);
        false
    }
}

impl Default for Esp32Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for Esp32Platform {
    fn http_post(
        &self,
        url: &str,
        auth_bearer: &str,
        body: &str,
        resp: &mut Vec<u8>,
        resp_cap: usize,
    ) -> HlResult<u16> {
        let mut overflow = false;
        let mut collected: Vec<u8> = Vec::new();
        let st = self.run_http(
            url,
            auth_bearer,
            body,
            false,
            &mut |status, chunk| {
                if status != 200 {
                    return false;
                }
                if collected.len() + chunk.len() > resp_cap {
                    overflow = true;
                    return true;
                }
                collected.extend_from_slice(chunk);
                false
            },
        )?;
        if overflow {
            return Err(HlError::Overflow);
        }
        resp.clear();
        resp.extend_from_slice(&collected);
        Ok(st)
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
        let mut err: Vec<u8> = Vec::new();
        let mut carry: Vec<u8> = Vec::new();
        let st = self.run_http(
            url,
            auth_bearer,
            body,
            true,
            &mut |status, chunk| {
                if status != 200 {
                    // error body -> errbuf (capped)
                    if err.len() < 4096 {
                        let room = 4096 - err.len();
                        let n = chunk.len().min(room);
                        err.extend_from_slice(&chunk[..n]);
                    }
                    return false;
                }
                Self::feed_sse(&mut carry, chunk, on_line)
            },
        )?;
        if st != 200 {
            *errbuf = String::from_utf8_lossy(&err).into_owned();
        }
        Ok(st)
    }

    fn fs_read(&self, path: &str, cap: usize) -> HlResult<Vec<u8>> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|_| HlError::Fs)?;
        let mut b = Vec::new();
        f.take(cap as u64)
            .read_to_end(&mut b)
            .map_err(|_| HlError::Fs)?;
        Ok(b)
    }

    fn fs_write(&self, path: &str, data: &[u8]) -> HlResult<()> {
        std::fs::write(path, data).map_err(|_| HlError::Fs)
    }

    fn fs_append(&self, path: &str, data: &[u8]) -> HlResult<()> {
        use std::io::Write;
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
        unsafe { sys::esp_timer_get_time() as u64 / 1000 }
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(core::time::Duration::from_millis(ms));
    }

    fn log(&self, level: Level, msg: &str) {
        match level {
            Level::Error => log::error!("[hl] {msg}"),
            Level::Info => log::info!("[hl] {msg}"),
            Level::Debug => log::debug!("[hl] {msg}"),
        }
    }
}
