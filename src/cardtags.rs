//! Bare card tags → the fully-qualified `owner/slug` the renderer requires.
//!
//! Card references became namespaced (`{% mafold/html %}`); a BARE `{% html %}`
//! resolves to nothing and renders as a grey "Unsupported card: html" box that
//! also EATS the card's body — the mini-UI or the ask options are simply gone.
//! The preamble tells the model the qualified form, but a system prompt is not
//! an enforcement mechanism:
//!
//!   * a conversation that started before the migration has dozens of its own
//!     `{% html %}` messages in context, and a model copies its own transcript
//!     over any instruction — those sessions emit the old syntax forever;
//!   * a model writes `{% mafold/html %}` and closes with `{% /html %}`, which
//!     is worse than a dead card: the container never closes and swallows the
//!     rest of the message;
//!   * other harnesses (codex/kimi/opencode) and `/`-commands reach the same
//!     send path with prose that never saw the preamble at all.
//!
//! So the daemon canonicalises what the model wrote before it becomes a
//! message. We already fetch the authoritative card list to BUILD the preamble
//! (`agent::available_card_tags`), so the same list can validate the output:
//! a bare tag whose slug is a published card gets its owner spliced in, and
//! everything else is left byte-for-byte alone.
//!
//! Two deliberate limits:
//!   * only KNOWN slugs are touched — `{% partial file="x" %}` and any other
//!     non-card Markdoc stays literal, exactly as the client renders it;
//!   * tags inside markdown code (a fence or a backtick span) are skipped,
//!     mirroring `cards/split.ts`: writing `{% html %}` in backticks is how you
//!     TALK about a card, and this repo's own dev chat does it constantly.
//!     Rewriting those would corrupt the text instead of fixing a card.

use std::collections::BTreeMap;
use std::sync::RwLock;

/// The account whose cards are first-party. Mirrors `cards_api::OFFICIAL_SCOPE`.
const OFFICIAL: &str = "mafold";

/// slug → owner, from the card list this bot can embed. Process-wide because
/// "what cards exist" is a property of the account, not of a turn; it is
/// refreshed on every daemon (re)connection.
static REGISTRY: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());

/// Record the card ids (`owner/slug`) this bot may embed. A slug published in
/// BOTH the official scope and a family scope resolves to the official one:
/// a bare tag then means the same card for every reader, which is the whole
/// reason viewer-relative bare resolution was removed server-side. Ids without
/// an owner are ignored — there is nothing to qualify them to.
pub fn set_registry(ids: &[String]) {
    let mut map = BTreeMap::new();
    for id in ids {
        let Some((owner, slug)) = id.split_once('/') else { continue };
        if owner.is_empty() || slug.is_empty() {
            continue;
        }
        let entry = map.entry(slug.to_string()).or_insert_with(|| owner.to_string());
        if owner == OFFICIAL {
            *entry = owner.to_string();
        }
    }
    if let Ok(mut reg) = REGISTRY.write() {
        *reg = map;
    }
}

/// How much of a streamed buffer can be committed without cutting a tag in
/// half: everything up to the last `{%` that has no `%}` yet (and never a
/// trailing lone `{`, which is the same seam one byte earlier). The held-back
/// tail goes out with the next chunk — or at the end of the turn, where the
/// final flush commits regardless.
pub fn commit_boundary(buf: &str) -> usize {
    let mut end = match buf.rfind("{%") {
        Some(i) if !buf[i..].contains("%}") => i,
        _ => buf.len(),
    };
    if end > 0 && buf.as_bytes()[end - 1] == b'{' {
        end -= 1;
    }
    end
}

