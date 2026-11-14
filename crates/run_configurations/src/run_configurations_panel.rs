use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, Styled, Subscription, WeakEntity, Window, actions,
    px,
};
use ui::{Checkbox, ListItem, ToggleState, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{CUSTOM_GROUP, RunConfiguration, RunConfigurations};

const PANEL_KEY: &str = "RunConfigurationsPanel";

actions!(
    run_configurations_panel,
    [
        /// Toggles focus on the panel listing the run configurations of the project.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RunConfigurationsPanel>(window, cx);
        });
    })
    .detach();
}

/// Lists the same configurations as the title bar selector, grouped the way the
/// extensions that provide them name them (for example "Maven" or "Gradle").
pub struct RunConfigurationsPanel {
    run_configurations: Entity<RunConfigurations>,
    focus_handle: FocusHandle,
    position: DockPosition,
    _subscription: Subscription,
}

impl RunConfigurationsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _, cx| {
            let run_configurations = RunConfigurations::for_workspace(workspace, cx);
            cx.new(|cx| Self {
                _subscription: cx.observe(&run_configurations, |_, _, cx| cx.notify()),
                run_configurations,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
            })
        })
    }

    fn extension_groups(&self, cx: &App) -> Vec<String> {
        let mut groups = Vec::new();
        for configuration in &self.run_configurations.read(cx).configurations {
            if configuration.group != CUSTOM_GROUP && !groups.contains(&configuration.group) {
                groups.push(configuration.group.clone());
            }
        }
        groups
    }

    fn render_group_header(title: String) -> impl IntoElement {
        div()
            .px_2()
            .pt_2()
            .pb_1()
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_configuration(
        &self,
        index: usize,
        configuration: &RunConfiguration,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = configuration.key();
        let is_running = self.run_configurations.read(cx).is_running(&key, cx);
        let has_debug = configuration.can_debug();
        let run_configurations = self.run_configurations.downgrade();
        ListItem::new(("run-configuration", index))
            .start_slot(
                Icon::new(if configuration.is_custom() {
                    IconName::Settings
                } else {
                    IconName::PlayOutlined
                })
                .size(IconSize::Small)
                .color(if is_running {
                    Color::Success
                } else {
                    Color::Muted
                }),
            )
            .child(Label::new(configuration.name.clone()).size(LabelSize::Small))
            .tooltip(Tooltip::text(configuration.template.label.clone()))
            .on_click({
                let key = key.clone();
                let run_configurations = run_configurations.clone();
                move |_, window, cx| {
                    run_configurations
                        .update(cx, |run_configurations, cx| {
                            run_configurations.run(key.clone(), window, cx)
                        })
                        .ok();
                }
            })
            .when(has_debug && !is_running, |item| {
                let key = key.clone();
                let run_configurations = run_configurations.clone();
                item.end_slot(
                    IconButton::new(("run-configuration-debug", index), IconName::Debug)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Success)
                        .tooltip(Tooltip::text("Debug"))
                        .on_click(move |_, window, cx| {
                            run_configurations
                                .update(cx, |run_configurations, cx| {
                                    run_configurations.debug(key.clone(), window, cx)
                                })
                                .ok();
                        }),
                )
            })
            .when(is_running, |item| {
                item.end_slot(
                    IconButton::new(("run-configuration-stop", index), IconName::Stop)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop"))
                        .on_click(move |_, _, cx| {
                            run_configurations
                                .update(cx, |run_configurations, cx| {
                                    run_configurations.stop(&key, cx).detach()
                                })
                                .ok();
                        }),
                )
            })
    }
}

/// `Panel::icon_tooltip` must return a `&'static str`, while the tooltip is made of group
/// names that come from extensions, so each distinct tooltip is leaked exactly once.
fn intern(text: String) -> &'static str {
    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut interned = match INTERNED.get_or_init(Default::default).lock() {
        Ok(interned) => interned,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(existing) = interned.get(text.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.into_boxed_str());
    interned.insert(leaked);
    leaked
}

impl EventEmitter<PanelEvent> for RunConfigurationsPanel {}

impl Focusable for RunConfigurationsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RunConfigurationsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let configurations = self.run_configurations.read(cx).configurations.clone();
        let mut list = v_flex()
            .id("run-configurations-list")
            .size_full()
            .overflow_y_scroll()
            .p_1();
        let mut seen_choices = HashSet::new();
        for configuration in &configurations {
            let (Some(choices), Some(choices_key)) =
                (&configuration.choices, configuration.choices_key())
            else {
                continue;
            };
            if !seen_choices.insert(choices_key.clone()) {
                continue;
            }
            list = list.child(Self::render_group_header(choices.spec.label.clone()));
            for value in &choices.values {
                let is_selected = self
                    .run_configurations
                    .read(cx)
                    .is_choice_selected(&choices_key, value);
                let run_configurations = self.run_configurations.downgrade();
                let choices_key = choices_key.clone();
                let value = value.clone();
                list = list.child(
                    div().px_2().py_0p5().child(
                        Checkbox::new(
                            SharedString::from(format!("run-configuration-choice-{choices_key}-{value}")),
                            ToggleState::from(is_selected),
                        )
                        .label(value.clone())
                        .on_click(move |_, _, cx| {
                            run_configurations
                                .update(cx, |run_configurations, cx| {
                                    run_configurations.toggle_choice(
                                        choices_key.clone(),
                                        value.clone(),
                                        cx,
                                    )
                                })
                                .ok();
                        }),
                    ),
                );
            }
        }
        let mut current_group: Option<&str> = None;
        for (index, configuration) in configurations.iter().enumerate() {
            let group = configuration.group.as_str();
            if current_group != Some(group) {
                list = list.child(Self::render_group_header(group.to_string()));
                current_group = Some(group);
            }
            list = list.child(self.render_configuration(index, configuration, cx));
        }
        v_flex()
            .key_context("RunConfigurationsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(list)
    }
}

impl Panel for RunConfigurationsPanel {
    fn persistent_name() -> &'static str {
        PANEL_KEY
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    /// Hidden from the activity bar until an installed extension provides configurations
    /// for the project.
    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        (!self.extension_groups(cx).is_empty()).then_some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _: &Window, cx: &App) -> Option<&'static str> {
        let groups = self.extension_groups(cx);
        (!groups.is_empty()).then(|| intern(groups.join(" / ")))
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}
