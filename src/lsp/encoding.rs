//! Conversion between Saddle's positions (columns in Unicode scalar values)
//! and the server's negotiated LSP position encoding.

use lsp_types::PositionEncodingKind;

use super::TextPosition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// What Saddle offers in `general.positionEncodings`, most preferred first.
    pub fn offered() -> Vec<PositionEncodingKind> {
        vec![
            PositionEncodingKind::UTF8,
            PositionEncodingKind::UTF32,
            PositionEncodingKind::UTF16,
        ]
    }

    /// The server's choice from its capabilities; absent means UTF-16.
    pub fn negotiated(kind: Option<&PositionEncodingKind>) -> Self {
        match kind.map(PositionEncodingKind::as_str) {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    fn units(self, c: char) -> u32 {
        match self {
            Self::Utf8 => c.len_utf8() as u32,
            Self::Utf16 => c.len_utf16() as u32,
            Self::Utf32 => 1,
        }
    }

    /// Character column → code-unit offset in this encoding, clamped to the line end.
    pub fn encode_column(self, line: &str, column: u32) -> u32 {
        line.chars()
            .take(column as usize)
            .map(|c| self.units(c))
            .sum()
    }

    /// Code-unit offset → character column. An offset inside a character
    /// rounds down; one past the line end clamps to it.
    pub fn decode_column(self, line: &str, offset: u32) -> u32 {
        let mut units = 0;
        let mut column = 0;
        for c in line.chars() {
            units += self.units(c);
            if units > offset {
                break;
            }
            column += 1;
        }
        column
    }
}

/// Lines of a document as LSP counts them (`\n` and `\r\n` terminators).
pub struct Lines<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            lines: text
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect(),
        }
    }

    pub fn line(&self, index: u32) -> Option<&'a str> {
        self.lines.get(index as usize).copied()
    }

    /// A position past the last line clamps to the end of the document.
    pub fn encode(&self, encoding: PositionEncoding, pos: TextPosition) -> lsp_types::Position {
        match self.line(pos.line) {
            Some(line) => {
                lsp_types::Position::new(pos.line, encoding.encode_column(line, pos.column))
            }
            None => {
                let last = self.lines.len().saturating_sub(1) as u32;
                let line = self.line(last).unwrap_or("");
                lsp_types::Position::new(last, encoding.encode_column(line, u32::MAX))
            }
        }
    }

    pub fn decode(&self, encoding: PositionEncoding, pos: lsp_types::Position) -> TextPosition {
        let column = self.line(pos.line).map_or(pos.character, |line| {
            encoding.decode_column(line, pos.character)
        });
        TextPosition {
            line: pos.line,
            column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "a€😀b"; // 1, 3 and 4 UTF-8 bytes; 1, 1 and 2 UTF-16 units

    #[test]
    fn negotiation() {
        assert_eq!(PositionEncoding::negotiated(None), PositionEncoding::Utf16);
        let utf8 = PositionEncodingKind::UTF8;
        assert_eq!(
            PositionEncoding::negotiated(Some(&utf8)),
            PositionEncoding::Utf8
        );
        let utf32 = PositionEncodingKind::UTF32;
        assert_eq!(
            PositionEncoding::negotiated(Some(&utf32)),
            PositionEncoding::Utf32
        );
        let odd = PositionEncodingKind::new("utf-7");
        assert_eq!(
            PositionEncoding::negotiated(Some(&odd)),
            PositionEncoding::Utf16
        );
        assert_eq!(PositionEncoding::offered()[0], PositionEncodingKind::UTF8);
    }

    #[test]
    fn encode_columns() {
        use PositionEncoding::*;
        let cases = [
            (0, [0, 0, 0]),
            (1, [1, 1, 1]),
            (2, [4, 2, 2]),
            (3, [8, 4, 3]),
            (4, [9, 5, 4]),
        ];
        for (column, [utf8, utf16, utf32]) in cases {
            assert_eq!(Utf8.encode_column(LINE, column), utf8, "utf8 {column}");
            assert_eq!(Utf16.encode_column(LINE, column), utf16, "utf16 {column}");
            assert_eq!(Utf32.encode_column(LINE, column), utf32, "utf32 {column}");
        }
        assert_eq!(Utf16.encode_column(LINE, 99), 5);
    }

    #[test]
    fn decode_round_trips() {
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            for column in 0..=4 {
                let offset = encoding.encode_column(LINE, column);
                assert_eq!(
                    encoding.decode_column(LINE, offset),
                    column,
                    "{encoding:?} {column}"
                );
            }
        }
    }

    #[test]
    fn decode_inside_a_character_rounds_down() {
        assert_eq!(PositionEncoding::Utf8.decode_column(LINE, 2), 1);
        assert_eq!(PositionEncoding::Utf8.decode_column(LINE, 7), 2);
        assert_eq!(PositionEncoding::Utf16.decode_column(LINE, 3), 2);
        assert_eq!(PositionEncoding::Utf16.decode_column(LINE, 99), 4);
    }

    #[test]
    fn lines_handle_crlf_and_clamp() {
        let text = "fn a() {}\r\nlet é = 1;\n";
        let lines = Lines::new(text);
        assert_eq!(lines.line(0), Some("fn a() {}"));
        assert_eq!(lines.line(1), Some("let é = 1;"));
        assert_eq!(lines.line(2), Some(""));
        assert_eq!(lines.line(3), None);

        let pos = TextPosition { line: 1, column: 6 };
        let lsp = lines.encode(PositionEncoding::Utf8, pos);
        assert_eq!(lsp, lsp_types::Position::new(1, 7));
        assert_eq!(lines.decode(PositionEncoding::Utf8, lsp), pos);

        let past = lines.encode(PositionEncoding::Utf16, TextPosition { line: 9, column: 0 });
        assert_eq!(past, lsp_types::Position::new(2, 0));
        let unknown = lines.decode(PositionEncoding::Utf16, lsp_types::Position::new(9, 3));
        assert_eq!(unknown, TextPosition { line: 9, column: 3 });
    }
}
