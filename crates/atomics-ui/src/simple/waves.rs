//! Large boards open on the first wave in flight. A wave is one layer of the plan's dependency graph.
use std::collections::{HashMap, HashSet, VecDeque};

use super::*;
use atomics_core::plan::{Plan, TaskStatus};

#[derive(Clone, Default)]
struct Selection {
    wave: Option<usize>,
    query: String,
}

/// Derive dependency depth in plan order in O(tasks + edges). Missing or cyclic prerequisites
/// stay accessible under Unscheduled, including any tasks that depend on them.
fn depths(plan: &Plan) -> Vec<Option<usize>> {
    let indices: HashMap<_, _> = plan.tasks.iter().enumerate().map(|(i, t)| (t.id.as_str(), i)).collect();
    let mut pending = Vec::with_capacity(plan.tasks.len());
    let mut children = vec![Vec::new(); plan.tasks.len()];
    let mut ready = VecDeque::new();
    for (i, task) in plan.tasks.iter().enumerate() {
        pending.push(task.depends_on.len());
        if task.depends_on.is_empty() {
            ready.push_back(i);
        }
        for dep in &task.depends_on {
            if let Some(&index) = indices.get(dep.as_str()) {
                children[index].push(i);
            }
        }
    }
    let mut depth = vec![0; pending.len()];
    let mut waves = vec![None; pending.len()];
    while let Some(i) = ready.pop_front() {
        waves[i] = Some(depth[i]);
        for &child in &children[i] {
            depth[child] = depth[child].max(depth[i] + 1);
            pending[child] -= 1;
            if pending[child] == 0 {
                ready.push_back(child);
            }
        }
    }
    waves
}

pub(super) fn filter<'a>(ui: &mut egui::Ui, plan: &'a Plan, client: &Client) -> Option<HashSet<&'a str>> {
    if plan.tasks.len() < 16 {
        return None;
    }
    let groups = depths(plan);
    let waves = groups.iter().flatten().max().map_or(0, |last| last + 1);
    let active = plan
        .tasks
        .iter()
        .zip(&groups)
        .filter(|(t, _)| matches!(t.status, TaskStatus::Running | TaskStatus::Review | TaskStatus::Blocked))
        .filter_map(|(_, g)| *g)
        .min()
        .or_else(|| {
            plan.tasks.iter().zip(&groups).filter(|(t, _)| t.status != TaskStatus::Done).filter_map(|(_, g)| *g).min()
        })
        .unwrap_or(waves.saturating_sub(1));
    let total = waves + usize::from(groups.contains(&None));
    let id = egui::Id::NULL.with(("simple-wave-selection", &plan.id));
    let mut state = ui.ctx().data(|d| d.get_temp::<Selection>(id)).unwrap_or_default();
    let mut selected = state.wave.unwrap_or(active).min(total.saturating_sub(1));
    let label = |i| if i == waves { "Unscheduled".to_owned() } else { format!("Wave {} of {waves}", i + 1) };
    ui.horizontal(|ui| {
        ui.menu_button(RichText::new(label(selected)).font(font(13.0)), |ui| {
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                if ui.selectable_label(state.wave.is_none(), "Follow active wave").clicked() {
                    selected = active;
                    state.wave = None;
                    state.query.clear();
                    ui.close();
                }
                for i in 0..total {
                    if ui.selectable_label(i == selected, label(i)).clicked() {
                        selected = i;
                        state.wave = Some(i);
                        state.query.clear();
                        ui.close();
                    }
                }
            });
        })
        .response
        .on_hover_text("Tasks that can run alongside one another");
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.query)
                .hint_text("Find a task…")
                .desired_width(ui.available_width().min(220.0))
                .font(font(13.0)),
        );
        touch::opens_keyboard(ui, &response);
    });
    let query = state.query.trim().to_lowercase();
    let filtered: HashSet<_> = plan
        .tasks
        .iter()
        .zip(groups)
        .filter(|(task, group)| {
            if query.is_empty() {
                group.unwrap_or(waves) == selected
            } else {
                [&task.title, &task.id, &task.agent].iter().any(|text| text.to_lowercase().contains(&query))
            }
        })
        .map(|(task, _)| task.id.as_str())
        .collect();
    if !query.is_empty() {
        ui.label(
            RichText::new(format!("{} matches across all waves", filtered.len()))
                .font(font(12.0))
                .color(client.palette.muted),
        );
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, state));
    ui.add_space(6.0);
    Some(filtered)
}
