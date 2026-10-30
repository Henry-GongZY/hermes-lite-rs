# hermes-lite-rs

The Rust twin of [hermes-lite](https://github.com/Henry-GongZY/hermes-lite) — same feature set, same
platform contract, idiomatic Rust: the core is **`no_std` + `alloc`**, the
platform abstraction is a trait, and backends are separate crates.

```
hermes-lite-core   no_std+alloc agent runtime (agent loop, SSE streaming,
                   tools, agentskills.io skills, JSONL history + compaction)
hermes-lite-std    std backend: ureq + rustls HTTP, std::fs  →  CLI demo
hermes-lite-esp32  ESP-IDF backend: raw esp_http_client C API + SPIFFS  →
                   serial REPL, WiFi provisioning, GPIO/ADC/sysinfo tools
```

## Parity with the C version

| C | Rust |
|---|---|
| `hl_platform_t` (7 fn pointers) | `Platform` trait (same 7 primitives) |
| buffered + SSE `http_post_stream` | trait methods + `supports_streaming()` |
| `hl_tool_fn` + registry | `Tool { name, description, parameters_json, f }` |
| arena JSON parser | `serde_json` (`alloc` feature) |
| `chat` / `chat_stream` | `Agent::chat` / `Agent::chat_stream` |
| skills `SKILL.md` index + `read_skill` | same, progressive disclosure |
| JSONL history, trim + compaction | same algorithm |
| test_agent.c (14 cases) | `tests/agent_tests.rs` (same 14 cases) |

Differences worth knowing:
- Request assembly uses `serde_json` allocation instead of fixed buffers;
  the response budget (`max_response_bytes`) is still enforced by platforms
  and the streaming accumulator, so memory stays bounded.
- ESP32 credentials live in a SPIFFS file (`/spiffs/wifi.txt`) instead of
  NVS — same persistence semantics, less code.
- The ESP32 HTTP backend calls the raw `esp_http_client` C API (event
  handler + status routing), mirroring `platform_esp32.c` line for line.

## Build & run (host)

```bash
cargo build --release
HL_API_KEY=sk-... ./target/release/hermes-lite        # exec/time tools
HL_SKILLS_DIR=skills HL_API_KEY=sk-... ./target/release/hermes-lite
cargo test -p hermes-lite-core                        # 15 tests, mock platform
```

## Build for ESP32

One-time setup (installs the `esp` rustup toolchain + ldproxy):

```bash
cargo install espup ldproxy
espup install
. ~/export-esp.sh
```

Per build:

```bash
cd hermes-lite-esp32
cargo +esp build --release
espflash flash --monitor target/xtensa-esp32-espidf/release/hermes-lite-esp32
# skills partition (SPIFFS at 0x10000), reuse the C project's generator:
python $IDF_PATH/components/spiffs/spiffsgen.py --obj-name-len 64 1572864 \
    skills skills.img
python -m esptool --chip esp32 -p PORT write_flash 0x10000 skills.img
```

Serial REPL is identical to the C firmware: `wifi <ssid> <pass>`, `skills`,
`reset`, `free`, anything else → the agent (streaming over serial).

TLS: call `platform.set_ca(pem)` with your endpoint's CA at boot; otherwise
use `http://` URLs in lab setups.

## Third targets (Zephyr, embassy, bare metal)

`hermes-lite-core` compiles under `no_std` — an embassy/Zephyr port is just
another `Platform` impl over `embassy-net` + `embedded-storage` (or Zephyr
sockets via bindings), no core changes.

## License

MIT
