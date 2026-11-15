//! Markdown → Telegram HTML.
//!
//! The model writes markdown, but Telegram renders only its own tags: unmarked
//! asterisks and pipes arrive literally. This is a small, deterministic subset
//! of markdown — enough for what the model actually writes, and no dependency.
//!
//! Anything not recognised stays literal text, escaped for HTML.

/// Telegram allows only these URL schemes in an `<a href>`.
const LINK_SCHEMES: [&str; 3] = ["http://", "https://", "tg://"];

/// Escape text so Telegram shows it as text, not as markup.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn esc_attr(s: &str) -> String {
    esc(s).replace('"', "&quot;")
}

fn safe_link(url: &str) -> Option<&str> {
    let url = url.trim();
    if LINK_SCHEMES
        .iter()
        .any(|s| url.len() > s.len() && url[..s.len()].eq_ignore_ascii_case(s))
    {
        Some(url)
    } else {
        None
    }
}

/// The first `marker` that is not the left half of a doubled marker, so
/// `*a **b** c*` still finds its closing star.
fn find_single(s: &str, marker: char) -> Option<usize> {
    let mut iter = s.char_indices();
    while let Some((i, c)) = iter.next() {
        if c == marker {
            let next = i + c.len_utf8();
            if s[next..].starts_with(marker) {
                iter.next(); // skip the pair
                continue;
            }
            return Some(i);
        }
    }
    None
}

/// Inline markdown on one line (or inside a heading, quote or list item).
fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(c) = rest.chars().next() {
        if c == '`'
            && let Some(end) = rest[1..].find('`')
        {
            out.push_str("<code>");
            out.push_str(&esc(&rest[1..1 + end]));
            out.push_str("</code>");
            rest = &rest[end + 2..];
            continue;
        }

        let paired = if rest.starts_with("**") {
            Some(("**", "b"))
        } else if rest.starts_with("__") {
            Some(("__", "b"))
        } else if rest.starts_with("~~") {
            Some(("~~", "s"))
        } else {
            None
        };
        if let Some((marker, tag)) = paired {
            let after = &rest[marker.len()..];
            if let Some(end) = after.find(marker)
                && end > 0
            {
                out.push_str(&format!("<{tag}>{}</{tag}>", inline(&after[..end])));
                rest = &after[end + marker.len()..];
                continue;
            }
        }

        if c == '*' && !rest.starts_with("**") {
            let after = &rest[1..];
            let opening_ok = after.chars().next().is_some_and(|n| !n.is_whitespace());
            if opening_ok
                && let Some(end) = find_single(after, '*')
                && !after[..end].ends_with(char::is_whitespace)
            {
                out.push_str(&format!("<i>{}</i>", inline(&after[..end])));
                rest = &after[end + 1..];
                continue;
            }
        }

        if c == '['
            && let Some(mid) = rest.find("](")
            && let Some(end) = rest[mid + 2..].find(')')
        {
            let label = &rest[1..mid];
            let url = &rest[mid + 2..mid + 2 + end];
            match safe_link(url) {
                Some(url) => out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    esc_attr(url),
                    inline(label)
                )),
                // A link Telegram would reject: keep the text, drop the target.
                None => out.push_str(&inline(label)),
            }
            rest = &rest[mid + 2 + end + 1..];
            continue;
        }

        let width = c.len_utf8();
        out.push_str(&esc(&rest[..width]));
        rest = &rest[width..];
    }
    out
}

