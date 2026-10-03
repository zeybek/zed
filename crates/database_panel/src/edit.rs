//! Editing cells of query results.

use gpui::{App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString};
use ui::{
    Button, ButtonStyle, Label, LabelSize, Modal, ModalFooter, ModalHeader, Section, Switch,
    ToggleState, prelude::*,
};
use ui_input::InputField;
use workspace::ModalView;

type OnApply = Box<dyn FnOnce(Option<String>, &mut Window, &mut App)>;

/// Edits one value. The value is applied to the result as a pending change, which is written
/// to the database when the result is committed.
pub struct EditCellModal {
    column: SharedString,
    column_type: SharedString,
    input: Entity<InputField>,
    is_null: bool,
    on_apply: Option<OnApply>,
}

impl EventEmitter<DismissEvent> for EditCellModal {}

impl ModalView for EditCellModal {}

impl Focusable for EditCellModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl EditCellModal {
    pub fn new(
        column: SharedString,
        column_type: SharedString,
        value: Option<SharedString>,
        on_apply: OnApply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputField::new(window, cx, "Value").tab_stop(true));
        if let Some(value) = &value {
            input.update(cx, |input, cx| input.set_text(value, window, cx));
        }
        Self {
            column,
            column_type,
            input,
            is_null: value.is_none(),
            on_apply: Some(on_apply),
        }
    }

    fn apply(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let value = if self.is_null {
            None
        } else {
            Some(self.input.read(cx).text(cx))
        };
        if let Some(on_apply) = self.on_apply.take() {
            on_apply(value, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for EditCellModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("DatabaseEditCellModal")
            .elevation_3(cx)
            .w(rems(30.))
            .on_action(cx.listener(Self::apply))
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("database-edit-cell", None)
                    .header(
                        ModalHeader::new()
                            .headline(format!("Edit {}", self.column))
                            .description(self.column_type.clone()),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .when(self.is_null, |div| div.opacity(0.5))
                                        .child(self.input.clone()),
                                )
                                .child(
                                    Switch::new("set-null", ToggleState::from(self.is_null))
                                        .label("NULL")
                                        .on_click(cx.listener(
                                            |this, state: &ToggleState, _, cx| {
                                                this.is_null = state.selected();
                                                cx.notify();
                                            },
                                        )),
                                )
                                .child(
                                    Label::new(
                                        "Changes are written to the database when you commit them.",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(Button::new("cancel", "Cancel").on_click(cx.listener(
                                    |this, _, window, cx| this.cancel(&menu::Cancel, window, cx),
                                )))
                                .child(
                                    Button::new("apply", "Apply")
                                        .style(ButtonStyle::Filled)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.apply(&menu::Confirm, window, cx)
                                        })),
                                ),
                        ),
                    ),
            )
    }
}
