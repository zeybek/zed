//! Execution plans shown as a tree.

use std::ops::Range;

use anyhow::{Context as _, Result};
use database_core::{CollectedResult, ConnectionConfig, DbStore, DriverKind, statement};
use gpui::{
    App, EventEmitter, FocusHandle, Focusable, SharedString, UniformListScrollHandle, WeakEntity,
    uniform_list,
};
use project::Project;
use serde_json::Value;
use ui::{
    Color, IconButton, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, Tooltip,
    prelude::*,
};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::{connection_modal::connect_interactively, results::open_text_in_editor};

/// A node of an execution plan.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanNode {
    pub depth: usize,
    pub title: String,
    pub detail: String,
}

/// Runs `EXPLAIN` for `sql` and opens its plan next to the active pane.
pub fn show_explain(
    config: ConnectionConfig,
    project: gpui::Entity<Project>,
    sql: String,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let connect = connect_interactively(
        cx.weak_entity(),
        config.clone(),
        project.clone(),
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        connect.await?;
        // EXPLAIN without ANALYZE doesn't run the statement; the read-only transaction makes sure
        // of it anyway.
        let result = cx
            .update(|_, cx| {
                DbStore::global(cx).update(cx, |store, cx| {
                    store.execute_for_agent(
                        config.clone(),
                        Some(project.clone()),
                        sql.clone(),
                        10_000,
                        4_000_000,
                        cx,
                    )
                })
            })?
            .await?;
        let (nodes, raw) = plan_nodes(config.driver, &result)?;
        workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = cx.weak_entity();
            let item = cx.new(|cx| ExplainItem {
                workspace: workspace_handle,
                project,
                title: statement::summary(
                    sql.trim_start_matches(|c: char| !c.is_whitespace()).trim(),
                    32,
                )
                .into(),
                nodes,
                raw,
                focus_handle: cx.focus_handle(),
                scroll_handle: UniformListScrollHandle::new(),
            });
            workspace.add_item_to_center(Box::new(item), window, cx);
        })?;
        telemetry::event!("Database Plan Explained", driver = config.driver.id());
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Turns the result of `EXPLAIN` into plan nodes, and the raw plan text.
pub fn plan_nodes(driver: DriverKind, result: &CollectedResult) -> Result<(Vec<PlanNode>, String)> {
    match driver {
        DriverKind::Postgres | DriverKind::Mysql => {
            let raw = result
                .rows
                .iter()
                .filter_map(|row| row.first().cloned().flatten())
                .collect::<Vec<_>>()
                .join("\n");
            let json: Value = serde_json::from_str(&raw).context("parsing the execution plan")?;
            let mut nodes = Vec::new();
            if driver == DriverKind::Postgres {
                let plans = json
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![json.clone()]);
                for plan in &plans {
                    if let Some(plan) = plan.get("Plan") {
                        postgres_nodes(plan, 0, &mut nodes);
                    }
                }
            } else {
                json_nodes("query", &json, 0, &mut nodes);
            }
            let pretty = serde_json::to_string_pretty(&json).unwrap_or(raw);
            Ok((nodes, pretty))
        }
        DriverKind::Sqlite => {
            // EXPLAIN QUERY PLAN returns (id, parent, notused, detail) rows.
            let rows = result
                .rows
                .iter()
                .filter_map(|row| {
                    let id = row.first()?.as_ref()?.parse::<i64>().ok()?;
                    let parent = row.get(1)?.as_ref()?.parse::<i64>().ok()?;
                    let detail = row.get(3)?.clone()?;
                    Some((id, parent, detail))
                })
                .collect::<Vec<_>>();
            let mut nodes = Vec::new();
            sqlite_nodes(&rows, 0, 0, &mut nodes);
            let raw = rows
                .iter()
                .map(|(id, parent, detail)| format!("{id}\t{parent}\t{detail}"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((nodes, raw))
        }
    }
}

fn postgres_nodes(plan: &Value, depth: usize, nodes: &mut Vec<PlanNode>) {
    let text = |key: &str| plan.get(key).and_then(Value::as_str);
    let number = |key: &str| plan.get(key).and_then(Value::as_f64);
    let mut title = text("Node Type").unwrap_or("?").to_string();
    if let Some(relation) = text("Relation Name") {
        title.push_str(&format!(" on {relation}"));
        if let Some(alias) = text("Alias").filter(|alias| *alias != relation) {
            title.push_str(&format!(" {alias}"));
        }
    }
    if let Some(index) = text("Index Name") {
        title.push_str(&format!(" using {index}"));
    }
    let mut detail = Vec::new();
    if let (Some(startup), Some(total)) = (number("Startup Cost"), number("Total Cost")) {
        detail.push(format!("cost={startup:.2}..{total:.2}"));
    }
    if let Some(rows) = number("Plan Rows") {
        detail.push(format!("rows={rows}"));
    }
    if let Some(time) = number("Actual Total Time") {
        detail.push(format!("actual time={time:.3} ms"));
    }
    if let Some(rows) = number("Actual Rows") {
        detail.push(format!("actual rows={rows}"));
    }
    for key in [
        "Filter",
        "Index Cond",
        "Hash Cond",
        "Join Filter",
        "Sort Key",
    ] {
        if let Some(value) = plan.get(key) {
            let value = match value {
                Value::String(value) => value.clone(),
                other => other.to_string(),
            };
            detail.push(format!("{key}: {value}"));
        }
    }
    nodes.push(PlanNode {
        depth,
        title,
        detail: detail.join("  "),
    });
    for child in plan
        .get("Plans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        postgres_nodes(child, depth + 1, nodes);
    }
}

fn json_nodes(name: &str, value: &Value, depth: usize, nodes: &mut Vec<PlanNode>) {
    match value {
        Value::Object(object) => {
            let scalars = object
                .iter()
                .filter(|(_, value)| !value.is_object() && !value.is_array())
                .map(|(key, value)| match value {
                    Value::String(value) => format!("{key}={value}"),
                    other => format!("{key}={other}"),
                })
                .collect::<Vec<_>>();
            nodes.push(PlanNode {
                depth,
                title: name.to_string(),
                detail: scalars.join("  "),
            });
            for (key, child) in object {
                if child.is_object() || child.is_array() {
                    json_nodes(key, child, depth + 1, nodes);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                json_nodes(&format!("{name}[{index}]"), item, depth, nodes);
            }
        }
        other => nodes.push(PlanNode {
            depth,
            title: name.to_string(),
            detail: other.to_string(),
        }),
    }
}

fn sqlite_nodes(rows: &[(i64, i64, String)], parent: i64, depth: usize, nodes: &mut Vec<PlanNode>) {
    for (id, row_parent, detail) in rows {
        if *row_parent == parent && *id != parent {
            nodes.push(PlanNode {
                depth,
                title: detail.clone(),
                detail: String::new(),
            });
            sqlite_nodes(rows, *id, depth + 1, nodes);
        }
    }
}

pub struct ExplainItem {
    workspace: WeakEntity<Workspace>,
    project: gpui::Entity<Project>,
    title: SharedString,
    nodes: Vec<PlanNode>,
    raw: String,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
}

impl EventEmitter<ItemEvent> for ExplainItem {}

impl Focusable for ExplainItem {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplainItem {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new("Execution Plan").size(LabelSize::Small))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("open-raw-plan", IconName::FileCode)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open the Raw Plan"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                open_text_in_editor(
                                    this.workspace.clone(),
                                    this.project.clone(),
                                    this.raw.clone(),
                                    "JSON",
                                    None,
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .child(
                uniform_list(
                    "execution-plan",
                    self.nodes.len(),
                    cx.processor(|this, range: Range<usize>, _window, _cx| {
                        range
                            .filter_map(|index| {
                                let node = this.nodes.get(index)?;
                                Some(
                                    ListItem::new(index)
                                        .spacing(ListItemSpacing::Sparse)
                                        .indent_level(node.depth)
                                        .indent_step_size(px(16.))
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .child(
                                                    Label::new(node.title.clone())
                                                        .size(LabelSize::Small),
                                                )
                                                .child(
                                                    Label::new(node.detail.clone())
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted)
                                                        .truncate(),
                                                ),
                                        )
                                        .into_any_element(),
                                )
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&self.scroll_handle),
            )
    }
}

impl Item for ExplainItem {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Plan: {}", self.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::ListTree))
    }

    fn show_toolbar(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_postgres_plan() {
        let result = CollectedResult {
            columns: vec!["QUERY PLAN".into()],
            rows: vec![vec![Some(
                r#"[{"Plan": {"Node Type": "Hash Join", "Startup Cost": 1.0, "Total Cost": 2.5, "Plan Rows": 10, "Hash Cond": "(a.id = b.a_id)",
                    "Plans": [{"Node Type": "Seq Scan", "Relation Name": "a", "Alias": "a", "Startup Cost": 0.0, "Total Cost": 1.0, "Plan Rows": 5},
                              {"Node Type": "Index Scan", "Relation Name": "b", "Alias": "x", "Index Name": "b_pkey", "Startup Cost": 0.0, "Total Cost": 1.2, "Plan Rows": 3}]}}]"#
                    .into(),
            )]],
            truncated: false,
            rows_affected: None,
        };
        let (nodes, raw) = plan_nodes(DriverKind::Postgres, &result).unwrap();
        assert_eq!(
            nodes
                .iter()
                .map(|node| (node.depth, node.title.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "Hash Join"),
                (1, "Seq Scan on a"),
                (1, "Index Scan on b x using b_pkey")
            ]
        );
        assert!(nodes[0].detail.contains("cost=1.00..2.50"));
        assert!(nodes[0].detail.contains("Hash Cond: (a.id = b.a_id)"));
        assert!(raw.contains("\"Node Type\""));
    }

    #[test]
    fn test_sqlite_plan() {
        let row = |id: &str, parent: &str, detail: &str| {
            vec![
                Some(id.to_string()),
                Some(parent.to_string()),
                Some("0".into()),
                Some(detail.to_string()),
            ]
        };
        let result = CollectedResult {
            columns: vec![
                "id".into(),
                "parent".into(),
                "notused".into(),
                "detail".into(),
            ],
            rows: vec![
                row("2", "0", "SCAN users"),
                row(
                    "5",
                    "0",
                    "SEARCH orders USING INDEX orders_user (user_id=?)",
                ),
                row("7", "5", "CORRELATED SCALAR SUBQUERY 1"),
            ],
            truncated: false,
            rows_affected: None,
        };
        let (nodes, _) = plan_nodes(DriverKind::Sqlite, &result).unwrap();
        assert_eq!(
            nodes
                .iter()
                .map(|node| (node.depth, node.title.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "SCAN users"),
                (0, "SEARCH orders USING INDEX orders_user (user_id=?)"),
                (1, "CORRELATED SCALAR SUBQUERY 1"),
            ]
        );
    }
}
