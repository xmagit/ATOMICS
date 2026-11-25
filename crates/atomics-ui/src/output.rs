//! What a board or a chat produced, as a line of buttons: its pull requests and links, each opened in the browser.

use atomics_core::output::Output;
use egui::{RichText, Stroke, vec2};

use crate::theme::Palette;

/// How many show before the rest fold into one menu.
const SHOWN: usize = 6;

/// The line; nothing where there is none. The latest come first.
pub fn strip(ui: &mut egui::Ui, outputs: &[Output], p: &Palette) {
    if outputs.is_empty() {
        return;
    }
    let mut latest: Vec<&Output> = outputs.iter().collect();
    latest.sort_by_key(|output| std::cmp::Reverse(output.at));
    let response = ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
        ui.label(RichText::new("Output").size(12.0).color(p.simple_text(p.muted, 7.2)));
        for output in latest.iter().take(SHOWN) {
            chip(ui, output, p);
        }
        if latest.len() > SHOWN {
            ui.menu_button(format!("{} more", latest.len() - SHOWN), |ui| {
                for output in &latest[SHOWN..] {
                    if ui.button(output.name()).on_hover_text(about(output)).clicked() {
                        open(ui, output);
                        ui.close();
                    }
                }
            });
        }
    });
    crate::flow::record(ui, "output", response.response.rect);
}

fn chip(ui: &mut egui::Ui, output: &Output, p: &Palette) {
    let name = crate::text::truncate(&output.name(), 40);
    let button = egui::Button::new(RichText::new(name).size(12.0).color(p.foreground))
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(6);
    let response = ui.add(button).on_hover_text(about(output));
    if response.clicked() {
        open(ui, output);
    }
}

/// What its name leaves out: where it leads, and the task it came from.
fn about(output: &Output) -> String {
    let mut about = match output.title.as_str() {
        "" => output.url.clone(),
        title => format!("{title}\n{}", output.url),
    };
    if let Some(task) = &output.task {
        about.push_str(&format!("\nFrom {task}"));
    }
    about
}

fn open(ui: &egui::Ui, output: &Output) {
    ui.ctx().open_url(egui::OpenUrl::new_tab(&output.url));
}
