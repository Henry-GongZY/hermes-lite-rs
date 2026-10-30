//! Skills: agentskills.io-compatible SKILL.md files with YAML-ish
//! frontmatter. Only name/description are injected into the system prompt;
//! the LLM pulls the full body via the built-in `read_skill` tool.

use alloc::format;
use alloc::string::{String, ToString};
use crate::platform::{HlResult, Platform};

fn fm_field(text: &str, field: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(field) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        return Some(rest.trim().trim_end_matches('\r').to_string());
    }
    None
}

/// Directory listings differ by platform: POSIX yields the entry itself
/// ("my-skill" for a dir), SPIFFS-style VFS yields the full remainder
/// ("my-skill/SKILL.md"). Try the entry verbatim, then the two
/// conventional layouts; a zero-byte read (a POSIX directory) is a miss.
fn try_read(platform: &dyn Platform, dir: &str, name: &str, cap: usize) -> Option<String> {
    for path in [
        format!("{dir}/{name}"),
        format!("{dir}/{name}/SKILL.md"),
        format!("{dir}/{name}.md"),
    ] {
        if let Ok(bytes) = platform.fs_read(&path, cap) {
            if !bytes.is_empty() {
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    None
}

/// Append the "- name: description" index to `out`. Returns skill count.
pub fn build_index(platform: &dyn Platform, dir: &str, out: &mut String, cap: usize) -> usize {
    let Ok(names) = platform.fs_list(dir) else {
        return 0;
    };
    let mut count = 0;
    for name in names {
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let Some(text) = try_read(platform, dir, &name, cap) else {
            continue;
        };
        // fallback display name: strip a trailing "/SKILL.md" or ".md"
        let fallback = name
            .strip_suffix("/SKILL.md")
            .unwrap_or_else(|| name.strip_suffix(".md").unwrap_or(&name));
        let nm = fm_field(&text, "name").unwrap_or_else(|| fallback.to_string());
        let ds = fm_field(&text, "description").unwrap_or_default();
        out.push_str("\n- ");
        out.push_str(&nm);
        out.push_str(": ");
        out.push_str(&ds);
        count += 1;
    }
    count
}

/// Load the full markdown body of a skill (read_skill tool backend).
pub fn load(platform: &dyn Platform, dir: &str, name: &str, cap: usize) -> HlResult<String> {
    // keep names flat: no traversal
    if name.is_empty() || name.starts_with('.') || name.contains('/') {
        return Err(crate::platform::HlError::Fs);
    }
    for path in [format!("{dir}/{name}/SKILL.md"), format!("{dir}/{name}.md")] {
        if let Ok(bytes) = platform.fs_read(&path, cap) {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    Err(crate::platform::HlError::Fs)
}

#[cfg(test)]
mod tests {
    use super::fm_field;

    #[test]
    fn frontmatter_fields() {
        let text = "---\nname: demo\ndescription: demo skill\n---\nbody";
        assert_eq!(fm_field(text, "name").as_deref(), Some("demo"));
        assert_eq!(fm_field(text, "description").as_deref(), Some("demo skill"));
        assert_eq!(fm_field(text, "nope"), None);
    }
}
