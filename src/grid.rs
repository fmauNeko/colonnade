//! Shared result grid: Kit table columns, cell display and a delegate over result rows.

use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Empty, Pixels, SharedString, Window, div, px};

use crate::db::model::{Cell, ColumnInfo};

const COLUMN_WIDTH: Pixels = px(160.);
const SHOWN_CHARS: usize = 500;
const ELLIPSIS: &str = "…";

/// Builds the Kit column for the result column at `index`.
///
/// The key comes from the index, so result columns sharing a name stay distinct.
/// Numeric columns are right aligned; `sortable` adds Kit's sort control.
#[must_use]
pub fn column_for(info: &ColumnInfo, index: usize, sortable: bool) -> Column {
    Column::new(format!("c{index}"), info.name.as_str())
        .width(COLUMN_WIDTH)
        .when(info.numeric, Column::text_right)
        .when(sortable, Column::sortable)
}

/// How a cell's text is styled: values are normal, placeholders muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellTone {
    Normal,
    Muted,
    MutedItalic,
}

/// What the grid shows for one cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellDisplay {
    pub text: SharedString,
    pub tone: CellTone,
    pub align_right: bool,
}

/// Describes how the grid shows `cell` in a column that is `numeric` or not.
///
/// NULL reads `NULL` in muted italics, so it never looks like empty text. Binary
/// data shows its size, muted. Text shows at most 500 characters of its first line,
/// followed by `…` when the cell holds more or the driver truncated the value.
#[must_use]
pub fn cell_display(cell: &Cell, numeric: bool) -> CellDisplay {
    let (text, tone) = match cell {
        Cell::Null => (SharedString::new_static("NULL"), CellTone::MutedItalic),
        Cell::Text { value, truncated } => {
            let first_line = value.split(['\n', '\r']).next().unwrap_or_default();
            let shown = first_line
                .char_indices()
                .nth(SHOWN_CHARS)
                .map_or(first_line, |(end, _)| &first_line[..end]);
            let cut = *truncated || shown.len() < value.len();
            let ellipsis = if cut { ELLIPSIS } else { "" };
            (format!("{shown}{ellipsis}").into(), CellTone::Normal)
        }
        Cell::Binary { len } => {
            let unit = if *len == 1 { "byte" } else { "bytes" };
            (format!("<binary {len} {unit}>").into(), CellTone::Muted)
        }
    };
    CellDisplay {
        text,
        tone,
        align_right: numeric,
    }
}

/// The text copying `cell` yields: the whole value of a text cell, as far as the
/// driver read it, otherwise the placeholder the grid shows.
#[must_use]
pub fn copy_text(cell: &Cell) -> String {
    match cell {
        Cell::Text { value, .. } => value.clone(),
        Cell::Null | Cell::Binary { .. } => cell_display(cell, false).text.into(),
    }
}

/// Renders a cell for a Kit table: monospace, muted placeholders, right aligned
/// in numeric columns and ended with an ellipsis when it overflows its column.
#[must_use]
pub fn render_cell(display: &CellDisplay, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .w_full()
        .truncate()
        .font_family(theme.mono_font_family.clone())
        .text_size(theme.mono_font_size)
        .when(display.align_right, Styled::text_right)
        .map(|cell| match display.tone {
            CellTone::Normal => cell,
            CellTone::Muted => cell.text_color(theme.muted_foreground),
            CellTone::MutedItalic => cell.text_color(theme.muted_foreground).italic(),
        })
        .child(display.text.clone())
        .into_any_element()
}

/// A Kit table delegate over one result's rows. It does not sort.
pub struct RowsDelegate {
    pub columns: Vec<ColumnInfo>,
    pub rows: Arc<Vec<Vec<Cell>>>,
}

impl RowsDelegate {
    fn cell(&self, row_ix: usize, col_ix: usize) -> Option<&Cell> {
        self.rows.get(row_ix)?.get(col_ix)
    }
}

