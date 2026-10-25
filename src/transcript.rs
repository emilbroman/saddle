//! Conversation transcript: the entry model, its wrapped line layout and the
//! scroll state of the transcript pane.

use std::ops::Range;

use crate::session::{LogEvent, Session, UiEvent};
use serde::{Deserialize, Serialize};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const GUTTER: usize = 2;
const TOOL_CALL_MAX_LINES: usize = 3;
const TOOL_RESULT_MAX_LINES: usize = 6;
const SHELL_OUTPUT_TAIL_LINES: usize = 20;
const CHANGES_MAX_LINES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamState {
    Streaming,
    Complete,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeLevel {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        text: String,
        state: StreamState,
    },
    ToolCall {
        id: String,
        name: String,
        /// One-line rendering of the call's arguments.
        summary: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Notice {
        text: String,
        level: NoticeLevel,
    },
    /// Saved file changes delivered to the agent.
    FilesChanged {
        headline: String,
        paths: Vec<String>,
        /// One line per file, shown when expanded.
        details: Vec<String>,
    },
}

#[derive(Debug)]
struct Item {
    entry: Entry,
    shell_result: bool,
    layout: Option<(usize, Vec<Line<'static>>)>,
}

#[derive(Debug, Default)]
pub struct Transcript {
    items: Vec<Item>,
    /// Show full details instead of previews (including shell output tails).
    expanded: bool,
    session: Option<Session>,
    persistence_error: Option<String>,
}

impl Transcript {
    pub fn restore(events: Vec<UiEvent>) -> Self {
        let mut transcript = Self::default();
        for event in events {
            match event {
                UiEvent::Push(entry) => transcript.push(entry),
                UiEvent::Delta(delta) => transcript.append_assistant(&delta),
                UiEvent::Finish(state) => transcript.finish_assistant(state),
            }
        }
        transcript.finish_assistant(StreamState::Interrupted);
        transcript
    }

    pub fn attach_session(&mut self, session: Session) {
        self.session = Some(session);
    }

    pub fn take_persistence_error(&mut self) -> Option<String> {
        self.persistence_error.take()
    }

    fn persist(&mut self, event: UiEvent) {
        if let Some(session) = &self.session
            && let Err(error) = session.append(LogEvent::Ui(event))
        {
            self.persistence_error = Some(format!("Cannot save the conversation: {error:#}"));
        }
    }

    pub fn push(&mut self, entry: Entry) {
        self.persist(UiEvent::Push(entry.clone()));
        let shell_result = match &entry {
            Entry::ToolResult { call_id, .. } => self
                .items
                .iter()
                .rev()
                .find_map(|item| match &item.entry {
                    Entry::ToolCall { id, name, .. } if id == call_id => Some(matches!(
                        name.as_str(),
                        "shell_start" | "shell_wait" | "shell_output"
                    )),
                    _ => None,
                })
                .unwrap_or(false),
            _ => false,
        };
        self.items.push(Item {
            entry,
            shell_result,
            layout: None,
        });
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.items.iter().map(|item| &item.entry)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        for item in &mut self.items {
            if matches!(
                item.entry,
                Entry::ToolCall { .. } | Entry::ToolResult { .. } | Entry::FilesChanged { .. }
            ) {
                item.layout = None;
            }
        }
    }

    /// Appends streamed text to the trailing in-progress assistant entry,
    /// starting a new one if there is none.
    pub fn append_assistant(&mut self, delta: &str) {
        self.persist(UiEvent::Delta(delta.to_owned()));
        if let Some(Item {
            entry:
                Entry::Assistant {
                    text,
                    state: StreamState::Streaming,
                },
            layout,
            ..
        }) = self.items.last_mut()
        {
            text.push_str(delta);
            *layout = None;
        } else {
            self.items.push(Item {
                entry: Entry::Assistant {
                    text: delta.to_owned(),
                    state: StreamState::Streaming,
                },
                shell_result: false,
                layout: None,
            });
        }
    }

    /// Ends the trailing in-progress assistant entry, if any.
    pub fn finish_assistant(&mut self, final_state: StreamState) {
        if matches!(
            self.items.last().map(|item| &item.entry),
            Some(Entry::Assistant {
                state: StreamState::Streaming,
                ..
            })
        ) {
            self.persist(UiEvent::Finish(final_state));
        }
        if let Some(Item {
            entry: Entry::Assistant { state, .. },
            layout,
            ..
        }) = self.items.last_mut()
            && *state == StreamState::Streaming
        {
            *state = final_state;
            *layout = None;
        }
    }

    /// Total number of rendered lines at `width` columns.
    pub fn line_count(&mut self, width: u16) -> usize {
        let width = usize::from(width);
        let expanded = self.expanded;
        self.items
            .iter_mut()
            .map(|item| item.lines(width, expanded).len())
            .sum()
    }

    /// Rendered lines in `range`, as counted by [`Self::line_count`].
    pub fn lines(&mut self, width: u16, range: Range<usize>) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let mut out = Vec::with_capacity(range.len());
        let mut offset = 0;
        let expanded = self.expanded;
        for item in &mut self.items {
            let lines = item.lines(width, expanded);
            let item_range = offset..offset + lines.len();
            offset = item_range.end;
            if item_range.end <= range.start {
                continue;
            }
            if item_range.start >= range.end {
                break;
            }
            let from = range.start.saturating_sub(item_range.start);
            let to = (range.end - item_range.start).min(lines.len());
            out.extend_from_slice(&lines[from..to]);
        }
        out
    }
}

impl Item {
    fn lines(&mut self, width: usize, expanded: bool) -> &[Line<'static>] {
        if self.layout.as_ref().is_none_or(|(w, _)| *w != width) {
            self.layout = Some((
                width,
                layout_entry(&self.entry, width, expanded, self.shell_result),
            ));
        }
        &self.layout.as_ref().unwrap().1
    }
}

/// Every entry is preceded by a blank separator line and rendered with a
/// two-column gutter whose first row carries the entry's marker.
fn layout_entry(
    entry: &Entry,
    width: usize,
    expanded: bool,
    shell_result: bool,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(GUTTER).max(1);
    let dim = Style::new().add_modifier(Modifier::DIM);

    let (marker, marker_style, body_style, mut rows, max_rows) = match entry {
        Entry::User { text } => (
            "›",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Style::new().fg(Color::Cyan),
            wrap(text, body_width),
            None,
        ),
        Entry::Assistant { text, state } => {
            let mut text = text.clone();
            if *state == StreamState::Streaming {
                text.push('▍');
            }
            let mut rows = wrap(&text, body_width);
            if *state == StreamState::Interrupted {
                rows.push("[interrupted]".into());
            }
            (
                "●",
                Style::new().fg(Color::Magenta),
                Style::new(),
                rows,
                None,
            )
        }
        Entry::ToolCall { name, summary, .. } => (
            "⚙",
            Style::new().fg(Color::Yellow),
            Style::new().fg(Color::Yellow),
            wrap(&format!("{name} {summary}"), body_width),
            Some(TOOL_CALL_MAX_LINES),
        ),
        Entry::ToolResult {
            output, is_error, ..
        } => {
            let style = if *is_error {
                Style::new().fg(Color::Red)
            } else {
                dim
            };
            (
                "↳",
                style,
                style,
                if shell_result && !expanded {
                    shell_preview(output, body_width)
                } else {
                    wrap(output, body_width)
                },
                (!shell_result).then_some(TOOL_RESULT_MAX_LINES),
            )
        }
        Entry::Notice { text, level } => {
            let (marker, style) = match level {
                NoticeLevel::Info => ("·", dim.add_modifier(Modifier::ITALIC)),
                NoticeLevel::Error => ("✗", Style::new().fg(Color::Red)),
            };
            (marker, style, style, wrap(text, body_width), None)
        }
        Entry::FilesChanged {
            headline,
            paths,
            details,
        } => {
            let style = Style::new().fg(Color::Blue);
            let rows = if expanded {
                let mut rows = wrap(headline, body_width);
                for detail in details {
                    rows.extend(wrap(detail, body_width));
                }
                rows
            } else {
                wrap(&format!("{headline}: {}", paths.join(", ")), body_width)
            };
            ("✎", style, style, rows, Some(CHANGES_MAX_LINES))
        }
    };

    if let Some(max) = max_rows.filter(|_| !expanded)
        && rows.len() > max
    {
        let hidden = rows.len() - (max - 1);
        rows.truncate(max - 1);
        rows.push(format!("… {hidden} more lines (Ctrl+O to expand)"));
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(Line::default());
    for (i, row) in rows.into_iter().enumerate() {
        let gutter = if i == 0 {
            Span::styled(format!("{marker} "), marker_style)
        } else {
            Span::raw(" ".repeat(GUTTER))
        };
        lines.push(Line::from(vec![gutter, Span::styled(row, body_style)]));
    }
    lines
}

fn shell_preview(output: &str, width: usize) -> Vec<String> {
    let lines: Vec<_> = output.split('\n').collect();
    // Job reports have an optional wait note, a status, output and a log path.
    let header_len = lines
        .iter()
        .take(2)
        .position(|line| line.starts_with("job "))
        .map_or(0, |index| index + 1);
    let footer_start = if lines
        .last()
        .is_some_and(|line| line.starts_with("[full log: "))
    {
        lines.len() - 1
    } else {
        lines.len()
    };
    let mut rows = Vec::new();
    for line in &lines[..header_len] {
        rows.extend(wrap(line, width));
    }
    let mut tail = if header_len < footer_start {
        wrap(&lines[header_len..footer_start].join("\n"), width)
    } else {
        Vec::new()
    };
    if tail.len() > SHELL_OUTPUT_TAIL_LINES {
        let hidden = tail.len() - SHELL_OUTPUT_TAIL_LINES;
        rows.extend(wrap(
            &format!("… {hidden} earlier lines (Ctrl+O to expand)"),
            width,
        ));
        tail.drain(..hidden);
    }
    rows.extend(tail);
    for line in &lines[footer_start..] {
        rows.extend(wrap(line, width));
    }
    rows
}

/// Wraps `text` to `width` display columns, keeping explicit newlines, blank
/// lines and leading indentation, and breaking words longer than a line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let options = textwrap::Options::new(width)
        .break_words(true)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = line
            .strip_suffix('\r')
            .unwrap_or(line)
            .replace('\t', "    ");
        if line.trim().is_empty() {
            rows.push(String::new());
            continue;
        }
        rows.extend(
            textwrap::wrap(&line, &options)
                .into_iter()
                .map(|row| row.into_owned()),
        );
    }
    rows
}

