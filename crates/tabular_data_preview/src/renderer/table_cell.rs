//! Table Cell Rendering

use gpui::{
    AnyElement, ClickEvent, ClipboardItem, ElementId, MouseButton, StatefulInteractiveElement,
};
use ui::{Color, Divider, Label, LabelSize, SharedString, Tooltip, div, prelude::*};

use crate::{
    TableView, TableViewEvent,
    settings::VerticalAlignment,
    types::{DataCellId, DisplayCellId},
};

/// Adds a right-click-to-copy handler and a tooltip showing `text` plus `hint`
/// (e.g. "Right click to copy content") to a `Stateful` element.
pub(crate) fn with_copy_on_right_click<E: StatefulInteractiveElement>(
    element: E,
    text: SharedString,
    hint: &'static str,
) -> E {
    element
        .on_mouse_down(MouseButton::Right, {
            let text_to_copy = text.clone();
            move |_event, _window, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(text_to_copy.to_string()));
            }
        })
        .tooltip(Tooltip::element(move |_window, cx| {
            v_flex()
                .gap_1()
                .child(div().font_buffer(cx).child(text.clone()))
                .child(Divider::horizontal())
                .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        }))
}

impl TableView {
    /// Create selectable table cell with mouse event handlers.
    ///
    /// A click selects the cell, a shift-click extends the selection and a double-click
    /// activates the cell.
    pub fn create_selectable_cell(
        display_cell_id: DisplayCellId,
        data_cell_id: DataCellId,
        cell_content: SharedString,
        is_null: bool,
        vertical_alignment: VerticalAlignment,
        cx: &Context<TableView>,
    ) -> AnyElement {
        create_table_cell(
            display_cell_id,
            cell_content,
            is_null,
            vertical_alignment,
            cx,
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.select_cell_with_mouse(data_cell_id, event.modifiers().shift, window, cx);
            if event.click_count() >= 2 {
                cx.emit(TableViewEvent::CellActivated(data_cell_id));
            }
        }))
        .into_any_element()
    }
}

/// Create styled table cell div element.
fn create_table_cell(
    display_cell_id: DisplayCellId,
    cell_content: SharedString,
    is_null: bool,
    vertical_alignment: VerticalAlignment,
    cx: &Context<'_, TableView>,
) -> gpui::Stateful<Div> {
    let cell = div()
        .id(ElementId::NamedInteger(
            format!("table-display-cell-{}", *display_cell_id.row).into(),
            *display_cell_id.col as u64,
        ))
        .flex()
        .h_full()
        .px_1()
        .border_color(cx.theme().colors().border_variant)
        .map(|div| match vertical_alignment {
            VerticalAlignment::Top => div.items_start(),
            VerticalAlignment::Center => div.items_center(),
        })
        .font_buffer(cx)
        .when(is_null, |div| {
            div.italic().text_color(cx.theme().colors().text_muted)
        });
    // Copying a NULL yields an empty string, consistent with copying a selection.
    let copied_text = if is_null {
        SharedString::default()
    } else {
        cell_content.clone()
    };
    with_copy_on_right_click(cell, copied_text, "Right click to copy content")
        .child(div().child(cell_content))
}
