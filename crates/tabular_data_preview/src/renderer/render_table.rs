use crate::types::TableCell;
use gpui::{AnyElement, Entity, Hsla};
use std::ops::Range;
use ui::{
    ColumnWidthConfig, ResizableColumnsState, SharedString, Table, UncheckedTableRow, div,
    prelude::*,
};

use crate::{
    TableView,
    settings::RowRenderMechanism,
    types::{AnyColumn, DataCellId, DisplayCellId, DisplayRow},
};

impl TableView {
    pub(crate) fn create_table(
        &self,
        current_widths: &Entity<ResizableColumnsState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        let cols = current_widths.read(cx).cols();
        let mut headers = Vec::with_capacity(cols);

        headers.push(self.create_row_identifier_header(cx));

        for i in 0..(cols - 1) {
            let header_text = self
                .engine
                .contents
                .headers
                .get(AnyColumn(i))
                .and_then(|h| h.display_value().cloned())
                .unwrap_or_else(|| format!("Col {}", i + 1).into());

            headers.push(self.create_header_element_with_sort_button(
                header_text,
                cx,
                AnyColumn::from(i),
            ));
        }

        Table::new(cols)
            .interactable(&self.table_interaction_state)
            .width_config(ColumnWidthConfig::Resizable(current_widths.clone()))
            .header(headers)
            .disable_base_style()
            .pin_cols(1)
            .map(|table| {
                let row_identifier_text_color = cx.theme().colors().editor_line_number;
                match self.settings.rendering_with {
                    RowRenderMechanism::VariableList => {
                        table.variable_row_height_list(row_count, self.list_state.clone(), {
                            cx.processor(move |this, display_row: usize, _window, cx| {
                                this.performance_metrics.rendered_indices.push(display_row);
                                // The mapping may be transiently stale while a filter/sort task is in-flight.
                                // `ui::Table` requires exactly `cols` cells per row, so pad with empty cells.
                                Self::render_single_table_row(
                                    this,
                                    cols,
                                    DisplayRow(display_row),
                                    row_identifier_text_color,
                                    this.row_height,
                                    cx,
                                )
                                .unwrap_or_else(|| {
                                    (0..cols).map(|_| div().into_any_element()).collect()
                                })
                            })
                        })
                    }
                    RowRenderMechanism::UniformList => {
                        table.uniform_list("tabular-data-table", row_count, {
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                this.performance_metrics
                                    .rendered_indices
                                    .extend(range.clone());

                                let row_height = this.row_height;
                                range
                                    .filter_map(|display_index| {
                                        Self::render_single_table_row(
                                            this,
                                            cols,
                                            DisplayRow(display_index),
                                            row_identifier_text_color,
                                            row_height,
                                            cx,
                                        )
                                    })
                                    .collect()
                            })
                        })
                    }
                }
            })
            .into_any_element()
    }

    /// Render a single table row
    ///
    /// Used both by UniformList and VariableRowHeightList
    fn render_single_table_row(
        this: &TableView,
        cols: usize,
        display_row: DisplayRow,
        row_identifier_text_color: Hsla,
        row_height: Pixels,
        cx: &Context<TableView>,
    ) -> Option<UncheckedTableRow<AnyElement>> {
        // Get the actual row index from our sorted indices
        let data_row = this.engine.d2d_mapping().get_data_row(display_row)?;
        let row = this.engine.contents.get_row(data_row)?;

        let mut elements = Vec::with_capacity(cols);
        elements.push(this.create_row_identifier_cell(display_row, data_row, cx)?);
        let selection_range = this.selection_display_range();

        // Remaining columns: actual table data
        for col in (0..this.engine.contents.number_of_cols).map(AnyColumn) {
            let table_cell = row.get(col)?;
            let is_null = table_cell.is_null();
            let cell_content = if is_null {
                SharedString::new_static("NULL")
            } else {
                table_cell.display_value().cloned().unwrap_or_default()
            };

            let display_cell_id = DisplayCellId::new(display_row, col);
            let data_cell_id = DataCellId::new(data_row, col);

            let is_focus_cell = this
                .selection
                .as_ref()
                .is_some_and(|selection| selection.focus == data_cell_id);

            let cell_bg = if is_focus_cell {
                Some(cx.theme().colors().element_selected)
            } else if this.is_cell_selected(display_row, col, selection_range.as_ref()) {
                Some(cx.theme().colors().element_selection_background)
            } else if this.marked_cells.contains(&data_cell_id) {
                Some(cx.theme().status().modified_background)
            } else {
                None
            };

            let cell = div()
                .size_full()
                .when(
                    !this.settings.multiline_cells_effectively_enabled(),
                    |div| {
                        div.whitespace_nowrap()
                            .text_ellipsis()
                            .h(row_height)
                            .overflow_hidden()
                    },
                )
                .child(TableView::create_selectable_cell(
                    display_cell_id,
                    data_cell_id,
                    cell_content,
                    is_null,
                    this.settings.vertical_alignment,
                    cx,
                ));

            elements.push(
                div()
                    .size_full()
                    .when_some(cell_bg, |div, color| div.bg(color))
                    .when(this.settings.show_debug_info, |parent| {
                        parent.child(div().text_color(row_identifier_text_color).child(
                            match table_cell {
                                TableCell::Real { position: pos, .. } => {
                                    let slv = pos.start.timestamp().value;
                                    let so = pos.start.offset;
                                    let elv = pos.end.timestamp().value;
                                    let eo = pos.end.offset;
                                    format!("Pos {so}(L{slv})-{eo}(L{elv})")
                                }
                                TableCell::Virtual => "Virtual cell".into(),
                                TableCell::Generated(_) => "Generated cell".into(),
                                TableCell::Null => "Null cell".into(),
                            },
                        ))
                    })
                    .child(cell)
                    .into_any_element(),
            );
        }

        Some(elements)
    }
}