/// Scroll position of the transcript pane. Follows the bottom of the
/// transcript until the user scrolls up; then stays anchored to a fixed top
/// line as content streams in, until scrolled back to the bottom.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    pinned_top: Option<usize>,
}

impl Scroll {
    pub fn is_following(&self) -> bool {
        self.pinned_top.is_none()
    }

    pub fn visible(&mut self, total: usize, height: usize) -> Range<usize> {
        let max_top = total.saturating_sub(height);
        let top = match self.pinned_top {
            Some(top) if top < max_top => top,
            _ => {
                self.pinned_top = None;
                max_top
            }
        };
        top..(top + height).min(total)
    }

    pub fn up(&mut self, lines: usize, total: usize, height: usize) {
        let max_top = total.saturating_sub(height);
        let top = self.pinned_top.unwrap_or(max_top).min(max_top);
        self.pin(top.saturating_sub(lines), max_top);
    }

    pub fn down(&mut self, lines: usize, total: usize, height: usize) {
        if let Some(top) = self.pinned_top {
            self.pin(top.saturating_add(lines), total.saturating_sub(height));
        }
    }

    pub fn top(&mut self, total: usize, height: usize) {
        self.pin(0, total.saturating_sub(height));
    }

    pub fn bottom(&mut self) {
        self.pinned_top = None;
    }

