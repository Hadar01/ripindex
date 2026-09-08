//! Result snippets. The index holds no content, so the file is re-read here.
//!
//! The chosen line is the one containing the most *distinct* query terms
//! (ties: earliest). Matching uses the index tokenizer so a query for
//! `response` highlights the `HttpResponse` atom it is part of. Files that
//! have changed or vanished since indexing simply yield no snippet.

use std::ops::Range;
use std::path::Path;

use crate::tokenizer::tokenize;

/// One line of a file with the byte ranges to highlight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// 1-based.
    pub line_no: usize,
    /// The line, whitespace-trimmed and cut to `max_width` chars around the
    /// first highlight (`…` marks a cut).
    pub text: String,
    /// Non-overlapping, ascending byte ranges in `text`.
    pub highlights: Vec<Range<usize>>,
}

/// Re-read `path` and pick the best line for `terms` (lowercased atoms or
/// parts). `None` if the file can't be read or no line matches.
pub fn snippet_for(path: &Path, terms: &[&str], max_width: usize) -> Option<Snippet> {
    if terms.is_empty() {
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            log::debug!("snippet: {}: {e}", path.display());
            return None;
        }
    };

    let mut best: Option<(usize, usize, &str, Vec<Range<usize>>)> = None; // (distinct, line_no, line, spans)
    let mut seen = vec![false; terms.len()];
    for (i, line) in text.lines().enumerate() {
        seen.fill(false);
        let mut spans: Vec<Range<usize>> = Vec::new();
        for tok in tokenize(line) {
            if let Some(k) = terms.iter().position(|t| *t == tok.term.as_ref()) {
                seen[k] = true;
                // Parts follow their atom and share its span; several query
                // terms can also hit one atom. Either way, one highlight.
                if spans.last() != Some(&tok.span) {
                    spans.push(tok.span);
                }
            }
        }
        let distinct = seen.iter().filter(|&&s| s).count();
        if distinct > 0 && best.as_ref().is_none_or(|b| distinct > b.0) {
            best = Some((distinct, i + 1, line, spans));
            if distinct == terms.len() {
                break;
            }
        }
    }
    let (_, line_no, line, spans) = best?;
    Some(make_snippet(line_no, line, spans, max_width))
}

/// Trim, then cut to a `max_width`-char window positioned so the first
/// highlight is about a quarter of the way in.
fn make_snippet(line_no: usize, line: &str, spans: Vec<Range<usize>>, max_width: usize) -> Snippet {
    let lead = line.len() - line.trim_start().len();
    let line = line.trim();
    let mut spans: Vec<Range<usize>> = spans
        .into_iter()
        .filter(|r| r.start >= lead && r.end - lead <= line.len())
        .map(|r| r.start - lead..r.end - lead)
        .collect();

    let n_chars = line.chars().count();
    if n_chars <= max_width || max_width == 0 {
        return Snippet { line_no, text: line.to_string(), highlights: spans };
    }

    // bounds[k] = byte offset of the k-th char; bounds[n_chars] = len.
    let bounds: Vec<usize> = line.char_indices().map(|(i, _)| i).chain(std::iter::once(line.len())).collect();
    let first_byte = spans.first().map_or(0, |r| r.start);
    let first_char = bounds.partition_point(|&b| b < first_byte);
    let start_char = first_char.saturating_sub(max_width / 4).min(n_chars - max_width);
    let end_char = start_char + max_width;
    let (start, end) = (bounds[start_char], bounds[end_char]);

    let prefix = if start_char > 0 { "…" } else { "" };
    let suffix = if end_char < n_chars { "…" } else { "" };
    let text = format!("{prefix}{}{suffix}", &line[start..end]);
    spans.retain(|r| r.start >= start && r.end <= end);
    for r in &mut spans {
        *r = r.start - start + prefix.len()..r.end - start + prefix.len();
    }
    Snippet { line_no, text, highlights: spans }
}

