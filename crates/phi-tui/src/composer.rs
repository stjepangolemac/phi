use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
pub(crate) struct Composer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

pub(crate) struct ComposerLayout {
    rows: Vec<Range<usize>>,
    cursor_row: usize,
    cursor_column: usize,
}

impl ComposerLayout {
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    pub(crate) fn cursor_column(&self) -> usize {
        self.cursor_column
    }

    pub(crate) fn visible_rows<'a>(
        &'a self,
        text: &'a str,
        offset: usize,
        height: usize,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.rows
            .iter()
            .skip(offset)
            .take(height)
            .map(|range| &text[range.clone()])
    }
}

impl Composer {
    pub(crate) fn layout(&self, width: usize) -> ComposerLayout {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for (index, character) in self.text.char_indices() {
            if character == '\n' {
                wrap_line(&self.text, line_start, index, width, &mut rows);
                line_start = index + character.len_utf8();
            }
        }
        wrap_line(&self.text, line_start, self.text.len(), width, &mut rows);

        let full_cursor_row = rows.iter().rposition(|range| {
            range.end == self.cursor
                && range.start < range.end
                && UnicodeWidthStr::width(&self.text[range.clone()]) == width
        });
        if let Some(index) = full_cursor_row
            && !rows.iter().any(|range| range.start == self.cursor)
        {
            rows.insert(index + 1, self.cursor..self.cursor);
        }

        let cursor_row = rows
            .iter()
            .rposition(|range| range.start <= self.cursor)
            .unwrap_or_default();
        let row = &rows[cursor_row];
        let cursor_column = UnicodeWidthStr::width(&self.text[row.start..self.cursor.min(row.end)]);

        ComposerLayout {
            rows,
            cursor_row,
            cursor_column,
        }
    }

    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub(crate) fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub(crate) fn insert(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if let Some(previous) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        {
            self.text.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    pub(crate) fn delete(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.text
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    pub(crate) fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub(crate) fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    pub(crate) fn move_word_left(&mut self) {
        while self.previous_char().is_some_and(char::is_whitespace) {
            self.move_left();
        }
        while self
            .previous_char()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.move_left();
        }
    }

    pub(crate) fn move_word_right(&mut self) {
        while self.next_char().is_some_and(char::is_whitespace) {
            self.move_right();
        }
        while self
            .next_char()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.move_right();
        }
    }

    pub(crate) fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.text.drain(self.cursor..end);
    }

    pub(crate) fn delete_to_line_start(&mut self) {
        let end = self.cursor;
        self.move_line_start();
        self.text.drain(self.cursor..end);
    }

    pub(crate) fn move_line_start(&mut self) {
        self.cursor = self.line_start();
    }

    pub(crate) fn move_line_end(&mut self) {
        self.cursor = self.line_end();
    }

    pub(crate) fn move_start(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub(crate) fn move_up(&mut self, width: usize) {
        let layout = self.layout(width);
        if layout.cursor_row == 0 {
            return;
        }
        self.cursor = byte_at_display_column(
            &self.text,
            &layout.rows[layout.cursor_row - 1],
            layout.cursor_column,
        );
    }

    pub(crate) fn move_down(&mut self, width: usize) {
        let layout = self.layout(width);
        if layout.cursor_row + 1 >= layout.rows.len() {
            return;
        }
        self.cursor = byte_at_display_column(
            &self.text,
            &layout.rows[layout.cursor_row + 1],
            layout.cursor_column,
        );
    }

    pub(crate) fn on_first_visual_row(&self, width: usize) -> bool {
        self.layout(width).cursor_row == 0
    }

    pub(crate) fn on_last_visual_row(&self, width: usize) -> bool {
        let layout = self.layout(width);
        layout.cursor_row + 1 == layout.rows.len()
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }

    fn previous_char(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    fn next_char(&self) -> Option<char> {
        self.text[self.cursor..].chars().next()
    }
}

fn wrap_line(text: &str, start: usize, end: usize, width: usize, rows: &mut Vec<Range<usize>>) {
    if start == end {
        rows.push(start..end);
        return;
    }

    let mut row_start = start;
    let mut row_width = 0;
    let mut last_break = None;

    for (relative, character) in text[start..end].char_indices() {
        let index = start + relative;
        let character_width = character.width().unwrap_or_default();

        while row_width + character_width > width && index > row_start {
            let split = if character.is_whitespace() {
                index
            } else {
                last_break
                    .filter(|split| *split > row_start)
                    .unwrap_or(index)
            };
            rows.push(row_start..split);
            row_start = split;
            row_width = UnicodeWidthStr::width(&text[row_start..index]);
            last_break = last_whitespace_end(text, row_start, index);
        }

        row_width += character_width;
        if character.is_whitespace() {
            last_break = Some(index + character.len_utf8());
        }
    }

    rows.push(row_start..end);
}

fn last_whitespace_end(text: &str, start: usize, end: usize) -> Option<usize> {
    text[start..end]
        .char_indices()
        .filter(|(_, character)| character.is_whitespace())
        .map(|(index, character)| start + index + character.len_utf8())
        .next_back()
}

fn byte_at_display_column(text: &str, range: &Range<usize>, column: usize) -> usize {
    let mut used = 0;
    for (relative, character) in text[range.clone()].char_indices() {
        let character_width = character.width().unwrap_or_default();
        if used + character_width > column {
            return range.start + relative;
        }
        used += character_width;
    }
    range.end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_words_and_maps_the_cursor() {
        let mut composer = Composer::default();
        composer.set("one two three".into());
        let layout = composer.layout(7);

        assert_eq!(
            layout
                .visible_rows(&composer.text, 0, 10)
                .collect::<Vec<_>>(),
            vec!["one two", " three"]
        );
        assert_eq!(layout.cursor_row(), 1);
        assert_eq!(layout.cursor_column(), 6);
    }

    #[test]
    fn hard_wraps_words_wider_than_the_composer() {
        let mut composer = Composer::default();
        composer.set("abcdefgh".into());
        let layout = composer.layout(3);

        assert_eq!(
            layout
                .visible_rows(&composer.text, 0, 10)
                .collect::<Vec<_>>(),
            vec!["abc", "def", "gh"]
        );
    }

    #[test]
    fn vertical_movement_uses_visual_rows_and_display_width() {
        let mut composer = Composer::default();
        composer.set("ab 界 cd".into());

        composer.move_up(5);
        assert_eq!(&composer.text[..composer.cursor], "ab ");
        composer.move_down(5);
        assert_eq!(composer.cursor, composer.text.len());
    }

    #[test]
    fn preserves_explicit_empty_lines() {
        let mut composer = Composer::default();
        composer.set("one\n\nthree".into());
        let layout = composer.layout(20);

        assert_eq!(
            layout
                .visible_rows(&composer.text, 0, 10)
                .collect::<Vec<_>>(),
            vec!["one", "", "three"]
        );
    }

    #[test]
    fn places_the_cursor_on_a_new_row_after_an_exact_width_line() {
        let mut composer = Composer::default();
        composer.set("abcd".into());
        let layout = composer.layout(4);

        assert_eq!(layout.row_count(), 2);
        assert_eq!(layout.cursor_row(), 1);
        assert_eq!(layout.cursor_column(), 0);
    }
}
