use std::path::Path;

use ropey::Rope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageEntry {
    pub id: &'static str,
    pub label: &'static str,
}

pub const SUPPORTED_LANGUAGES: &[LanguageEntry] = &[
    LanguageEntry {
        id: "plaintext",
        label: "Plain Text",
    },
    LanguageEntry {
        id: "rust",
        label: "Rust",
    },
    LanguageEntry {
        id: "toml",
        label: "TOML",
    },
    LanguageEntry {
        id: "json",
        label: "JSON",
    },
    LanguageEntry {
        id: "typescript",
        label: "TypeScript",
    },
    LanguageEntry {
        id: "tsx",
        label: "TSX",
    },
    LanguageEntry {
        id: "javascript",
        label: "JavaScript",
    },
    LanguageEntry {
        id: "python",
        label: "Python",
    },
    LanguageEntry {
        id: "markdown",
        label: "Markdown",
    },
    LanguageEntry {
        id: "yaml",
        label: "YAML",
    },
    LanguageEntry {
        id: "html",
        label: "HTML",
    },
    LanguageEntry {
        id: "css",
        label: "CSS",
    },
    LanguageEntry {
        id: "sql",
        label: "SQL",
    },
    LanguageEntry {
        id: "bash",
        label: "Shell",
    },
    LanguageEntry {
        id: "c",
        label: "C",
    },
    LanguageEntry {
        id: "cpp",
        label: "C++",
    },
    LanguageEntry {
        id: "go",
        label: "Go",
    },
    LanguageEntry {
        id: "swift",
        label: "Swift",
    },
    LanguageEntry {
        id: "xml",
        label: "XML",
    },
    LanguageEntry {
        id: "make",
        label: "Makefile",
    },
];

/// Declarative language identification from path extension, filename, or bounded first-line shebang.
/// Never reads beyond the first 128 bytes of a `Rope`, and creates zero JS engines or LSP servers.
pub fn detect_language(path: Option<&Path>, rope: Option<&Rope>) -> LanguageEntry {
    if let Some(path) = path {
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            let lower_name = file_name.to_ascii_lowercase();
            match lower_name.as_str() {
                "cargo.toml" | "cargo.lock" | "rust-toolchain.toml" => {
                    return entry_by_id("toml");
                }
                "dockerfile" => return entry_by_id("bash"),
                "makefile" | "gnumakefile" => return entry_by_id("make"),
                ".bashrc" | ".zshrc" | ".profile" => return entry_by_id("bash"),
                _ => {}
            }
        }

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_ascii_lowercase();
            let matched = match lower.as_str() {
                "rs" => Some("rust"),
                "toml" => Some("toml"),
                "json" | "jsonc" | "jsonl" => Some("json"),
                "ts" | "mts" | "cts" => Some("typescript"),
                "tsx" => Some("tsx"),
                "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
                "py" | "pyi" => Some("python"),
                "md" | "markdown" => Some("markdown"),
                "yml" | "yaml" => Some("yaml"),
                "html" | "htm" => Some("html"),
                "css" | "scss" => Some("css"),
                "sql" => Some("sql"),
                "sh" | "bash" | "zsh" => Some("bash"),
                "c" | "h" => Some("c"),
                "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Some("cpp"),
                "go" => Some("go"),
                "swift" => Some("swift"),
                "xml" | "svg" | "plist" => Some("xml"),
                _ => None,
            };
            if let Some(id) = matched {
                return entry_by_id(id);
            }
        }
    }

    if let Some(rope) = rope
        && rope.len() > 0
    {
        let cap = rope.len().min(128);
        let mut prefix = String::with_capacity(cap);
        for chunk in rope.chunks() {
            let remaining = cap.saturating_sub(prefix.len());
            if remaining == 0 {
                break;
            }
            if chunk.len() <= remaining {
                prefix.push_str(chunk);
            } else {
                let mut end = remaining;
                while end > 0 && !chunk.is_char_boundary(end) {
                    end -= 1;
                }
                prefix.push_str(&chunk[..end]);
                break;
            }
        }
        if let Some(first_line) = prefix.lines().next()
            && let Some(shebang) = first_line.strip_prefix("#!")
        {
            if shebang.contains("python") {
                return entry_by_id("python");
            }
            if shebang.contains("node") || shebang.contains("deno") || shebang.contains("bun") {
                return entry_by_id("javascript");
            }
            if shebang.contains("sh") || shebang.contains("bash") || shebang.contains("zsh") {
                return entry_by_id("bash");
            }
        }
    }

    entry_by_id("plaintext")
}