/// Splice the owner into every bare tag that names a published card. Idempotent
/// (`mafold/html` is already qualified and is skipped), so it can run on the
/// whole message as often as the message grows.
pub fn qualify(text: &str) -> String {
    if !text.contains("{%") {
        return text.to_string();
    }
    let Ok(reg) = REGISTRY.read() else { return text.to_string() };
    if reg.is_empty() {
        return text.to_string();
    }
    let code = code_ranges(text);
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 16);
    let mut copied = 0usize; // everything before this is already in `out`
    let mut i = 0usize;
    while let Some(rel) = text[i..].find("{%") {
        let open = i + rel;
        i = open + 2;
        if in_code(open, &code) {
            continue;
        }
        // `{%` ␣* `/`? ␣* name
        let mut p = open + 2;
        while p < b.len() && (b[p] as char).is_whitespace() {
            p += 1;
        }
        if p < b.len() && b[p] == b'/' {
            p += 1;
            while p < b.len() && (b[p] as char).is_whitespace() {
                p += 1;
            }
        }
        let start = p;
        if start >= b.len() || !b[start].is_ascii_lowercase() {
            continue;
        }
        while p < b.len() && (b[p].is_ascii_lowercase() || b[p].is_ascii_digit() || b[p] == b'-') {
            p += 1;
        }
        // Already `owner/slug` — the reference form we want. A `/` NOT followed
        // by a letter is the self-close marker of `{%ask/%}`, not a namespace;
        // the client's TAG_RE draws the line in exactly the same place.
        if p + 1 < b.len() && b[p] == b'/' && b[p + 1].is_ascii_alphabetic() {
            continue;
        }
        // Only rewrite what the client would actually parse as a tag: the run
        // has to close with `%}` before any later `{%` opens.
        let closes = match text[p..].find("%}") {
            Some(c) => text[p..].find("{%").is_none_or(|n| c < n),
            None => false,
        };
        if !closes {
            continue;
        }
        let Some(owner) = reg.get(&text[start..p]) else { continue };
        out.push_str(&text[copied..start]);
        out.push_str(owner);
        out.push('/');
        copied = start;
        i = p;
    }
    if copied == 0 {
        return text.to_string();
    }
    out.push_str(&text[copied..]);
    out
}

/// Byte ranges that sit inside markdown code — fenced blocks and inline
/// backtick spans. Same rule as the client splitter (`cards/split.ts`), so the
/// daemon and the renderer agree on which `{% … %}` are live tags.
fn code_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // Fenced blocks: a line whose start (≤3 spaces in) is ``` or ~~~ opens; the
    // next fence line of the same char and at least as long closes it.
    let mut open_at: Option<(usize, char, usize)> = None;
    let mut pos = 0usize;
    while pos <= s.len() {
        let end = s[pos..].find('\n').map_or(s.len(), |i| pos + i);
        let line = &s[pos..end];
        let fence = fence_mark(line);
        match (open_at, fence) {
            (None, Some(f)) => open_at = Some((pos, f.0, f.1)),
            (Some((from, ch, len)), Some(f)) if f.0 == ch && f.1 >= len => {
                ranges.push((from, end));
                open_at = None;
            }
            _ => {}
        }
        if end == s.len() {
            break;
        }
        pos = end + 1;
    }
    if let Some((from, _, _)) = open_at {
        ranges.push((from, s.len())); // unclosed fence → code to EOF
    }
    // Inline spans outside fenced blocks: a run of N backticks closes at the
    // next run of exactly N (CommonMark).
    let fenced = ranges.clone();
    let b = s.as_bytes();
    let mut open: Option<(usize, usize)> = None;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let run = i;
        while i < b.len() && b[i] == b'`' {
            i += 1;
        }
        let len = i - run;
        if in_code(run, &fenced) {
            open = None;
            continue;
        }
        match open {
            None => open = Some((run, len)),
            Some((from, want)) if want == len => {
                ranges.push((from, i));
                open = None;
            }
            _ => {}
        }
    }
    ranges
}

/// `(fence char, run length)` when the line opens/closes a fence.
fn fence_mark(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let n = rest.chars().take_while(|c| *c == ch).count();
    (n >= 3).then_some((ch, n))
}

