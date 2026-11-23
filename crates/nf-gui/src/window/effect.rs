//! The external effect after echo cancellation and noise suppression.

use std::path::Path;

use nf_engine::{EffectInfo, EngineState};
use windows_reactor::*;

use super::{Msg, NOT_RUN, Window};
use crate::effect_picker;
use crate::widgets::{card, secondary, selectable};

impl Window {
    pub(super) fn choose_effect(&mut self, context: &ComponentContext<Self>) {
        if self.choosing_effect || self.test.busy() {
            return;
        }
        self.choosing_effect = true;
        let owner = effect_picker::owner();
        context.spawn_background_with_rejection(
            move |_| Msg::EffectChosen(effect_picker::choose(owner)),
            Msg::EffectChosen(Err(NOT_RUN.to_owned())),
        );
    }

    pub(super) fn active_effect(&self) -> Option<&EffectInfo> {
        let chosen = &self.settings.engine.effect;
        self.effect.as_ref().filter(|e| {
            self.status.state == EngineState::Running
                && chosen.path.as_ref() == Some(&e.path)
                && chosen.plugin_id.as_ref().is_none_or(|id| id == &e.plugin_id)
        })
    }

    pub(super) fn set_effect_param(&mut self, path: &Path, plugin_id: &str, id: u32, value: Option<f64>) {
        let Some(effect) = self.active_effect().filter(|e| e.path == path && e.plugin_id == plugin_id) else { return };
        let Some(param) = effect.params.iter().find(|p| p.id == id) else { return };
        let Some(value) = value.and_then(|value| param_value(value, param.min, param.max, param.stepped)) else { return };
        if value == self.settings.engine.effect.params.get(&id).copied().unwrap_or(param.default) {
            return;
        }
        self.configure(|c| {
            c.effect.params.insert(id, value);
        });
    }

    pub(super) fn effect_card(&self, context: &ViewContext<Self>) -> View {
        let chosen = &self.settings.engine.effect;
        let enabled = !self.choosing_effect && !self.test.busy();
        let pick = Button::new().is_enabled(enabled).on_click(context.callback(|()| Msg::ChooseEffect)).content(if chosen.path.is_some() {
            "Change effect…"
        } else {
            "Choose effect…"
        });
        let Some(path) = chosen.path.as_ref() else {
            return card("CLAP effect", None, pick);
        };
        let remove = Button::new().is_enabled(enabled).on_click(context.callback(|()| Msg::RemoveEffect)).content("Remove");
        let bypass =
            CheckBox::new().is_checked(chosen.bypass).on_is_checked_changed(context.callback(Msg::BypassEffect)).content("Bypass effect");
        let controls = if let Some(effect) = self.active_effect() {
            let params = effect.params.iter().filter_map(|p| {
                let value = param_value(chosen.params.get(&p.id).copied().unwrap_or(p.default), p.min, p.max, p.stepped)?;
                let (path, plugin_id, id) = (path.clone(), effect.plugin_id.clone(), p.id);
                let field = NumberBox::new()
                    .minimum(p.min)
                    .maximum(p.max)
                    .value(value)
                    .is_enabled(enabled && p.max > p.min)
                    .on_value_changed(context.callback(move |value| Msg::EffectParam {
                        path: path.clone(),
                        plugin_id: plugin_id.clone(),
                        id,
                        value,
                    }))
                    .slot(NumberBoxSlot::Header, p.name.as_str());
                Some((p.id, field))
            });
            let fallback: View = if effect.failed {
                secondary("The effect host stopped. Dry audio continues. Stop and start the filter to reload the effect.").into()
            } else if effect.missed_blocks > 0 {
                secondary(format!("Effect missed {} blocks. Dry audio played for those blocks.", effect.missed_blocks)).into()
            } else {
                View::empty()
            };
            let parameters = View::keyed_fragment([(
                format!("{}:{}", effect.path.display(), effect.plugin_id),
                StackPanel::new().spacing(6.0).keyed_children(params),
            )]);
            StackPanel::new().spacing(6.0).children((TextBlock::new().text(effect.name.as_str()), parameters, fallback))
        } else if self.status.state == EngineState::Running {
            secondary("Waiting for the effect host.").into()
        } else {
            secondary("Start the filter to load the effect and show its controls.").into()
        };
        let buttons = StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).children((pick, remove));
        card(
            "CLAP effect",
            None,
            StackPanel::new().spacing(8.0).children((selectable(path.display().to_string()), buttons, bypass, controls)),
        )
    }
}

/// The host supplies the finite range and a CLAP stepped parameter takes integer values. Reject malformed ranges before
/// handing them to a native control, including one with no valid integer between its ends.
fn param_value(value: f64, min: f64, max: f64, stepped: bool) -> Option<f64> {
    if !value.is_finite() || !min.is_finite() || !max.is_finite() || min > max {
        return None;
    }
    if stepped {
        let (min, max) = (min.ceil(), max.floor());
        (min <= max).then(|| value.round().clamp(min, max))
    } else {
        Some(value.clamp(min, max))
    }
}

#[cfg(test)]
mod tests {
    use super::param_value;

    #[test]
    fn external_parameter_bounds_are_safe_for_native_controls() {
        assert_eq!(param_value(f64::NAN, 0.0, 1.0, false), None);
        assert_eq!(param_value(1.0, 1.0, 0.0, false), None);
        assert_eq!(param_value(1.0, 0.1, 0.9, true), None);
        assert_eq!(param_value(0.0, 0.1, 3.0, true), Some(1.0));
        assert_eq!(param_value(1.6, -2.0, 2.0, true), Some(2.0));
        assert_eq!(param_value(5.0, -2.0, 0.5, false), Some(0.5));
    }
}