impl TableDelegate for RowsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns
            .get(col_ix)
            .map_or_else(Column::default, |info| column_for(info, col_ix, false))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let numeric = self.columns.get(col_ix).is_some_and(|info| info.numeric);
        match self.cell(row_ix, col_ix) {
            Some(cell) => render_cell(&cell_display(cell, numeric), cx),
            None => Empty.into_any_element(),
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.cell(row_ix, col_ix).map(copy_text).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::component::table::{ColumnSort, DataTable, TableDelegate as _, TableState};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _, Point, Render,
        Styled as _, TestAppContext, TextAlign, Window, WindowBounds, WindowOptions, div, px, size,
    };

    use super::{CellTone, RowsDelegate, cell_display, column_for};
    use crate::db::model::{Cell, ColumnInfo};

    fn info(name: &str, numeric: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: if numeric { "integer" } else { "text" }.into(),
            numeric,
        }
    }

    fn text(value: &str) -> Cell {
        Cell::text(value.to_owned())
    }

    #[test]
    fn null_and_empty_text_render_differently() {
        // Given: a NULL cell and an empty text cell.
        let (null, empty) = (Cell::Null, text(""));
        // When: displaying both.
        let (null, empty) = (cell_display(&null, false), cell_display(&empty, false));
        // Then: NULL shows a muted italic placeholder; empty text stays empty and normal.
        assert_ne!(null.text, empty.text);
        assert_eq!(null.tone, CellTone::MutedItalic);
        assert_eq!(empty.text, "");
        assert_eq!(empty.tone, CellTone::Normal);
    }

    #[test]
    fn binary_shows_its_size_muted() {
        // Given: a binary cell of 4096 bytes.
        let cell = Cell::Binary { len: 4096 };
        // When: displaying it.
        let display = cell_display(&cell, false);
        // Then: the muted text reports the size.
        assert!(display.text.contains("4096"));
        assert_eq!(display.tone, CellTone::Muted);
    }

    #[test]
    fn long_text_is_cut_after_500_characters() {
        // Given: single lines of 500 and 501 two-byte characters.
        let first_500 = "é".repeat(500);
        for (len, shown) in [(500, first_500.clone()), (501, format!("{first_500}…"))] {
            // When: displaying the line.
            let display = cell_display(&text(&"é".repeat(len)), false);
            // Then: 500 characters remain, with an ellipsis only when the line was longer.
            assert_eq!(display.text, shown, "{len} characters");
        }
    }

    #[test]
    fn multi_line_text_shows_its_first_line() {
        // Given: texts with Unix and Windows line breaks.
        for value in ["first\nsecond", "first\r\nsecond"] {
            // When: displaying the text.
            let display = cell_display(&text(value), false);
            // Then: only the first line remains, marked as cut.
            assert_eq!(display.text, "first…", "{value:?}");
        }
    }

    #[test]
    fn driver_truncated_text_is_marked() {
        // Given: a short value the driver reported as truncated.
        let cell = Cell::Text {
            value: "abc".into(),
            truncated: true,
        };
        // When: displaying it.
        let display = cell_display(&cell, false);
        // Then: the value is followed by an ellipsis.
        assert_eq!(display.text, "abc…");
    }

    #[test]
    fn numeric_columns_align_every_cell_right() {
        // Given: one cell of each kind.
        for cell in [text("42"), Cell::Null, Cell::Binary { len: 1 }] {
            for numeric in [true, false] {
                // When: displaying it in a numeric or a text column.
                let display = cell_display(&cell, numeric);
                // Then: only the numeric column aligns it right.
                assert_eq!(display.align_right, numeric, "{cell:?}");
            }
        }
    }

    #[test]
    fn numeric_column_is_right_aligned() {
        // Given: a numeric result column.
        let info = info("id", true);
        // When: building its Kit column without sorting.
        let column = column_for(&info, 0, false);
        // Then: it is 160 px wide, named after the column, right aligned and unsortable.
        assert_eq!(column.width, px(160.));
        assert_eq!(column.name, "id");
        assert_eq!(column.align, TextAlign::Right);
        assert_eq!(column.sort, None);
    }

    #[test]
    fn sortable_text_column_is_left_aligned() {
        // Given: a text result column.
        let info = info("name", false);
        // When: building its sortable Kit column.
        let column = column_for(&info, 1, true);
        // Then: it is 160 px wide, left aligned and sortable.
        assert_eq!(column.width, px(160.));
        assert_eq!(column.align, TextAlign::Left);
        assert_eq!(column.sort, Some(ColumnSort::Default));
    }

    struct Results {
        table: Entity<TableState<RowsDelegate>>,
    }

    impl Render for Results {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(DataTable::new(&self.table))
        }
    }

    #[gpui_kit::test]
    fn data_table_over_rows_shows_every_row(cx: &mut TestAppContext) {
        // Given: an initialized application and three result rows of every cell kind.
        cx.update(gpui_kit::init);
        let delegate = RowsDelegate {
            columns: vec![info("id", true), info("name", false), info("avatar", false)],
            rows: Arc::new(vec![
                vec![text("1"), text("ada"), Cell::Binary { len: 4 }],
                vec![text("2"), Cell::Null, Cell::Binary { len: 4 }],
                vec![text("3"), text(""), Cell::Null],
            ]),
        };
        let bounds = Bounds::new(Point::default(), size(px(640.), px(320.)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        // When: a 640×320 window renders them in a Kit DataTable.
        let (window, view) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    cx.new(|cx| Results {
                        table: cx.new(|cx| TableState::new(delegate, window, cx)),
                    })
                })
            })
            .expect("the window opens");
        // Then: the table and its last row are visible, and the table reports three rows.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("table").visible());
            assert!(window.find(("row", 2usize)).visible());
            assert_eq!(view.read(cx).table.read(cx).delegate().rows_count(cx), 3);
        })
        .expect("the window is still open");
    }

    #[gpui_kit::test]
    fn cell_text_copies_the_complete_value(cx: &mut TestAppContext) {
        // Given: a text cell longer than the grid shows, on two lines.
        let value = format!("{}\nsecond line", "x".repeat(600));
        let delegate = RowsDelegate {
            columns: vec![info("body", false)],
            rows: Arc::new(vec![vec![text(&value)]]),
        };
        // When: copying the cell.
        let copied = cx.update(|cx| delegate.cell_text(0, 0, cx));
        // Then: the copy holds the whole value.
        assert_eq!(copied, value);
    }

    #[gpui_kit::test]
    fn cell_text_copies_placeholders_as_shown(cx: &mut TestAppContext) {
        // Given: a row with a NULL and a binary cell.
        let row = vec![Cell::Null, Cell::Binary { len: 4 }];
        let delegate = RowsDelegate {
            columns: vec![info("name", false), info("avatar", false)],
            rows: Arc::new(vec![row.clone()]),
        };
        for (col_ix, cell) in row.iter().enumerate() {
            // When: copying the cell.
            let copied = cx.update(|cx| delegate.cell_text(0, col_ix, cx));
            // Then: the copy reads as the grid shows it.
            assert_eq!(copied, cell_display(cell, false).text, "{cell:?}");
        }
    }
}
