use windows_reactor::*;

use super::{Msg, Window};
use crate::settings::PipelinePreset;
use crate::widgets::secondary;

/// Editing the name or browsing saved presets does not change what is playing. Load applies the selected chain in one update.
#[derive(Default)]
pub(super) struct PresetPicker {
    expanded: bool,
    selected: Option<String>,
    name: String,
    feedback: String,
}

pub(crate) enum PresetAction {
    Expand(bool),
    Select(Option<String>),
    Name(String),
    Save,
    Load,
    Remove,
}

impl Window {
    pub(super) fn edit_preset(&mut self, action: PresetAction) {
        let picker = &mut self.preset_picker;
        match action {
            PresetAction::Expand(expanded) => picker.expanded = expanded,
            PresetAction::Name(name) => {
                picker.name = name;
                picker.feedback.clear();
            }
            PresetAction::Select(Some(name)) if self.settings.presets.contains_key(&name) => {
                picker.name.clone_from(&name);
                picker.selected = Some(name);
                picker.feedback.clear();
            }
            PresetAction::Save if !picker.name.trim().is_empty() => {
                let name = picker.name.trim().to_owned();
                self.settings.presets.insert(name.clone(), PipelinePreset::capture(&self.settings.engine));
                picker.feedback = format!("Saved “{name}”.");
                picker.name.clone_from(&name);
                picker.selected = Some(name);
                self.persist();
            }
            PresetAction::Load if !self.test.busy() => {
                if let Some(name) = &picker.selected
                    && let Some(preset) = self.settings.presets.get(name).cloned()
                {
                    picker.feedback = format!("Loaded “{name}”.");
                    self.configure(|c| preset.apply(c));
                }
            }
            PresetAction::Remove => {
                if let Some(name) = picker.selected.take() {
                    self.settings.presets.remove(&name);
                    picker.feedback = format!("Removed “{name}”.");
                    picker.name.clear();
                    self.persist();
                }
            }
            _ => {}
        }
    }

    pub(super) fn presets_section(&self, context: &ViewContext<Self>) -> View {
        let picker = &self.preset_picker;
        let names: Vec<String> = self.settings.presets.keys().cloned().collect();
        let selected = names.iter().position(|name| Some(name) == picker.selected.as_ref());
        let save_label = if self.settings.presets.contains_key(picker.name.trim()) { "Replace preset" } else { "Save preset" };
        let list: View = if names.is_empty() {
            View::empty()
        } else {
            let choices = names.clone();
            let select = ComboBox::new()
                .items_source(names.iter().map(String::as_str))
                .selected_index(selected)
                .placeholder_text("Choose a preset")
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .on_selection_changed(context.callback(move |index: Option<usize>| {
                    // A replacement ComboBox may also emit a selection-cleared notification.
                    Msg::Preset(PresetAction::Select(index.and_then(|i| choices.get(i).cloned())))
                }))
                .slot(ComboBoxSlot::Header, "Saved pipelines");
            let buttons = StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).children((
                Button::new()
                    .is_enabled(selected.is_some() && !self.test.busy())
                    .on_click(context.callback(|()| Msg::Preset(PresetAction::Load)))
                    .content("Load"),
                Button::new()
                    .is_enabled(selected.is_some())
                    .on_click(context.callback(|()| Msg::Preset(PresetAction::Remove)))
                    .content("Remove"),
            ));
            StackPanel::new().spacing(8.0).children((View::keyed_fragment([(names.join("\0"), select)]), buttons))
        };
        let body = StackPanel::new().spacing(8.0).children((
            list,
            TextBox::new()
                .text(picker.name.as_str())
                .on_text_changed(context.callback(|name| Msg::Preset(PresetAction::Name(name))))
                .slot(TextBoxSlot::Header, "Preset name"),
            Button::new()
                .is_enabled(!picker.name.trim().is_empty())
                .on_click(context.callback(|()| Msg::Preset(PresetAction::Save)))
                .content(save_label),
            if picker.feedback.is_empty() { View::empty() } else { secondary(picker.feedback.as_str()).into() },
        ));
        Expander::new()
            .is_expanded(picker.expanded)
            .on_is_expanded_changed(context.callback(|open| Msg::Preset(PresetAction::Expand(open))))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .slots([SlotView::new(ExpanderSlot::Header, "Processing presets"), SlotView::new(ExpanderSlot::Content, body)])
    }
}