/// Telegram has no tables. Rows stay text (they wrap), but the `|---|`
/// divider is pure noise, so it is dropped.
fn is_divider(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|')
        && t.ends_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

pub fn to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut fence = false;
    let mut quote = false;

    for line in text.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if quote {
                out.push_str("</blockquote>\n");
                quote = false;
            }
            if fence {
                out.push_str("</code></pre>\n");
            } else {
                out.push_str("<pre><code>\n");
            }
            fence = !fence;
            continue;
        }

        if fence {
            out.push_str(&esc(line));
            out.push('\n');
            continue;
        }

        if is_divider(line) {
            continue;
        }

        let plain = trimmed.trim_start_matches('>').trim_start();
        if plain.len() != trimmed.len() {
            if !quote {
                out.push_str("<blockquote>");
                quote = true;
            }
            out.push_str(&inline(plain));
            out.push('\n');
            continue;
        }
        if quote {
            out.push_str("</blockquote>\n");
            quote = false;
        }

        let indent = &line[..line.len() - trimmed.len()];
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str(indent);
            out.push_str("• ");
            out.push_str(&inline(rest));
            out.push('\n');
            continue;
        }

        if let Some(rest) = heading(trimmed) {
            out.push_str(indent);
            out.push_str("<b>");
            // A bold title would nest <b> in <b>; the outer tag already does it.
            out.push_str(&inline(rest).replace("<b>", "").replace("</b>", ""));
            out.push_str("</b>\n");
            continue;
        }

        if matches!(trimmed.trim_end(), "---" | "***" | "___") {
            out.push_str("─────\n");
            continue;
        }

        out.push_str(indent);
        out.push_str(&inline(trimmed));
        out.push('\n');
    }

    if fence {
        out.push_str("</code></pre>\n");
    }
    if quote {
        out.push_str("</blockquote>\n");
    }
    let end = out.trim_end().len();
    out.truncate(end);
    out
}

/// `### Title` → "Title"; `#tag` and a bare `#` are not headings.
fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    if rest.trim().is_empty() {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_italic_strike_and_code() {
        assert_eq!(to_html("**жирный**"), "<b>жирный</b>");
        assert_eq!(to_html("__жирный__"), "<b>жирный</b>");
        assert_eq!(to_html("*курсив*"), "<i>курсив</i>");
        assert_eq!(to_html("~~зачёркнуто~~"), "<s>зачёркнуто</s>");
        assert_eq!(to_html("`a < b`"), "<code>a &lt; b</code>");
        assert_eq!(to_html("*a **b** c*"), "<i>a <b>b</b> c</i>");
    }

    #[test]
    fn plain_text_is_escaped() {
        assert_eq!(to_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        // An unclosed or space-padded marker is text, not markup.
        assert_eq!(to_html("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(to_html("**не закрыт"), "**не закрыт");
        assert_eq!(to_html("snake_case_name"), "snake_case_name");
    }

    #[test]
    fn links_keep_only_safe_schemes() {
        assert_eq!(
            to_html("[сайт](https://example.com/a?b=1&c=2)"),
            "<a href=\"https://example.com/a?b=1&amp;c=2\">сайт</a>"
        );
        assert_eq!(to_html("[файл](file:///etc/passwd)"), "файл");
        assert_eq!(
            to_html("[tg](tg://resolve?domain=hermit)"),
            "<a href=\"tg://resolve?domain=hermit\">tg</a>"
        );
    }

    #[test]
    fn headings_bullets_and_quotes() {
        assert_eq!(to_html("## Итог **теста**"), "<b>Итог теста</b>");
        assert_eq!(to_html("- один\n  - два"), "• один\n  • два");
        assert_eq!(
            to_html("> цитата\n> ещё"),
            "<blockquote>цитата\nещё\n</blockquote>"
        );
        assert_eq!(to_html("#не заголовок"), "#не заголовок");
        assert_eq!(to_html("---"), "─────");
    }

    #[test]
    fn fences_and_tables() {
        assert_eq!(
            to_html("```\nfn a() { b < c }\n```"),
            "<pre><code>\nfn a() { b &lt; c }\n</code></pre>"
        );
        // No language class: Telegram would need it on <code>, not <pre>.
        assert_eq!(
            to_html("```python\nx = 1\n```"),
            "<pre><code>\nx = 1\n</code></pre>"
        );
        assert_eq!(
            to_html("| a | b |\n|---|---|\n| 1 | **2** |"),
            "| a | b |\n| 1 | <b>2</b> |"
        );
    }
}
