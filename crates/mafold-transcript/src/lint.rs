//! Source-scanning gate: every card tag a producer EMITS must be fully
//! qualified (`owner/slug`).
//!
//! The `owner/slug` migration was done in batches and some emitters were simply
//! never in a batch — so `/usage` kept rendering as raw markup in the bubble
//! long after bare resolution was removed. Nothing failed: a bare tag is not an
//! error, it is just text that never becomes a card. Only a scan catches that.
//!
//! This lives in the library rather than in one crate's `#[cfg(test)]` because
//! the rule is a property of the card vocabulary, and the vocabulary is shared:
//! `mafold-cli` scans its emitters, this crate scans its renderer, and anything
//! else that learns to emit cards gets the same gate for free instead of a
//! third copy of the scanner.

/// Card tags emitted by a chunk of Rust source, in order.
///
/// `//`-comments are prose ABOUT tags (`/// the {% bash %} card`), not
/// emissions. Rust doc/line comments are line-based, so dropping from `//` to
/// end-of-line is exact for them; `format!` placeholders like `{{%` are
/// normalized to `{%` first so the emitted shape is what gets checked. A line
/// carrying `LINT-IGNORE` is skipped — the escape hatch for a test fixture that
/// is deliberately bare.
pub fn emitted_tags(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        // Checked on the RAW line, before comments are stripped: the escape
        // hatch has to survive its own stripping.
        if line.contains("LINT-IGNORE") {
            continue;
        }
        let code = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        let code = code.replace("{{%", "{%");
        let mut rest = code.as_str();
        while let Some(i) = rest.find("{%") {
            rest = &rest[i + 2..];
            let name: String = rest
                .trim_start()
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '/')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

/// The emitted tags that are NOT namespaced — the ones that would render as
/// literal markup instead of a card. `owner/cardname` is the card preamble's
/// placeholder for "any card" and passes, like any other qualified tag.
pub fn bare_tags(src: &str) -> Vec<String> {
    emitted_tags(src)
        .into_iter()
        .filter(|t| !t.contains('/'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard the guard: a scanner that silently matches nothing would "pass"
    /// forever. Comments must stay invisible, emissions must not.
    #[test]
    fn the_lint_itself_detects_a_bare_tag() {
        assert_eq!(emitted_tags("let s = \"{% stats a=1 %}\";"), vec!["stats"]); // LINT-IGNORE
        assert_eq!(emitted_tags("format!(\"{{% stats %}}\")"), vec!["stats"]); // LINT-IGNORE
        assert_eq!(emitted_tags("/// the {% bash %} card"), Vec::<String>::new()); // LINT-IGNORE
        assert_eq!(
            emitted_tags("let s = \"{% mafold/stats %}…{% /mafold/stats %}\";"), // LINT-IGNORE
            vec!["mafold/stats", "mafold/stats"]
        );
        // The escape hatch must not be a blanket off-switch for a whole file.
        assert_eq!(emitted_tags("\"{% stats %}\" // LINT-IGNORE"), Vec::<String>::new());
        assert_eq!(emitted_tags("\"{% stats %}\""), vec!["stats"]); // LINT-IGNORE
        assert_eq!(bare_tags("\"{% mafold/tool %}{% tool %}\""), vec!["tool"]); // LINT-IGNORE
    }

    /// The renderer is the single biggest card emitter in the product — every
    /// tag it writes reaches every client of every platform.
    #[test]
    fn this_crate_emits_only_qualified_tags() {
        const SOURCES: &[(&str, &str)] = &[
            ("render.rs", include_str!("render.rs")),
            ("transcript.rs", include_str!("transcript.rs")),
        ];
        let mut bare = Vec::new();
        for (name, src) in SOURCES {
            for tag in bare_tags(src) {
                bare.push(format!("{name}: {{% {tag} %}}")); // LINT-IGNORE
            }
        }
        assert!(
            bare.is_empty(),
            "bare card tags emitted (a card reference is `owner/slug`, \
             see .docs/card-namespace-v1.md §4):\n  {}",
            bare.join("\n  ")
        );
    }
}
