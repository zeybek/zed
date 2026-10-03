//! Recently executed queries, to run again or copy into an editor.

use std::sync::Arc;

use database_core::{
    ConnectionConfig, DbStore, DbStoreEvent, HistoryEntry, QuerySource, statement,
};
use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate};
use project::Project;
use ui::{Color, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::results::{QueryRequest, ResultOrigin, open_text_in_editor, run_query};

pub fn init(_cx: &mut App) {}

pub struct HistoryPicker {
    picker: Entity<Picker<HistoryPickerDelegate>>,
    _subscription: Subscription,
}

impl HistoryPicker {
    /// Shows the history of the editor's connection.
    pub fn toggle(
        editor: &Entity<Editor>,
        config: ConnectionConfig,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = editor.read(cx).workspace() else {
            return;
        };
        let Some(project) = editor.read(cx).project().cloned() else {
            return;
        };
        let editor = editor.downgrade();
        workspace.update(cx, |workspace, cx| {
            Self::toggle_in_workspace(workspace, config, project, Some(editor), window, cx)
        });
    }

    pub fn toggle_in_workspace(
        workspace: &mut Workspace,
        config: ConnectionConfig,
        project: Entity<Project>,
        editor: Option<WeakEntity<Editor>>,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        let workspace_handle = cx.weak_entity();
        let store = DbStore::global(cx);
        store.update(cx, |store, cx| store.load_history(&config.key, cx));
        workspace.toggle_modal(window, cx, move |window, cx| {
            let this = cx.entity().downgrade();
            let key = config.key.clone();
            let delegate = HistoryPickerDelegate {
                this,
                workspace: workspace_handle,
                project,
                editor,
                config,
                entries: Vec::new(),
                matches: Vec::new(),
                selected_index: 0,
            };
            let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
            let subscription = cx.subscribe_in(&store, window, {
                let picker = picker.clone();
                move |_, _, event: &DbStoreEvent, window, cx| {
                    if *event == DbStoreEvent::HistoryChanged(key.clone()) {
                        picker.update(cx, |picker, cx| picker.refresh(window, cx));
                    }
                }
            });
            Self {
                picker,
                _subscription: subscription,
            }
        });
    }
}

impl EventEmitter<DismissEvent> for HistoryPicker {}

impl ModalView for HistoryPicker {}

impl Focusable for HistoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for HistoryPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex().w(rems(40.)).child(self.picker.clone())
    }
}

pub struct HistoryPickerDelegate {
    this: WeakEntity<HistoryPicker>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    editor: Option<WeakEntity<Editor>>,
    config: ConnectionConfig,
    entries: Vec<HistoryEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl HistoryPickerDelegate {
    fn selected_sql(&self) -> Option<String> {
        let candidate = self.matches.get(self.selected_index)?;
        Some(self.entries.get(candidate.candidate_id)?.sql.clone())
    }
}

impl PickerDelegate for HistoryPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "database query history"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut gpui::Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!("Search queries run on {}…", self.config.key.id).into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No queries yet".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut gpui::Context<Picker<Self>>,
    ) -> Task<()> {
        self.entries = DbStore::global(cx)
            .read(cx)
            .history(&self.config.key)
            .cloned()
            .collect();
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.sql))
            .collect::<Vec<_>>();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        score: 0.,
                        positions: Vec::new(),
                        string: candidate.string.clone(),
                    })
                    .collect()
            } else {
                let mut matches = fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    executor,
                )
                .await;
                // Keep the most recent queries first among equally good matches.
                matches.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.candidate_id.cmp(&b.candidate_id))
                });
                matches
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(
        &mut self,
        secondary: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Picker<Self>>,
    ) {
        let Some(sql) = self.selected_sql() else {
            return;
        };
        if secondary {
            // Paste into the SQL editor the history was opened from, or a new one.
            if let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) {
                editor.update(cx, |editor, cx| editor.insert(&sql, window, cx));
            } else {
                open_text_in_editor(
                    self.workspace.clone(),
                    self.project.clone(),
                    sql,
                    "SQL",
                    Some(self.config.key.clone()),
                    window,
                    cx,
                );
            }
        } else {
            let origin = self
                .editor
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .map_or(ResultOrigin::Other, |editor| {
                    ResultOrigin::Editor(editor.entity_id())
                });
            let request = QueryRequest {
                config: self.config.clone(),
                project: self.project.clone(),
                sql,
                origin,
                source: QuerySource::Panel,
                focus: true,
            };
            self.workspace
                .update(cx, |workspace, cx| {
                    run_query(workspace, request, window, cx)
                })
                .ok();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut gpui::Context<Picker<Self>>) {
        self.this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut gpui::Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.matches.get(index)?;
        let entry = self.entries.get(candidate.candidate_id)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .w_full()
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_buffer(_cx)
                                .child(statement::summary(&entry.sql, 120)),
                        )
                        .child(
                            Label::new(relative_time(entry.executed_at))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut gpui::Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        Some(
            h_flex()
                .p_2()
                .gap_3()
                .justify_end()
                .child(
                    Label::new("Enter: run again")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Secondary enter: copy to editor")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }
}

fn relative_time(executed_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let seconds = (now - executed_at).max(0);
    match seconds {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relative_time() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_time(now), "just now");
        assert_eq!(relative_time(now - 120), "2 min ago");
        assert_eq!(relative_time(now - 7_200), "2 h ago");
        assert_eq!(relative_time(now - 3 * 86_400), "3 d ago");
    }
}