    fn pin(&mut self, top: usize, max_top: usize) {
        self.pinned_top = (top < max_top).then_some(top);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrap_breaks_on_words_and_long_words() {
        assert_eq!(
            wrap("hello brave new world", 11),
            ["hello brave", "new world"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_keeps_newlines_blank_lines_and_indentation() {
        assert_eq!(
            wrap("fn main() {\n    body();\n\n}", 40),
            ["fn main() {", "    body();", "", "}"]
        );
        assert_eq!(wrap("a\r\nb", 10), ["a", "b"]);
        assert_eq!(wrap("\tx", 10), ["    x"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn wrap_counts_display_width() {
        assert_eq!(wrap("日本語テキスト", 6), ["日本語", "テキス", "ト"]);
        assert_eq!(wrap("x", 0), ["x"]);
    }

    #[test]
    fn layout_adds_separator_and_gutter() {
        let mut t = Transcript::default();
        t.push(Entry::User {
            text: "one two three".into(),
        });
        assert_eq!(t.line_count(9), 3);
        assert_eq!(text(&t.lines(9, 0..3)), ["", "› one two", "  three"]);
    }

    #[test]
    fn lines_slices_across_entries() {
        let mut t = Transcript::default();
        for word in ["a", "b", "c"] {
            t.push(Entry::User { text: word.into() });
        }
        assert_eq!(t.line_count(20), 6);
        assert_eq!(text(&t.lines(20, 1..4)), ["› a", "", "› b"]);
        assert_eq!(text(&t.lines(20, 5..6)), ["› c"]);
        assert!(t.lines(20, 6..10).is_empty());
    }

    #[test]
    fn streaming_appends_and_relayouts() {
        let mut t = Transcript::default();
        t.push(Entry::User { text: "q".into() });
        t.append_assistant("Hel");
        assert_eq!(text(&t.lines(40, 2..4)), ["", "● Hel▍"]);
        t.append_assistant("lo");
        assert_eq!(text(&t.lines(40, 3..4)), ["● Hello▍"]);
        t.finish_assistant(StreamState::Complete);
        assert_eq!(text(&t.lines(40, 3..4)), ["● Hello"]);

        t.append_assistant("Next");
        t.finish_assistant(StreamState::Interrupted);
        assert_eq!(t.entries().count(), 3);
        assert_eq!(text(&t.lines(40, 5..7)), ["● Next", "  [interrupted]"]);
    }

    #[test]
    fn finish_without_streaming_entry_is_noop() {
        let mut t = Transcript::default();
        t.push(Entry::User { text: "q".into() });
        t.finish_assistant(StreamState::Complete);
        assert_eq!(
            t.entries().collect::<Vec<_>>(),
            [&Entry::User { text: "q".into() }]
        );
    }

    #[test]
    fn layout_cache_follows_width() {
        let mut t = Transcript::default();
        t.push(Entry::User {
            text: "aaaa bbbb".into(),
        });
        assert_eq!(t.line_count(40), 2);
        assert_eq!(t.line_count(6), 3);
        assert_eq!(t.line_count(40), 2);
    }

    #[test]
    fn tool_output_is_collapsed() {
        let mut t = Transcript::default();
        let output = (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        t.push(Entry::ToolResult {
            call_id: "1".into(),
            output,
            is_error: false,
        });
        let total = t.line_count(40);
        assert_eq!(total, 1 + TOOL_RESULT_MAX_LINES);
        let lines = text(&t.lines(40, 0..total));
        assert_eq!(lines[1], "↳ 1");
        assert_eq!(lines.last().unwrap(), "  … 5 more lines (Ctrl+O to expand)");

        t.toggle_expanded();
        assert_eq!(t.line_count(40), 11);
        t.toggle_expanded();
        assert_eq!(t.line_count(40), total);
    }

    fn shell_report(status: &str) -> String {
        let output = (1..=30)
            .map(|i| format!("output {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("job 7 {status} after 2s\n{output}\n[full log: /tmp/7.log]")
    }

    fn push_tool(t: &mut Transcript, id: &str, name: &str, output: String) {
        t.push(Entry::ToolCall {
            id: id.into(),
            name: name.into(),
            summary: "command".into(),
        });
        t.push(Entry::ToolResult {
            call_id: id.into(),
            output,
            is_error: false,
        });
    }

    #[test]
    fn shell_results_show_status_and_twenty_line_tail_by_default() {
        for name in ["shell_start", "shell_wait", "shell_output"] {
            let mut t = Transcript::default();
            push_tool(&mut t, "1", name, shell_report("exited with status 1"));
            let total = t.line_count(100);
            let lines = text(&t.lines(100, 0..total));
            assert_eq!(lines[3], "↳ job 7 exited with status 1 after 2s");
            assert_eq!(lines[4], "  … 10 earlier lines (Ctrl+O to expand)");
            assert_eq!(lines[5], "  output 11");
            assert_eq!(lines[24], "  output 30");
            assert_eq!(lines[25], "  [full log: /tmp/7.log]");

            t.toggle_expanded();
            let expanded_total = t.line_count(100);
            let expanded = text(&t.lines(100, 0..expanded_total));
            assert!(expanded.contains(&"  output 1".to_owned()));
            assert_eq!(expanded_total, total + 9);
            t.toggle_expanded();
            assert_eq!(t.line_count(100), total);
            assert_eq!(text(&t.lines(100, 0..total)), lines);
        }
    }

    #[test]
    fn shell_preview_keeps_wait_note_status_and_no_output() {
        let output = "Wait interrupted; the job keeps running.\njob 7 running after 2s (still running; shell_wait or shell_output with job_id 7)\n(no new output)\n[full log: /tmp/7.log]";
        assert_eq!(
            shell_preview(output, 100),
            output.split('\n').collect::<Vec<_>>()
        );
        let output = format!(
            "Waited 60 s; the job keeps running.\n{}",
            shell_report("running")
        );
        let rows = shell_preview(&output, 100);
        assert_eq!(rows[0], "Waited 60 s; the job keeps running.");
        assert_eq!(rows[1], "job 7 running after 2s");
        assert_eq!(rows[3], "output 11");
    }

    #[test]
    fn shell_results_are_matched_by_call_id_and_restored_without_schema_changes() {
        let events = vec![
            UiEvent::Push(Entry::ToolCall {
                id: "shell".into(),
                name: "shell_start".into(),
                summary: "command".into(),
            }),
            UiEvent::Push(Entry::ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                summary: "file".into(),
            }),
            UiEvent::Push(Entry::ToolResult {
                call_id: "shell".into(),
                output: shell_report("running"),
                is_error: false,
            }),
        ];
        let mut t = Transcript::restore(events);
        let total = t.line_count(100);
        let lines = text(&t.lines(100, 0..total));
        assert!(lines.contains(&"  output 30".to_owned()));
        assert!(!lines.contains(&"  output 1".to_owned()));
        assert_eq!(t.entries().count(), 3);
    }

    #[test]
    fn unrelated_and_unmatched_results_keep_collapsed_default() {
        for name in ["shell_list", "shell_kill", "read_file"] {
            let mut t = Transcript::default();
            push_tool(&mut t, "1", name, shell_report("running"));
            assert_eq!(t.line_count(100), 3 + TOOL_RESULT_MAX_LINES);
        }
        let mut t = Transcript::default();
        t.push(Entry::ToolResult {
            call_id: "missing".into(),
            output: shell_report("running"),
            is_error: false,
        });
        assert_eq!(t.line_count(100), 1 + TOOL_RESULT_MAX_LINES);
    }

    #[test]
    fn shell_preview_rewraps_the_tail_on_resize_and_preserves_error_style() {
        let mut t = Transcript::default();
        t.push(Entry::ToolCall {
            id: "1".into(),
            name: "shell_start".into(),
            summary: "command".into(),
        });
        t.push(Entry::ToolResult {
            call_id: "1".into(),
            output: "failed to start".into(),
            is_error: true,
        });
        let wide = t.line_count(100);
        assert_eq!(wide, 4);
        let lines = t.lines(100, 0..wide);
        assert_eq!(text(&lines)[3], "↳ failed to start");
        assert_eq!(lines[3].spans[1].style.fg, Some(Color::Red));
        assert!(t.line_count(8) > wide);
        assert_eq!(t.line_count(100), wide);

        let output = format!(
            "job 7 running after 2s\n{}\n[full log: /tmp/7.log]",
            "x".repeat(300)
        );
        let rows = shell_preview(&output, 10);
        assert_eq!(
            rows.iter()
                .filter(|row| row.as_str() == "xxxxxxxxxx")
                .count(),
            20
        );
        assert!(!rows.iter().any(|row| row.contains("more lines")));
    }

    #[test]
    fn file_changes_are_one_line_until_expanded() {
        let mut t = Transcript::default();
        t.push(Entry::FilesChanged {
            headline: "2 files changed".into(),
            paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            details: vec![
                "modified src/a.rs (+1 -0)".into(),
                "deleted src/b.rs".into(),
            ],
        });
        let total = t.line_count(60);
        assert_eq!(
            text(&t.lines(60, 0..total)),
            ["", "✎ 2 files changed: src/a.rs, src/b.rs"]
        );

        t.toggle_expanded();
        let total = t.line_count(60);
        assert_eq!(
            text(&t.lines(60, 0..total)),
            [
                "",
                "✎ 2 files changed",
                "  modified src/a.rs (+1 -0)",
                "  deleted src/b.rs"
            ]
        );
    }

    #[test]
    fn scroll_follows_bottom_by_default() {
        let mut s = Scroll::default();
        assert_eq!(s.visible(100, 10), 90..100);
        assert_eq!(s.visible(120, 10), 110..120);
        assert_eq!(s.visible(5, 10), 0..5);
        assert!(s.is_following());
    }

    #[test]
    fn scroll_up_pins_view_while_content_grows() {
        let mut s = Scroll::default();
        s.up(5, 100, 10);
        assert_eq!(s.visible(100, 10), 85..95);
        assert_eq!(s.visible(150, 10), 85..95);
        s.down(3, 150, 10);
        assert_eq!(s.visible(150, 10), 88..98);
    }

    #[test]
    fn scroll_down_to_bottom_resumes_following() {
        let mut s = Scroll::default();
        s.up(5, 100, 10);
        s.down(50, 100, 10);
        assert!(s.is_following());
        assert_eq!(s.visible(200, 10), 190..200);
    }

    #[test]
    fn scroll_is_clamped() {
        let mut s = Scroll::default();
        s.up(1_000, 100, 10);
        assert_eq!(s.visible(100, 10), 0..10);
        s.up(1, 5, 10);
        assert!(s.is_following());

        let mut s = Scroll::default();
        s.top(100, 10);
        assert_eq!(s.visible(100, 10), 0..10);
        // A resize that makes everything fit returns to following.
        assert_eq!(s.visible(8, 10), 0..8);
        assert!(s.is_following());
        s.top(100, 10);
        s.bottom();
        assert!(s.is_following());
    }
}
