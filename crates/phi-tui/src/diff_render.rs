use std::{path::Path, sync::LazyLock};

use diffy::{Line as DiffLine, Patch};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};
use unicode_width::UnicodeWidthStr;

use super::wrap_styled_line;

const TEXT: Color = Color::Rgb(190, 190, 185);
const MUTED: Color = Color::Rgb(110, 110, 108);
const ADDED: Color = Color::Rgb(120, 210, 120);
const REMOVED: Color = Color::Rgb(235, 105, 95);
const ADDED_BG: Color = Color::Rgb(29, 55, 39);
const REMOVED_BG: Color = Color::Rgb(72, 37, 33);
const DIFF_LEFT_PADDING: usize = 4;
const DIFF_RIGHT_PADDING: usize = 2;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> =
    LazyLock::new(|| ThemeSet::load_defaults().themes["base16-ocean.dark"].clone());

struct Change<'a> {
    action: &'a str,
    path: String,
    diff: &'a str,
}

pub(super) fn push(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    let Some(changes) = parse_changes(content) else {
        push_fallback(lines, content, width);
        return;
    };
    let totals = changes
        .iter()
        .map(|change| counts(change.diff))
        .fold((0, 0), |total, count| {
            (total.0 + count.0, total.1 + count.1)
        });

    lines.push(Line::raw(" ".repeat(width)));
    if changes.len() == 1 {
        push_header(lines, "• ", changes[0].action, &changes[0].path, totals);
        render_diff(lines, changes[0].diff, &changes[0].path, width);
    } else {
        push_header(
            lines,
            "• ",
            "Edited",
            &format!("{} files", changes.len()),
            totals,
        );
        for change in changes {
            let count = counts(change.diff);
            push_file_header(lines, change.action, &change.path, count);
            render_diff(lines, change.diff, &change.path, width);
        }
    }
    lines.push(Line::raw(" ".repeat(width)));
}

fn push_file_header(
    lines: &mut Vec<Line<'static>>,
    action: &str,
    path: &str,
    count: (usize, usize),
) {
    let mut spans = vec![Span::styled("  └ ", Style::default().fg(MUTED))];
    if action != "Edited" {
        spans.push(Span::styled(
            format!("{action} "),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(path.to_owned(), Style::default().fg(TEXT)));
    push_counts(&mut spans, count);
    lines.push(Line::from(spans));
}

fn parse_changes(content: &str) -> Option<Vec<Change<'_>>> {
    let (header, details) = content.split_once("\n\n")?;
    if let Some((action, path)) = parse_action(strip_duration(header)) {
        return Some(vec![Change {
            action,
            path,
            diff: details.trim_matches(['\r', '\n']),
        }]);
    }

    let mut changes = Vec::new();
    let mut current: Option<(&str, String, usize)> = None;
    let mut offset = 0;
    for line in details.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if let Some((action, path)) = parse_action(text) {
            if let Some((action, path, start)) = current.take() {
                changes.push(Change {
                    action,
                    path,
                    diff: details[start..offset].trim_matches(['\r', '\n']),
                });
            }
            current = Some((action, path, offset + line.len()));
        }
        offset += line.len();
    }
    if let Some((action, path, start)) = current {
        changes.push(Change {
            action,
            path,
            diff: details[start..].trim_matches(['\r', '\n']),
        });
    }
    (!changes.is_empty()).then_some(changes)
}

fn strip_duration(header: &str) -> &str {
    header.rsplit_once(" · ").map_or(header, |(label, _)| label)
}

fn parse_action(line: &str) -> Option<(&str, String)> {
    let (source, action) = [
        ("Created `", "Added"),
        ("Updated `", "Edited"),
        ("Deleted `", "Deleted"),
        ("Moved `", "Moved"),
    ]
    .into_iter()
    .find(|(prefix, _)| line.starts_with(prefix))?;
    let path = line[source.len()..].replace('`', "");
    Some((action, path))
}

fn counts(diff: &str) -> (usize, usize) {
    Patch::from_str(diff).map_or((0, 0), |patch| {
        patch
            .hunks()
            .iter()
            .flat_map(|hunk| hunk.lines())
            .fold((0, 0), |count, line| match line {
                DiffLine::Insert(_) => (count.0 + 1, count.1),
                DiffLine::Delete(_) => (count.0, count.1 + 1),
                DiffLine::Context(_) => count,
            })
    })
}