pub fn entry_by_id(id: &str) -> LanguageEntry {
    SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .find(|entry| entry.id.eq_ignore_ascii_case(id))
        .unwrap_or(SUPPORTED_LANGUAGES[0])
}

pub fn next_language(current_id: &str) -> LanguageEntry {
    let idx = SUPPORTED_LANGUAGES
        .iter()
        .position(|entry| entry.id.eq_ignore_ascii_case(current_id))
        .unwrap_or(0);
    SUPPORTED_LANGUAGES[(idx + 1) % SUPPORTED_LANGUAGES.len()]
}

/// Resolves effective syntax language for a document: explicit `DocMeta.language_override`
/// takes precedence when recognized; `None` (Auto Detect) falls back to `detect_language`.
pub fn resolve_language(
    path: Option<&Path>,
    rope: Option<&Rope>,
    language_override: Option<&str>,
) -> LanguageEntry {
    if let Some(override_id) = language_override {
        let trimmed = override_id.trim();
        if !trimmed.is_empty()
            && let Some(found) = SUPPORTED_LANGUAGES
                .iter()
                .copied()
                .find(|entry| entry.id.eq_ignore_ascii_case(trimmed))
        {
            return found;
        }
    }
    detect_language(path, rope)
}

/// Returns the single-line comment token for languages with unambiguous line-comment syntax.
///
/// Per contract, block-comment-only or comment-less formats (`plaintext`, `json`, `markdown`,
/// `html`, `css`, `xml`) return `None` so `ToggleComment` is explicitly disabled rather than
/// inserting invalid syntax.
pub fn comment_prefix_for_language(language_id: &str) -> Option<&'static str> {
    if language_id.eq_ignore_ascii_case("rust")
        || language_id.eq_ignore_ascii_case("typescript")
        || language_id.eq_ignore_ascii_case("tsx")
        || language_id.eq_ignore_ascii_case("javascript")
        || language_id.eq_ignore_ascii_case("c")
        || language_id.eq_ignore_ascii_case("cpp")
        || language_id.eq_ignore_ascii_case("go")
        || language_id.eq_ignore_ascii_case("swift")
    {
        Some("//")
    } else if language_id.eq_ignore_ascii_case("python")
        || language_id.eq_ignore_ascii_case("toml")
        || language_id.eq_ignore_ascii_case("yaml")
        || language_id.eq_ignore_ascii_case("bash")
        || language_id.eq_ignore_ascii_case("make")
    {
        Some("#")
    } else if language_id.eq_ignore_ascii_case("sql") {
        Some("--")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_prefix_known_languages_and_unsupported_formats() {
        assert_eq!(comment_prefix_for_language("rust"), Some("//"));
        assert_eq!(comment_prefix_for_language("typescript"), Some("//"));
        assert_eq!(comment_prefix_for_language("tsx"), Some("//"));
        assert_eq!(comment_prefix_for_language("javascript"), Some("//"));
        assert_eq!(comment_prefix_for_language("c"), Some("//"));
        assert_eq!(comment_prefix_for_language("cpp"), Some("//"));
        assert_eq!(comment_prefix_for_language("go"), Some("//"));
        assert_eq!(comment_prefix_for_language("swift"), Some("//"));

        assert_eq!(comment_prefix_for_language("python"), Some("#"));
        assert_eq!(comment_prefix_for_language("toml"), Some("#"));
        assert_eq!(comment_prefix_for_language("yaml"), Some("#"));
        assert_eq!(comment_prefix_for_language("bash"), Some("#"));
        assert_eq!(comment_prefix_for_language("make"), Some("#"));

        assert_eq!(comment_prefix_for_language("sql"), Some("--"));

        for unsupported in ["plaintext", "json", "markdown", "html", "css", "xml"] {
            assert_eq!(
                comment_prefix_for_language(unsupported),
                None,
                "{unsupported} must disable line comments"
            );
        }
    }

    #[test]
    fn resolve_language_honors_override_and_auto_detect_fallback() {
        let path = Path::new("src/main.rs");
        let rope = Rope::from_str("fn main() {}\n");
        assert_eq!(resolve_language(Some(path), Some(&rope), None).id, "rust");
        assert_eq!(
            resolve_language(Some(path), Some(&rope), Some("python")).id,
            "python"
        );
        // Unknown legacy override falls back cleanly to auto-detect instead of panicking.
        assert_eq!(
            resolve_language(Some(path), Some(&rope), Some("nonexistent-lang")).id,
            "rust"
        );
    }
}
