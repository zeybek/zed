//! Completions for table and column names, merged with those of language servers.

use std::{cell::RefCell, ops::Range, rc::Rc};

use anyhow::Result;
use database_core::{ConnectionConfig, DbStore, RelationKind};
use editor::{CompletionProvider, Editor};
use gpui::{App, AppContext as _, Context, Entity, Task, TaskExt as _, Window};
use language::{Buffer, BufferEvent, CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionGroup, CompletionResponse, CompletionSource,
    Project, lsp_store::CompletionDocumentation,
};

/// Wraps the project's completion provider, adding names from the database schema of the
/// editor's connection. Replacing the provider outright would silently drop language server
/// completions.
pub struct SqlSchemaCompletionProvider {
    project: Entity<Project>,
    inner: Rc<dyn CompletionProvider>,
}

impl SqlSchemaCompletionProvider {
    fn new(project: Entity<Project>) -> Self {
        Self {
            inner: Rc::new(project.clone()),
            project,
        }
    }
}

/// Installs the provider on SQL editors, and removes it when the buffer's language changes to
/// something else.
pub fn register(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    update_provider(editor, &project, cx);
    cx.subscribe(&buffer, move |editor, _, event, cx| {
        if let BufferEvent::LanguageChanged(_) | BufferEvent::FileHandleChanged = event {
            update_provider(editor, &project, cx);
        }
    })
    .detach();
}

fn update_provider(editor: &mut Editor, project: &Entity<Project>, cx: &mut Context<Editor>) {
    let is_sql = crate::sql_editor::is_sql_editor(editor, cx);
    let provider: Rc<dyn CompletionProvider> = if is_sql {
        Rc::new(SqlSchemaCompletionProvider::new(project.clone()))
    } else {
        Rc::new(project.clone())
    };
    editor.set_completion_provider(Some(provider));
}

/// What precedes the word being completed.
#[derive(Debug, PartialEq, Eq)]
struct CompletionTarget {
    /// The identifier before a `.`, such as a schema or table name.
    qualifier: Option<String>,
    /// The partial word, used to compute the range to replace.
    word: Range<usize>,
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Finds the word ending at `offset` and an optional `qualifier.` before it. Quoted qualifiers
/// like `"My Schema".` are unquoted.
fn completion_target(text: &str, offset: usize) -> CompletionTarget {
    let before = &text[..offset];
    let word_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_identifier_char(*c))
        .last()
        .map_or(offset, |(index, _)| index);
    let prefix = &before[..word_start];
    let qualifier = prefix.strip_suffix('.').and_then(|prefix| {
        if let Some(quoted) = prefix.strip_suffix(['"', '`']) {
            let quote = prefix.chars().last()?;
            let start = quoted.rfind(quote)?;
            return Some(quoted[start + 1..].to_string());
        }
        let start = prefix
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_identifier_char(*c))
            .last()?
            .0;
        Some(prefix[start..].to_string())
    });
    CompletionTarget {
        qualifier,
        word: word_start..offset,
    }
}

struct Candidate {
    name: String,
    detail: String,
    group: &'static str,
}

/// Names that complete `target` from the cached schema, and what still needs loading.
fn candidates(
    config: &ConnectionConfig,
    target: &CompletionTarget,
    cx: &mut App,
) -> (Vec<Candidate>, bool) {
    let store = DbStore::global(cx);
    let Some(schemas) = store.read(cx).schemas(&config.key).map(<[_]>::to_vec) else {
        load(&store, config, None, None, cx);
        return (Vec::new(), true);
    };
    let mut items = Vec::new();
    let mut incomplete = false;
    let matches = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    if let Some(qualifier) = &target.qualifier {
        // `schema.` completes relations, `table.` completes columns.
        if let Some(schema) = schemas
            .iter()
            .find(|schema| matches(&schema.name, qualifier))
        {
            match store.read(cx).relations(&config.key, &schema.name) {
                Some(relations) => items.extend(relations.iter().map(|relation| Candidate {
                    name: relation.name.to_string(),
                    detail: relation_detail(relation.kind).into(),
                    group: "Tables",
                })),
                None => {
                    load(&store, config, Some(schema.name.to_string()), None, cx);
                    incomplete = true;
                }
            }
            return (items, incomplete);
        }
        for schema in &schemas {
            let relation = store
                .read(cx)
                .relations(&config.key, &schema.name)
                .and_then(|relations| {
                    relations
                        .iter()
                        .find(|relation| matches(&relation.name, qualifier))
                        .cloned()
                });
            let Some(relation) = relation else {
                continue;
            };
            match store
                .read(cx)
                .columns(&config.key, &schema.name, &relation.name)
            {
                Some(columns) => items.extend(columns.iter().map(|column| Candidate {
                    name: column.name.to_string(),
                    detail: column.data_type.to_string(),
                    group: "Columns",
                })),
                None => {
                    load(
                        &store,
                        config,
                        Some(schema.name.to_string()),
                        Some(relation.name.to_string()),
                        cx,
                    );
                    incomplete = true;
                }
            }
            return (items, incomplete);
        }
        return (items, false);
    }

    // Unqualified names: relations of the first (default) schema, other schema names, and the
    // columns of relations whose columns were already loaded.
    for (index, schema) in schemas.iter().enumerate() {
        if index > 0 {
            items.push(Candidate {
                name: schema.name.to_string(),
                detail: "schema".into(),
                group: "Schemas",
            });
        }
        match store.read(cx).relations(&config.key, &schema.name) {
            Some(relations) => {
                for relation in relations {
                    if index == 0 {
                        items.push(Candidate {
                            name: relation.name.to_string(),
                            detail: relation_detail(relation.kind).into(),
                            group: "Tables",
                        });
                    }
                    if let Some(columns) =
                        store
                            .read(cx)
                            .columns(&config.key, &schema.name, &relation.name)
                    {
                        items.extend(columns.iter().map(|column| Candidate {
                            name: column.name.to_string(),
                            detail: format!("{}.{}", relation.name, column.data_type),
                            group: "Columns",
                        }));
                    }
                }
            }
            None if index == 0 => {
                load(&store, config, Some(schema.name.to_string()), None, cx);
                incomplete = true;
            }
            None => {}
        }
    }
    (items, incomplete)
}

