# ESP32 Rust build notes (machine-specific fixes, 2026-10)

1. Python >= 3.10 is REQUIRED for the esp-idf-sys managed environment:
   the CMake check (`idf_tools.py check-python-dependencies`) relies on
   `importlib.metadata` name normalization that only landed in Python 3.10.
   On py3.9 every dashed package (esp-idf-monitor, esp-idf-panic-decoder...)
   reports "Package was not found". Build with python3.12 first in PATH:
       export PATH="/opt/homebrew/opt/python@3.12/bin:$PATH"
2. ESP toolchain esp 16.1.0: `panic_immediate_abort` is a real panic
   strategy now -> set `panic = "immediate-abort"` in [profile.release]
   + `cargo-features = ["panic-immediate-abort"]` in Cargo.toml, and drop
   the old `build-std-features = ["panic_immediate_abort"]`.
3. Custom partition tables: esp-idf-sys resolves partitions.csv relative to
   its generated CMake project dir (out/), NOT the crate root. Copy it with
   the official glob mechanism in .cargo/config.toml:
       [env]
       ESP_IDF_GLOB_BASE = { value = "..", relative = true }
       ESP_IDF_GLOB_PARTITIONS = "partitions.csv"
4. The managed ESP-IDF is v5.2.3 (pinned by esp-idf-sys 0.36), independent
   of the C project's ~/esp-idf v5.5.
5. Flash: convert the ELF yourself (espflash handles it too):
       python -m esptool --chip esp32 elf2image --flash_mode dio \
           --flash_size 4MB --flash_freq 40m -o app.bin \
           target/xtensa-esp32-espidf/release/hermes-lite-esp32
       esptool write_flash 0x1000 bootloader.bin 0x8000 partition-table.bin \
           0x190000 app.bin 0x10000 skills.img