fn push_header(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    action: &str,
    path: &str,
    (added, removed): (usize, usize),
) {
    let mut spans = vec![
        Span::styled(prefix.to_owned(), Style::default().fg(Color::Green)),
        Span::styled(
            format!("{action} "),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(path.to_owned(), Style::default().fg(TEXT)),
    ];
    push_counts(&mut spans, (added, removed));
    lines.push(Line::from(spans));
}

fn push_counts(spans: &mut Vec<Span<'static>>, (added, removed): (usize, usize)) {
    if added == 0 && removed == 0 {
        return;
    }
    spans.extend([
        Span::styled(" (", Style::default().fg(MUTED)),
        Span::styled(format!("+{added}"), Style::default().fg(ADDED)),
        Span::styled(" ", Style::default()),
        Span::styled(format!("−{removed}"), Style::default().fg(REMOVED)),
        Span::styled(")", Style::default().fg(MUTED)),
    ]);
}

fn render_diff(lines: &mut Vec<Line<'static>>, diff: &str, path: &str, width: usize) {
    let Ok(patch) = Patch::from_str(diff) else {
        push_fallback(lines, diff, width);
        return;
    };
    let max_line = patch
        .hunks()
        .iter()
        .flat_map(|hunk| [hunk.old_range().end(), hunk.new_range().end()])
        .max()
        .unwrap_or(1)
        .saturating_sub(1)
        .max(1);
    let number_width = max_line.to_string().len();
    let gutter_width = number_width + 3;
    let content_width = width
        .saturating_sub(DIFF_LEFT_PADDING + gutter_width + DIFF_RIGHT_PADDING)
        .max(1);
    let syntax = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| SYNTAXES.find_syntax_by_extension(extension))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());

    for (hunk_index, hunk) in patch.hunks().iter().enumerate() {
        if hunk_index > 0 {
            lines.push(Line::styled(
                format!("{}{:>number_width$} ⋮", " ".repeat(DIFF_LEFT_PADDING), ""),
                Style::default().fg(MUTED),
            ));
        }
        let mut old_highlighter = HighlightLines::new(syntax, &THEME);
        let mut new_highlighter = HighlightLines::new(syntax, &THEME);
        let mut old_line = hunk.old_range().start();
        let mut new_line = hunk.new_range().start();
        for diff_line in hunk.lines() {
            let (number, sign, background, dimmed, highlighted) = match diff_line {
                DiffLine::Context(text) => {
                    let _ = old_highlighter.highlight_line(text, &SYNTAXES);
                    let spans = highlighted_spans(&mut new_highlighter, text);
                    let number = new_line;
                    old_line += 1;
                    new_line += 1;
                    (number, ' ', None, false, spans)
                }
                DiffLine::Delete(text) => {
                    let spans = highlighted_spans(&mut old_highlighter, text);
                    let number = old_line;
                    old_line += 1;
                    (number, '−', Some(REMOVED_BG), true, spans)
                }
                DiffLine::Insert(text) => {
                    let spans = highlighted_spans(&mut new_highlighter, text);
                    let number = new_line;
                    new_line += 1;
                    (number, '+', Some(ADDED_BG), false, spans)
                }
            };
            let logical = Line::from(highlighted);
            for (index, wrapped) in wrap_styled_line(&logical, content_width)
                .into_iter()
                .enumerate()
            {
                push_diff_line(
                    lines,
                    (index == 0).then_some(number),
                    if index == 0 { sign } else { ' ' },
                    wrapped,
                    number_width,
                    width,
                    background,
                    dimmed,
                );
            }
        }
    }
}

