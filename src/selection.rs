use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    row: usize,
    column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DragState {
    anchor: SelectionPoint,
    head: SelectionPoint,
    moved: bool,
}

#[derive(Debug, Default)]
pub struct PreviewSelection {
    area: Rect,
    lines: Vec<String>,
    drag: Option<DragState>,
    selection: Option<(SelectionPoint, SelectionPoint)>,
}

impl PreviewSelection {
    pub fn update_surface(&mut self, area: Rect, mut lines: Vec<String>) {
        lines.resize(area.height as usize, String::new());
        if self.drag.is_none() && (self.area != area || self.lines != lines) {
            self.selection = None;
        }
        self.area = area;
        self.lines = lines;
    }

    pub fn clear_surface(&mut self) {
        self.area = Rect::default();
        self.lines.clear();
        self.clear();
    }

    pub fn begin(&mut self, column: u16, row: u16) -> bool {
        let Some(point) = self.point_inside(column, row) else {
            return false;
        };
        self.selection = None;
        self.drag = Some(DragState {
            anchor: point,
            head: point,
            moved: false,
        });
        true
    }

    pub fn update(&mut self, column: u16, row: u16) -> bool {
        let Some(mut drag) = self.drag else {
            return false;
        };
        let Some(point) = self.point_clamped(column, row) else {
            return false;
        };
        drag.head = point;
        drag.moved |= point != drag.anchor;
        self.drag = Some(drag);
        self.selection = drag.moved.then_some((drag.anchor, drag.head));
        true
    }

    pub fn finish(&mut self, column: u16, row: u16) -> Option<String> {
        self.update(column, row);
        let drag = self.drag.take()?;
        if !drag.moved {
            self.selection = None;
            return None;
        }
        self.selection = Some((drag.anchor, drag.head));
        let text = self.selected_text();
        if text.is_empty() {
            self.selection = None;
            None
        } else {
            Some(text)
        }
    }

    pub fn clear(&mut self) {
        self.drag = None;
        self.selection = None;
    }

    pub fn range_for_row(&self, row: usize) -> Option<(usize, usize)> {
        let (first, last) = self.ordered_selection()?;
        if row < first.row || row > last.row {
            return None;
        }
        let line = self.lines.get(row).map(String::as_str).unwrap_or_default();
        let width = display_width(line);
        let start = if row == first.row {
            grapheme_start(line, first.column)
        } else {
            0
        };
        let end = if row == last.row {
            grapheme_end(line, last.column)
        } else {
            width
        };
        (start < end).then_some((start, end))
    }

    fn selected_text(&self) -> String {
        let Some((first, last)) = self.ordered_selection() else {
            return String::new();
        };
        (first.row..=last.row)
            .map(|row| {
                let line = self.lines.get(row).map(String::as_str).unwrap_or_default();
                match self.range_for_row(row) {
                    Some((start, end)) => slice_display_range(line, start, end),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ordered_selection(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        self.selection.map(|(anchor, head)| {
            if anchor <= head {
                (anchor, head)
            } else {
                (head, anchor)
            }
        })
    }

    fn point_inside(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        if self.area.is_empty()
            || column < self.area.x
            || column >= self.area.right()
            || row < self.area.y
            || row >= self.area.bottom()
        {
            return None;
        }
        Some(SelectionPoint {
            row: (row - self.area.y) as usize,
            column: (column - self.area.x) as usize,
        })
    }

    fn point_clamped(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        if self.area.is_empty() {
            return None;
        }
        let column = column.clamp(self.area.x, self.area.right().saturating_sub(1));
        let row = row.clamp(self.area.y, self.area.bottom().saturating_sub(1));
        self.point_inside(column, row)
    }
}

fn display_width(text: &str) -> usize {
    text.graphemes(true)
        .map(|grapheme| UnicodeWidthStr::width(grapheme).max(1))
        .sum()
}

fn grapheme_start(text: &str, column: usize) -> usize {
    let mut offset = 0;
    for grapheme in text.graphemes(true) {
        let next = offset + UnicodeWidthStr::width(grapheme).max(1);
        if column < next {
            return offset;
        }
        offset = next;
    }
    offset
}

fn grapheme_end(text: &str, column: usize) -> usize {
    let mut offset = 0;
    for grapheme in text.graphemes(true) {
        let next = offset + UnicodeWidthStr::width(grapheme).max(1);
        if column < next {
            return next;
        }
        offset = next;
    }
    offset
}

fn slice_display_range(text: &str, start: usize, end: usize) -> String {
    let mut output = String::new();
    let mut offset = 0;
    for grapheme in text.graphemes(true) {
        let next = offset + UnicodeWidthStr::width(grapheme).max(1);
        if offset < end && next > start {
            output.push_str(grapheme);
        }
        offset = next;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(lines: &[&str]) -> PreviewSelection {
        let mut selection = PreviewSelection::default();
        selection.update_surface(
            Rect::new(3, 5, 20, lines.len() as u16),
            lines.iter().map(|line| (*line).to_string()).collect(),
        );
        selection
    }

    #[test]
    fn dragging_forward_or_backward_selects_inclusive_terminal_cells() {
        let mut forward = selection(&["› hello"]);
        assert!(forward.begin(5, 5));
        assert!(forward.update(7, 5));
        assert_eq!(forward.finish(7, 5).as_deref(), Some("hel"));
        assert_eq!(forward.range_for_row(0), Some((2, 5)));

        let mut backward = selection(&["› hello"]);
        assert!(backward.begin(7, 5));
        assert!(backward.update(5, 5));
        assert_eq!(backward.finish(5, 5).as_deref(), Some("hel"));
        assert_eq!(backward.range_for_row(0), Some((2, 5)));
    }

    #[test]
    fn click_without_drag_does_not_copy() {
        let mut selection = selection(&["hello"]);
        assert!(selection.begin(3, 5));
        assert_eq!(selection.finish(3, 5), None);
        assert_eq!(selection.range_for_row(0), None);
    }

    #[test]
    fn multiline_selection_preserves_rendered_line_breaks() {
        let mut selection = selection(&["› hello", "│ world", "tail"]);
        assert!(selection.begin(5, 5));
        assert!(selection.update(5, 6));
        assert_eq!(selection.finish(5, 6).as_deref(), Some("hello\n│ w"));
        assert_eq!(selection.range_for_row(0), Some((2, 7)));
        assert_eq!(selection.range_for_row(1), Some((0, 3)));
        assert_eq!(selection.range_for_row(2), None);
    }

    #[test]
    fn wide_graphemes_are_selected_as_whole_cells() {
        let mut selection = selection(&["你a"]);
        assert!(selection.begin(3, 5));
        assert!(selection.update(4, 5));
        assert_eq!(selection.finish(4, 5).as_deref(), Some("你"));
        assert_eq!(selection.range_for_row(0), Some((0, 2)));
    }

    #[test]
    fn content_changes_clear_finished_selection_but_not_active_drag() {
        let mut selection = selection(&["hello"]);
        assert!(selection.begin(3, 5));
        assert!(selection.update(4, 5));
        selection.update_surface(Rect::new(3, 5, 20, 1), vec!["hullo".to_string()]);
        assert!(selection.range_for_row(0).is_some());
        assert!(selection.finish(4, 5).is_some());

        selection.update_surface(Rect::new(3, 5, 20, 1), vec!["world".to_string()]);
        assert_eq!(selection.range_for_row(0), None);
    }
}
