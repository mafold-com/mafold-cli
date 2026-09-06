//! The reader's prose: a message body with every card island and every code
//! span cut out — the text an `@`-mention grammar must run on.
//!
//! A message body is not one kind of text. The client splits it
//! (`cards/split.ts`, its RN and Swift twins) into card islands — `{% owner/slug
//! … %}`, leaf or container, rendered by CardHost — and text runs; inside a text
//! run, backtick code renders as code. Only what is left is prose, and only prose
//! gets a tappable mention label.
//!
//! Every scanner that asks "does this text @-mention X" — the api's unread badge
//! (`mentions_user`), the api's bot trigger (`extract_mentions`), the daemon's
//! reply gate (`mentions_me`) — has to ask it of THIS text, or it answers about
//! text the reader never sees as a mention. Measured in the Mafold DEV forum on
//! 2026-09-06: of 49 `@linsky` hits that lit a channel's `@` badge, 41 sat in a
//! card body (an agent's HTML mock-up showing a profile row "@linsky", tool
//! output, a diff of a memory file, an ask card's option label) or inside
//! backticks. Not one of them rendered a mention label; the badge lied 41 times.
//!
//! The rules mirror `splitCards` byte for byte:
//!   * a tag is `{%` ws* `/`? ws* NAME … `%}` — NAME is
//!     `[a-zA-Z][\w:-]*(/[a-zA-Z][\w:-]*)?`, `…` runs to the FIRST `%}`;
//!   * a tag inside markdown code (a fence or a backtick span) is literal text;
//!   * `/` right before `%}` makes a leaf; otherwise the body runs to the matching
//!     `{% /NAME %}`, else to the first close tag nothing inside the body opened
//!     (the author's misspelled close), else to the end of the message;
//!   * a stray close tag is text;
//!   * fences inside a consumed card body don't decide code for what follows.
//!
//! Each cut becomes one newline, so the text on either side keeps its boundary:
//! `{% x /%}@ops` mentions in the bubble (the `@` opens a fresh text run) and it
//! mentions here.
//!
//! Not the same question as [`crate::render::strip_cards`]: that one feeds a
//! model the prose of its own last turn, where a code block IS the answer and
//! stays. This one asks what a person sees as a mention label.

use std::borrow::Cow;

/// `text` with every card and code span replaced by a newline. Borrows when
/// there is nothing to cut — the common case, a human line — so the unread walk
/// that runs this over every message of every conversation allocates nothing.
pub fn visible_prose(text: &str) -> Cow<'_, str> {
    if !text.contains("{%") && !text.contains('`') && !text.contains("~~~") {
        return Cow::Borrowed(text);
    }
    let mut ranges = code_ranges(text);
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut settled = 0usize; // prose before this offset already had its code cut
    let mut i = 0usize;
    while let Some(rel) = text[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(text, start) else {
            i = start + 2;
            continue;
        };
        // Inside a fence / backtick span, or a close nothing opened: literal.
        if tag.is_close || in_code(start, &ranges) {
            i = tag.end;
            continue;
        }
        let end = if tag.self_close {
            tag.end
        } else {
            find_close(text, tag.end, tag.name)
                .or_else(|| orphan_close(text, tag.end, &ranges).map(|(_, end)| end))
                .unwrap_or(text.len())
        };
        push_code_cuts(&mut cuts, &ranges, settled, start);
        cuts.push((start, end));
        // A fence inside this card must not put the rest of the message "in
        // code" — recompute for the suffix (only needed if the span could have
        // opened one at all).
        if text[start..end].contains(['`', '~']) {
            ranges = if end < text.len() {
                code_ranges(&text[end..]).into_iter().map(|(a, b)| (a + end, b + end)).collect()
            } else {
                Vec::new()
            };
        }
        settled = end;
        i = end;
    }
    push_code_cuts(&mut cuts, &ranges, settled, text.len());
    if cuts.is_empty() {
        return Cow::Borrowed(text);
    }
    cuts.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    for (a, b) in cuts {
        if a < pos {
            continue; // can't happen (cuts never overlap), belt and braces
        }
        out.push_str(&text[pos..a]);
        out.push('\n');
        pos = b;
    }
    out.push_str(&text[pos..]);
    Cow::Owned(out)
}

/// The code ranges that fall inside the prose run `from..to`, clipped to it.
fn push_code_cuts(cuts: &mut Vec<(usize, usize)>, ranges: &[(usize, usize)], from: usize, to: usize) {
    for &(a, b) in ranges {
        if b > from && a < to {
            cuts.push((a.max(from), b.min(to)));
        }
    }
}

struct Tag<'a> {
    name: &'a str,
    is_close: bool,
    self_close: bool,
    /// Byte offset just past the tag's `%}`.
    end: usize,
}