fn relation_detail(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
    }
}

fn load(
    store: &Entity<DbStore>,
    config: &ConnectionConfig,
    schema: Option<String>,
    relation: Option<String>,
    cx: &mut App,
) {
    // Completions never prompt for a password or connect on their own; they use connections that
    // are already open.
    if !store.read(cx).is_connected(&config.key) {
        return;
    }
    let config = config.clone();
    store.update(cx, |store, cx| {
        let task = match (schema, relation) {
            (None, _) if store.is_loading_schemas(&config.key) => return,
            (None, _) => store.load_schemas(config, None, cx),
            (Some(schema), None) => {
                if store.is_loading_relations(&config.key, &schema) {
                    return;
                }
                store.load_relations(config, None, schema.into(), cx)
            }
            (Some(schema), Some(relation)) => {
                if store.is_loading_columns(&config.key, &schema, &relation) {
                    return;
                }
                store.load_columns(config, None, schema.into(), relation.into(), cx)
            }
        };
        task.detach_and_log_err(cx);
    });
}

impl CompletionProvider for SqlSchemaCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: text::Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let inner = self
            .inner
            .completions(buffer, buffer_position, trigger, window, cx);
        // The editor is being updated, so it can't be read here.
        let schema_response = crate::sql_editor::connection_for_buffer(
            &self.project,
            cx.entity_id(),
            Some(buffer),
            cx,
        )
        .map(|config| {
            let snapshot = buffer.read(cx).snapshot();
            let offset = buffer_position.to_offset(&snapshot);
            let text = snapshot.text_for_range(0..offset).collect::<String>();
            let target = completion_target(&text, offset);
            let (items, is_incomplete) = candidates(&config, &target, cx);
            let replace_range =
                snapshot.anchor_before(target.word.start)..snapshot.anchor_after(target.word.end);
            let completions = items
                .into_iter()
                .map(|candidate| Completion {
                    replace_range: replace_range.clone(),
                    new_text: quote_if_needed(config.driver, &candidate.name),
                    label: CodeLabel::plain(candidate.name, None),
                    documentation: Some(CompletionDocumentation::SingleLine(
                        candidate.detail.into(),
                    )),
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: None,
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: Some(CompletionGroup {
                        key: candidate.group.into(),
                        label: Some(candidate.group.into()),
                    }),
                })
                .collect::<Vec<_>>();
            CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete,
            }
        });
        cx.background_spawn(async move {
            let mut responses = inner.await.unwrap_or_else(|error| {
                log::debug!("language server completions failed: {error:#}");
                Vec::new()
            });
            responses.extend(schema_response);
            Ok(responses)
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        // The project resolves only items from language servers and skips custom ones.
        self.inner
            .resolve_completions(buffer, completion_indices, completions, cx)
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        self.inner.apply_additional_edits_for_completion(
            buffer,
            completions,
            completion_index,
            push_to_history,
            all_commit_ranges,
            cx,
        )
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        text == "."
            || self
                .inner
                .is_completion_trigger(buffer, position, text, trigger_in_words, cx)
    }

    fn show_snippets(&self) -> bool {
        self.inner.show_snippets()
    }
}

fn quote_if_needed(driver: database_core::DriverKind, name: &str) -> String {
    database_core::quote_identifier(driver, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_target() {
        let target = |text: &str| completion_target(text, text.len());
        assert_eq!(
            target("select * from pub"),
            CompletionTarget {
                qualifier: None,
                word: 14..17
            }
        );
        assert_eq!(
            target("select u.na"),
            CompletionTarget {
                qualifier: Some("u".into()),
                word: 9..11
            }
        );
        assert_eq!(
            target("select * from public."),
            CompletionTarget {
                qualifier: Some("public".into()),
                word: 21..21
            }
        );
        assert_eq!(
            target("select * from \"My Schema\".ta"),
            CompletionTarget {
                qualifier: Some("My Schema".into()),
                word: 26..28
            }
        );
    }
}
