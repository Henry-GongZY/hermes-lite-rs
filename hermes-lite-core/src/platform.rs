//! Platform abstraction — the Rust equivalent of C's `hl_platform_t`.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Info = 1,
    Debug = 2,
}

#[derive(Debug)]
pub enum HlError {
    /// Transport-level HTTP failure (connect/TLS/OOM in platform).
    Transport,
    /// Response exceeded the configured byte budget.
    Overflow,
    /// Platform does not implement streaming; caller must fall back.
    Unsupported,
    /// Non-2xx status; carries the status code and error body snippet.
    Status(u16, String),
    /// Malformed JSON in a response.
    BadJson,
    /// Request did not fit its buffer.
    TooLarge,
    /// Filesystem error.
    Fs,
}

pub type HlResult<T> = Result<T, HlError>;

/// Platform contract. Mirror of the C `hl_platform_t`.
///
/// `max_response_bytes` bounds are enforced by the *platform* — the agent
/// passes its configured budget so ports know how much they may buffer.
pub trait Platform {
    /// Buffered HTTPS POST (Content-Type: application/json).
    /// On success the body is appended to `resp` (must not exceed
    /// `resp_cap`) and the HTTP status is returned.
    fn http_post(
        &self,
        url: &str,
        auth_bearer: &str,
        body: &str,
        resp: &mut Vec<u8>,
        resp_cap: usize,
    ) -> HlResult<u16>;

    /// Optional streaming POST: every complete SSE `data:` payload (without
    /// the `data:` prefix, including the literal `[DONE]`) goes to `on_line`;
    /// returning `true` from the callback aborts the transfer.
    /// Non-2xx bodies go to `errbuf` (capped) and no lines are delivered.
    /// Default: not implemented — [`Agent::chat_stream`] then falls back to
    /// buffered mode.
    fn supports_streaming(&self) -> bool {
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn http_post_stream(
        &self,
        _url: &str,
        _auth_bearer: &str,
        _body: &str,
        _on_line: &mut dyn FnMut(&str) -> bool,
        _errbuf: &mut String,
    ) -> HlResult<u16> {
        Err(HlError::Unsupported)
    }

    /// Optional buffered HTTPS GET (used by OTA skill sync).
    /// Default: not implemented — callers must handle the error.
    fn http_get(
        &self,
        _url: &str,
        _auth_bearer: &str,
        _resp: &mut Vec<u8>,
        _resp_cap: usize,
    ) -> HlResult<u16> {
        Err(HlError::Unsupported)
    }

    /// Read up to `cap` bytes of `path` (fs::error when absent).
    fn fs_read(&self, path: &str, cap: usize) -> HlResult<Vec<u8>>;
    /// Truncate + write.
    fn fs_write(&self, path: &str, data: &[u8]) -> HlResult<()>;
    fn fs_append(&self, path: &str, data: &[u8]) -> HlResult<()>;
    /// Directory entry names (no paths; entries starting with '.' may be skipped).
    fn fs_list(&self, dir: &str) -> HlResult<Vec<String>>;

    fn time_ms(&self) -> u64;

    /// Sleep for backoff between HTTP retries. Default: no-op (instant retry).
    fn sleep_ms(&self, _ms: u64) {}

    fn log(&self, level: Level, msg: &str);
}