/// A byte that can sit inside a tag name: `[\w:-]`.
fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b':' || c == b'-'
}

fn skip_ws(text: &str, mut p: usize) -> usize {
    while let Some(c) = text[p..].chars().next() {
        if !c.is_whitespace() {
            break;
        }
        p += c.len_utf8();
    }
    p
}

/// The tag opening at `at` (which must be a `{%`), or `None` where the client's
/// `TAG_RE` would not match either: no name, or no `%}` anywhere after.
fn parse_tag(text: &str, at: usize) -> Option<Tag<'_>> {
    let b = text.as_bytes();
    let mut p = skip_ws(text, at + 2);
    let is_close = b.get(p) == Some(&b'/');
    if is_close {
        p = skip_ws(text, p + 1);
    }
    let name_start = p;
    if !b.get(p).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    p += 1;
    while p < b.len() && is_name_byte(b[p]) {
        p += 1;
    }
    // `owner/slug` — a `/` NOT followed by a letter is the self-close of
    // `{%ask/%}`, not a namespace.
    if b.get(p) == Some(&b'/') && b.get(p + 1).is_some_and(u8::is_ascii_alphabetic) {
        p += 2;
        while p < b.len() && is_name_byte(b[p]) {
            p += 1;
        }
    }
    let name = &text[name_start..p];
    let close = p + text[p..].find("%}")?;
    let self_close = text[p..close].trim_end().ends_with('/');
    Some(Tag { name, is_close, self_close, end: close + 2 })
}

/// End offset of the first `{% /NAME %}` at or after `from` — the same fixed-tag
/// search `splitCards` runs, code or not.
fn find_close(text: &str, from: usize, name: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = from;
    while let Some(rel) = text[i..].find("{%") {
        let start = i + rel;
        i = start + 2;
        let mut p = skip_ws(text, start + 2);
        if b.get(p) != Some(&b'/') {
            continue;
        }
        p = skip_ws(text, p + 1);
        if !text[p..].starts_with(name) {
            continue;
        }
        p = skip_ws(text, p + name.len());
        if text[p..].starts_with("%}") {
            return Some(p + 2);
        }
    }
    None
}

/// `(start, end)` of the first close tag after `from` that nothing inside the
/// body opened — the close the author meant for the unclosed container but
/// misspelled. Depth-tracked so a nested container's own close isn't taken.
fn orphan_close(text: &str, from: usize, ranges: &[(usize, usize)]) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut i = from;
    while let Some(rel) = text[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(text, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        if in_code(start, ranges) {
            continue;
        }
        if tag.is_close {
            if depth == 0 {
                return Some((start, tag.end));
            }
            depth -= 1;
        } else if !tag.self_close {
            depth += 1;
        }
    }
    None
}

/// Byte ranges that sit inside markdown code — fenced blocks and inline
/// backtick spans. The client splitter's rule (`codeRanges` in `cards/split.ts`),
/// so every Rust scanner agrees with the renderer on which `{% … %}` are live.
pub fn code_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // Fenced blocks: a line whose start (≤3 blanks in) is ``` or ~~~ opens; the
    // next fence line of the same char and at least as long closes it.
    let mut open_at: Option<(usize, u8, usize)> = None;
    let mut pos = 0usize;
    loop {
        let end = s[pos..].find('\n').map_or(s.len(), |i| pos + i);
        match (open_at, fence_mark(&s[pos..end])) {
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
    // next run of exactly N (CommonMark); other lengths are content.
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
        if in_code(run, &fenced) {
            open = None;
            continue;
        }
        match open {
            None => open = Some((run, i - run)),
            Some((from, want)) if want == i - run => {
                ranges.push((from, i));
                open = None;
            }
            _ => {}
        }
    }
    ranges
}

/// `(fence byte, run length)` when the line opens or closes a fence.
fn fence_mark(line: &str) -> Option<(u8, usize)> {
    let indent = line.bytes().take_while(|c| *c == b' ' || *c == b'\t').count();
    if indent > 3 {
        return None;
    }
    let rest = line[indent..].as_bytes();
    let ch = *rest.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = rest.iter().take_while(|c| **c == ch).count();
    (n >= 3).then_some((ch, n))
}