/// Render for the terminal. `color = true` wraps highlights in ANSI bold-red;
/// otherwise they are bracketed `[like this]`.
pub fn render(snippet: &Snippet, color: bool) -> String {
    let (open, close) = if color { ("\x1b[1;31m", "\x1b[0m") } else { ("[", "]") };
    let mut out = String::with_capacity(snippet.text.len() + 16 * snippet.highlights.len());
    let mut pos = 0;
    for r in &snippet.highlights {
        if r.start < pos || r.end > snippet.text.len() || !snippet.text.is_char_boundary(r.start) {
            continue; // defensive: never panic while printing results
        }
        out.push_str(&snippet.text[pos..r.start]);
        out.push_str(open);
        out.push_str(&snippet.text[r.clone()]);
        out.push_str(close);
        pos = r.end;
    }
    out.push_str(&snippet.text[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn plain(path: &Path, terms: &[&str]) -> Option<(usize, String)> {
        snippet_for(path, terms, 160).map(|s| (s.line_no, render(&s, false)))
    }

    #[test]
    fn picks_line_with_most_distinct_terms() {
        let dir = tempfile::tempdir().unwrap();
        let p = file(&dir, "a.txt", "alpha\nbeta beta beta\n  alpha beta  \nalpha\n");
        assert_eq!(plain(&p, &["alpha", "beta"]), Some((3, "[alpha] [beta]".into())));
        assert_eq!(plain(&p, &["alpha"]), Some((1, "[alpha]".into()))); // earliest on ties
        assert_eq!(plain(&p, &["beta"]), Some((2, "[beta] [beta] [beta]".into())));
        assert_eq!(plain(&p, &["gamma"]), None);
        assert_eq!(plain(&p, &[]), None);
    }

    #[test]
    fn highlights_whole_atom_for_identifier_parts() {
        let dir = tempfile::tempdir().unwrap();
        let p = file(&dir, "a.rs", "fn parse_http_response() -> HttpResponse {}\n");
        assert_eq!(
            plain(&p, &["http"]),
            Some((1, "fn [parse_http_response]() -> [HttpResponse] {}".into()))
        );
        // Two query terms hitting the same atom produce one highlight.
        assert_eq!(
            plain(&p, &["parse", "response"]),
            Some((1, "fn [parse_http_response]() -> [HttpResponse] {}".into()))
        );
    }

    #[test]
    fn crlf_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let p = file(&dir, "a.txt", "first\r\nle café est ouvert\r\n");
        assert_eq!(plain(&p, &["café"]), Some((2, "le [café] est ouvert".into())));
    }

    #[test]
    fn missing_or_binary_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(plain(&dir.path().join("nope"), &["x"]), None);
        let p = dir.path().join("bad.bin");
        fs::write(&p, b"\xFF\xFE x").unwrap();
        assert_eq!(plain(&p, &["x"]), None);
    }

    #[test]
    fn long_lines_are_windowed_around_the_first_hit() {
        let dir = tempfile::tempdir().unwrap();
        let filler = "x ".repeat(100); // 200 chars
        let line = format!("{filler}needle here {filler}");
        let p = file(&dir, "long.txt", &line);
        let s = snippet_for(&p, &["needle", "here"], 40).unwrap();
        assert!(s.text.starts_with('…') && s.text.ends_with('…'));
        assert_eq!(s.text.chars().count(), 42);
        assert_eq!(render(&s, false).matches("[needle] [here]").count(), 1);
        for r in &s.highlights {
            assert!(s.text.is_char_boundary(r.start) && s.text.is_char_boundary(r.end));
        }

        // Hit at the very start: no leading ellipsis.
        let p2 = file(&dir, "start.txt", &format!("needle {filler}"));
        let s2 = snippet_for(&p2, &["needle"], 20).unwrap();
        assert!(s2.text.starts_with("needle") && s2.text.ends_with('…'));
        assert_eq!(render(&s2, false).find("[needle]"), Some(0));

        // Hit at the very end: window slides back, no trailing ellipsis.
        let p3 = file(&dir, "end.txt", &format!("{filler}needle"));
        let s3 = snippet_for(&p3, &["needle"], 20).unwrap();
        assert!(s3.text.starts_with('…') && s3.text.ends_with("needle"));
        assert!(render(&s3, false).ends_with("[needle]"));
    }

    #[test]
    fn window_respects_multibyte_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{} needle {}", "é".repeat(50), "日".repeat(50));
        let p = file(&dir, "mb.txt", &line);
        let s = snippet_for(&p, &["needle"], 30).unwrap();
        assert_eq!(s.text.chars().count(), 32);
        assert_eq!(render(&s, false).matches("[needle]").count(), 1);
    }

    #[test]
    fn render_color() {
        let s = Snippet { line_no: 1, text: "ab cd".into(), highlights: vec![0..2, 3..5] };
        assert_eq!(render(&s, true), "\x1b[1;31mab\x1b[0m \x1b[1;31mcd\x1b[0m");
        assert_eq!(render(&s, false), "[ab] [cd]");
        let empty = Snippet { line_no: 1, text: "plain".into(), highlights: vec![] };
        assert_eq!(render(&empty, true), "plain");
    }
}