fn in_code(i: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|(a, b)| i >= *a && i < *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() {
        set_registry(&[
            "mafold/html".into(),
            "mafold/ask".into(),
            "mafold/run".into(),
            "mafold/generating".into(),
            "linsky/generating".into(),
            "linsky/generating-swap".into(),
        ]);
    }

    #[test]
    fn qualifies_open_and_close() {
        registry();
        assert_eq!(
            qualify("see:\n{% html %}\n<b>hi</b>\n{% /html %}\n"),
            "see:\n{% mafold/html %}\n<b>hi</b>\n{% /mafold/html %}\n"
        );
    }

    #[test]
    fn qualifies_a_lone_bare_close() {
        // The half-migrated form is the worst one: the container never closes
        // and swallows the rest of the message.
        registry();
        assert_eq!(
            qualify("{% mafold/html %}x{% /html %}tail"),
            "{% mafold/html %}x{% /mafold/html %}tail"
        );
    }

    #[test]
    fn self_closing_and_attributes_survive_byte_for_byte() {
        registry();
        assert_eq!(
            qualify(r#"{% ask a="1" b='x/y' /%}"#),
            r#"{% mafold/ask a="1" b='x/y' /%}"#
        );
        assert_eq!(qualify("{%ask/%}"), "{%mafold/ask/%}");
        assert_eq!(qualify("{%  /  ask  %}"), "{%  /  mafold/ask  %}");
    }

    #[test]
    fn already_qualified_is_untouched_and_idempotent() {
        registry();
        let s = "{% mafold/html %}body{% /mafold/html %}";
        assert_eq!(qualify(s), s);
        assert_eq!(qualify(&qualify(s)), s);
        assert_eq!(qualify("{% opsdu/greatbet /%}"), "{% opsdu/greatbet /%}");
    }

    #[test]
    fn unknown_tags_stay_literal() {
        registry();
        for s in ["{% partial file=\"x\" %}", "{% nosuchcard /%}", "{% if $x %}"] {
            assert_eq!(qualify(s), s, "must not invent a namespace for {s}");
        }
    }

    #[test]
    fn official_scope_wins_over_a_family_shadow() {
        // `generating` exists in both scopes; a bare tag must mean the same card
        // for every reader, so it resolves to the first-party one.
        registry();
        assert_eq!(qualify("{% generating /%}"), "{% mafold/generating /%}");
        assert_eq!(
            qualify("{% generating-swap /%}"),
            "{% linsky/generating-swap /%}"
        );
    }

    #[test]
    fn code_is_left_alone() {
        registry();
        let fenced = "before\n```\n{% html %}\n```\nafter {% html %} here";
        assert_eq!(
            qualify(fenced),
            "before\n```\n{% html %}\n```\nafter {% mafold/html %} here"
        );
        assert_eq!(qualify("write `{% html %}` to embed"), "write `{% html %}` to embed");
        assert_eq!(
            qualify("~~~md\n{% ask %}\n~~~"),
            "~~~md\n{% ask %}\n~~~"
        );
    }

    #[test]
    fn a_fence_inside_a_card_body_does_not_hide_later_tags() {
        // An UNCLOSED fence swallows to EOF — that is the client's rule too, so
        // the daemon must not disagree about it. Balanced fences stay balanced.
        registry();
        let s = "```\ncode\n```\n{% html %}\n";
        assert_eq!(qualify(s), "```\ncode\n```\n{% mafold/html %}\n");
    }

    #[test]
    fn no_tag_no_copy() {
        registry();
        let s = "plain prose with 50% and a { brace";
        assert_eq!(qualify(s), s);
    }

    #[test]
    fn an_unterminated_run_is_not_a_tag() {
        registry();
        // No `%}` before the next `{%` — the client wouldn't parse it either.
        assert_eq!(qualify("{% html {% ask %}"), "{% html {% mafold/ask %}");
    }

    #[test]
    fn commit_boundary_holds_back_a_partial_tag() {
        assert_eq!(commit_boundary("hello {% ht"), 6);
        assert_eq!(commit_boundary("hello {"), 6);
        assert_eq!(commit_boundary("hello {% ask %} tail"), 20);
        assert_eq!(commit_boundary("no tags at all"), 14);
        assert_eq!(commit_boundary(""), 0);
        // A complete tag followed by an incomplete one: cut at the second.
        assert_eq!(commit_boundary("{% ask %} then {% ht"), 15);
    }

    #[test]
    fn multibyte_text_is_sliced_on_char_boundaries() {
        registry();
        assert_eq!(
            qualify("看这个 {% html %} 卡片"),
            "看这个 {% mafold/html %} 卡片"
        );
        assert_eq!(commit_boundary("中文一段 {% ht"), "中文一段 ".len());
    }
}