/// Is byte offset `i` inside one of `ranges`?
pub fn in_code(i: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|(a, b)| i >= *a && i < *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(s: &str) -> String {
        visible_prose(s).into_owned()
    }

    #[test]
    fn plain_text_is_borrowed_untouched() {
        for s in ["hey @ops look", "50% off, {not a tag}", "a ~ b", "帮我看看@ops:claude"] {
            assert!(matches!(visible_prose(s), Cow::Borrowed(_)), "{s}");
        }
        // Contains the delimiters but nothing to cut: still borrowed.
        assert!(matches!(visible_prose("stray {% /mafold/html %} close"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_leaf_card_is_cut_and_leaves_a_boundary() {
        assert_eq!(prose("see {% mafold/kline symbol=\"@ops\" /%}@ops"), "see \n@ops");
        assert_eq!(prose("{%ask/%}x"), "\nx");
        assert_eq!(prose("{% mafold/result duration=\"1s\" /%}"), "\n");
    }

    #[test]
    fn a_container_body_is_cut() {
        let s = "看这个稿子\n{% mafold/html %}\n<b>linsky</b><i>@linsky</i>\n{% /mafold/html %}\n完";
        assert_eq!(prose(s), "看这个稿子\n\n\n完");
        // Bare (unqualified) tags are cards too — the client renders an
        // "unsupported card" that still eats the body.
        assert_eq!(prose("{% html %}<i>@ops</i>{% /html %} done"), "\n done");
        // Whitespace and CJK space around the close tag's name.
        assert_eq!(prose("{% mafold/todo %}@ops{%\u{3000}/ mafold/todo\t%}!"), "\n!");
    }

    #[test]
    fn an_unclosed_container_runs_to_the_end() {
        // Mid-stream: the lid isn't down yet, nothing after the open is prose.
        assert_eq!(prose("{% mafold/trace summary=\"s\" steps=\"1\" %}\n我先看看 @ops 稍等"), "\n");
    }

    #[test]
    fn a_misspelled_close_ends_the_container_there() {
        let s = "{% mafold/html %}<b>@ops</b>{% /mafell/html %} tail @ops";
        assert_eq!(prose(s), "\n tail @ops");
        // Depth-tracked: a nested container's own close is not the orphan.
        let nested = "{% mafold/run %}{% mafold/tool %}@a{% /mafold/tool %}@b{% /oops %} @c";
        assert_eq!(prose(nested), "\n @c");
    }

    #[test]
    fn code_is_cut_like_a_card() {
        assert_eq!(prose("`@ops` and\n```\n@ops\n```\nbut @ops"), "\n and\n\n\nbut @ops");
        assert_eq!(prose("~~~md\n@ops\n~~~"), "\n");
        // A run of two backticks doesn't close a run of one.
        assert_eq!(prose("`a `` @ops` x"), "\n x");
        // An unclosed fence is code to the end.
        assert_eq!(prose("```\n@ops"), "\n");
    }

    #[test]
    fn a_tag_inside_code_is_literal() {
        // The fence is cut anyway; the point is the tag inside doesn't open a
        // container that would swallow "after".
        assert_eq!(prose("```\n{% mafold/html %}\n```\nafter @ops"), "\n\nafter @ops");
        assert_eq!(prose("write `{% mafold/html %}` to embed, @ops"), "write \n to embed, @ops");
    }

    #[test]
    fn a_fence_inside_a_card_does_not_poison_the_rest() {
        let s = "{% mafold/bash %}\n```\nls\n{% /mafold/bash %} then @ops";
        assert_eq!(prose(s), "\n then @ops");
    }

    #[test]
    fn not_a_tag_stays_text() {
        // No name, or no `%}` anywhere after: the client leaves it as text.
        assert_eq!(prose("看这个 {% 符号 @ops"), "看这个 {% 符号 @ops");
        assert_eq!(prose("{% mafold/html @ops"), "{% mafold/html @ops");
    }

    /// The shapes that lit `@` badges in the Mafold DEV forum with no visible
    /// mention — and the one shape that IS a mention: the answer outside the
    /// folded trace.
    #[test]
    fn real_agent_replies() {
        let reply = "{% mafold/trace summary=\"Ran 3 shell commands\" steps=\"4\" %}\n\
我来查。@linsky 稍等\n\
{% mafold/run summary=\"Ran 1 shell command\" %}\n\
{% mafold/tool name=\"Edit\" detail=\"memory.md\" added=3 removed=1 %}\n\
+**例外(@linsky,2026-08-14 明确发火):** …\n\
{% /mafold/tool %}\n\
{% /mafold/run %}\n\
{% /mafold/trace %}\n\
查完了,@linsky 是卡片体的锅。\n\
{% mafold/ask %}\nq|下一步|0|要我怎么接?\no|都别动|等 @opsdu 报完一起看\n{% /mafold/ask %}\n\
{% mafold/result duration=\"19s\" tokens=\"12k\" /%}";
        let seen = prose(reply);
        assert_eq!(seen.trim(), "查完了,@linsky 是卡片体的锅。");
        assert!(!seen.contains("稍等") && !seen.contains("例外") && !seen.contains("@opsdu"), "{seen:?}");
    }
}