fn highlighted_spans(highlighter: &mut HighlightLines<'_>, text: &str) -> Vec<Span<'static>> {
    highlighter
        .highlight_line(text, &SYNTAXES)
        .unwrap_or_else(|_| Vec::new())
        .into_iter()
        .filter_map(|(style, text)| {
            let text = text.trim_end_matches(['\r', '\n']);
            (!text.is_empty()).then(|| {
                Span::styled(
                    text.to_owned(),
                    Style::default().fg(Color::Rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    )),
                )
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn push_diff_line(
    lines: &mut Vec<Line<'static>>,
    number: Option<usize>,
    sign: char,
    content: Line<'static>,
    number_width: usize,
    width: usize,
    background: Option<Color>,
    dimmed: bool,
) {
    let row_style = background.map_or_else(Style::default, |color| Style::default().bg(color));
    let sign_color = match sign {
        '+' => ADDED,
        '−' => REMOVED,
        _ => MUTED,
    };
    let number = number.map_or_else(
        || " ".repeat(number_width),
        |number| format!("{number:>number_width$}"),
    );
    let mut spans = vec![
        Span::styled(" ".repeat(DIFF_LEFT_PADDING), row_style),
        Span::styled(number, Style::default().fg(MUTED).patch(row_style)),
        Span::styled(" ", row_style),
        Span::styled(
            sign.to_string(),
            Style::default().fg(sign_color).patch(row_style),
        ),
        Span::styled(" ", row_style),
    ];
    spans.extend(content.spans.into_iter().map(|span| {
        let mut style = span.style.patch(row_style);
        if dimmed {
            style = style.add_modifier(Modifier::DIM);
        }
        Span::styled(span.content.into_owned(), style)
    }));
    let used = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        row_style,
    ));
    lines.push(Line::from(spans).style(row_style));
}

fn push_fallback(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    for line in content.lines() {
        let text = line.chars().take(width).collect::<String>();
        lines.push(Line::styled(text, Style::default().fg(TEXT)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gutters_counts_and_change_backgrounds() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Updated `src/main.rs` · 1s\n\n--- src/main.rs\n+++ src/main.rs\n@@ -8,2 +8,2 @@\n let x = 1;\n-old();\n+new();",
            60,
        );

        assert_eq!(lines[1].to_string(), "• Edited src/main.rs (+1 −1)");
        let removed = lines
            .iter()
            .find(|line| line.to_string().contains("old();"))
            .unwrap();
        let added = lines
            .iter()
            .find(|line| line.to_string().contains("new();"))
            .unwrap();
        assert_eq!(removed.style.bg, Some(REMOVED_BG));
        assert_eq!(added.style.bg, Some(ADDED_BG));
        assert!(removed.to_string().starts_with("    9 − "));
        assert!(added.to_string().starts_with("    9 + "));
        assert_eq!(UnicodeWidthStr::width(removed.to_string().as_str()), 60);
    }

    #[test]
    fn preserves_trailing_blank_context_lines() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Updated `/tmp/main.scm` · 1s\n\n--- /tmp/main.scm\n+++ /tmp/main.scm\n@@ -1,2 +1,2 @@\n-old\n+new\n ",
            40,
        );

        assert!(lines.iter().any(|line| line.style.bg == Some(REMOVED_BG)));
        assert!(lines.iter().any(|line| line.style.bg == Some(ADDED_BG)));
    }

    #[test]
    fn separates_hunks_with_an_ellipsis() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Updated `a.rs` · 1s\n\n--- a.rs\n+++ a.rs\n@@ -1 +1 @@\n-a\n+b\n@@ -10 +10 @@\n-c\n+d",
            40,
        );
        assert!(lines.iter().any(|line| line.to_string().contains('⋮')));
    }

    #[test]
    fn wraps_changed_rows_with_the_same_full_width_background() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Created `a.txt` · 1s\n\n--- /dev/null\n+++ a.txt\n@@ -0,0 +1 @@\n+abcdefghijk",
            16,
        );
        let changed = lines
            .iter()
            .filter(|line| line.style.bg == Some(ADDED_BG))
            .collect::<Vec<_>>();
        assert_eq!(changed.len(), 2);
        assert!(changed[1].to_string().starts_with("        "));
        assert!(
            changed
                .iter()
                .all(|line| UnicodeWidthStr::width(line.to_string().as_str()) == 16)
        );
    }

    #[test]
    fn multi_file_subheaders_label_structural_changes() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Patched 2 files · 1s\n\nUpdated `src/a.rs`\n--- src/a.rs\n+++ src/a.rs\n@@ -1 +1 @@\n-a\n+b\n\nCreated `tests/b.rs`\n--- /dev/null\n+++ tests/b.rs\n@@ -0,0 +1 @@\n+b",
            60,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string() == "  └ src/a.rs (+1 −1)")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string() == "  └ Added tests/b.rs (+1 −0)")
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.to_string().contains("└ Edited"))
        );
    }

    #[test]
    fn labels_empty_file_creation_and_deletion_without_zero_counts() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Patched 2 files · 1s\n\nDeleted `old.json`\n--- old.json\n+++ /dev/null\n\nCreated `new.json`\n--- /dev/null\n+++ new.json",
            60,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string() == "  └ Deleted old.json")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string() == "  └ Added new.json")
        );
        assert!(!lines.iter().any(|line| line.to_string().contains("+0")));
    }

    #[test]
    fn right_aligns_mixed_width_line_numbers() {
        let mut lines = Vec::new();
        push(
            &mut lines,
            "Updated `a.rs` · 1s\n\n--- a.rs\n+++ a.rs\n@@ -99,2 +99,2 @@\n context\n-old\n+new",
            40,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().starts_with("     99   "))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().starts_with("    100 − "))
        );
    }
}
