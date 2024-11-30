#[cfg(test)]
mod harpoon_tests;

use collections::HashMap;
use editor::items::entry_git_aware_label_color;
use gpui::{
    actions, impl_actions, rems, Action, AnyElement, AppContext, DismissEvent, EntityId,
    EventEmitter, FocusHandle, FocusableView, KeyContext, Model, Modifiers, MouseButton,
    MouseUpEvent, ParentElement, Render, Styled, Task, View, ViewContext, VisualContext, WeakView,
};
use picker::{Picker, PickerDelegate};
use project::{project_settings::ProjectSettings, Project};
use serde::Deserialize;
use settings::Settings;
use std::{path::Path, sync::Arc};
use ui::{prelude::*, ListItem, ListItemSpacing, Tooltip};
use util::ResultExt;
use workspace::{
    global_marks::GlobalMarks,
    item::{ItemHandle, ItemSettings, TabContentParams},
    pane::{render_item_indicator, tab_details, Event as PaneEvent},
    ModalView, Pane, SaveIntent, Workspace,
};

const PANEL_WIDTH_REMS: f32 = 28.;

#[derive(PartialEq, Clone, Deserialize, Default)]
pub struct Toggle {
    #[serde(default)]
    pub select_last: bool,
}

#[derive(Clone, Deserialize, PartialEq)]
struct Number(usize);

impl_actions!(harpoon, [Toggle, Number]);
actions!(harpoon, [CloseSelectedItem, Add, Delete, Swap]);

const MAX_HARPOON_LEN: usize = 9;

enum HarpoonState {
    Go(Option<usize>),
    Add,
    Delete(Option<usize>),
    Swap(Option<usize>, Option<usize>),
}

pub struct Harpoon {
    picker: View<Picker<HarpoonDelegate>>,
    state: HarpoonState,
    // init_modifiers: Option<Modifiers>,
}

impl ModalView for Harpoon {}

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(Harpoon::register).detach();
}

/// a to add a file
/// d # to delete a file
/// s # # to swap a file
/// n to go to next in list
/// p to go to prev in list
impl Harpoon {
    fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
        workspace.register_action(|workspace, action: &Toggle, cx| {
            Self::open(action, workspace, cx);
            return;
        });

        workspace.register_action(|workspace, action: &Add, cx| {
            let Some(mut harpoon) = workspace.active_modal(cx) else {
                return;
            };
            harpoon.update(cx, |harpoon: &mut Harpoon, cx| {
                harpoon.state = HarpoonState::Add;
                harpoon.handle(cx);
            });
        });
        workspace.register_action(|workspace, action: &Delete, cx| {
            let Some(mut harpoon) = workspace.active_modal(cx) else {
                return;
            };
            harpoon.update(cx, |harpoon: &mut Harpoon, cx| {
                harpoon.state = HarpoonState::Delete(None);
                harpoon.handle(cx);
            });
        });
        workspace.register_action(|workspace, action: &Swap, cx| {
            let Some(mut harpoon) = workspace.active_modal(cx) else {
                return;
            };
            harpoon.update(cx, |harpoon: &mut Harpoon, cx| {
                harpoon.state = HarpoonState::Swap(None, None);
                harpoon.handle(cx);
            });
        });
        workspace.register_action(|workspace, action: &Number, cx| {
            let Some(mut harpoon) = workspace.active_modal(cx) else {
                return;
            };
            harpoon.update(cx, |harpoon: &mut Harpoon, cx| {
                match harpoon.state {
                    HarpoonState::Go(None) => harpoon.state = HarpoonState::Go(Some(action.0)),
                    HarpoonState::Delete(None) => {
                        harpoon.state = HarpoonState::Delete(Some(action.0))
                    }
                    HarpoonState::Swap(None, None) => {
                        harpoon.state = HarpoonState::Swap(Some(action.0), None)
                    }
                    HarpoonState::Swap(Some(n), None) => {
                        harpoon.state = HarpoonState::Swap(Some(n), Some(action.0))
                    }
                    _ => {}
                }
                harpoon.handle(cx);
            });
        });
    }

    fn open(action: &Toggle, workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) {
        let mut weak_pane = workspace.active_pane().downgrade();
        for dock in [
            workspace.left_dock(),
            workspace.bottom_dock(),
            workspace.right_dock(),
        ] {
            dock.update(cx, |this, cx| {
                let Some(panel) = this
                    .active_panel()
                    .filter(|panel| panel.focus_handle(cx).contains_focused(cx))
                else {
                    return;
                };
                if let Some(pane) = panel.pane(cx) {
                    weak_pane = pane.downgrade();
                }
            })
        }

        let project = workspace.project().clone();
        workspace.toggle_modal(cx, |cx| {
            let delegate =
                HarpoonDelegate::new(project, action, cx.view().downgrade(), weak_pane, cx);
            Harpoon::new(delegate, cx)
        });
    }

    fn new(delegate: HarpoonDelegate, cx: &mut ViewContext<Self>) -> Self {
        Self {
            picker: cx.new_view(|cx| Picker::nonsearchable_uniform_list(delegate, cx)),
            state: HarpoonState::Go(None),
            // init_modifiers: cx.modifiers().modified().then_some(cx.modifiers()),
        }
    }

    fn handle(&mut self, cx: &mut ViewContext<Self>) {
        match self.state {
            HarpoonState::Add => self.picker.update(cx, |picker, cx| {
                picker.delegate.add_match(cx);
                self.state = HarpoonState::Go(None);
            }),
            HarpoonState::Delete(Some(at)) => self.picker.update(cx, |picker, cx| {
                picker.delegate.remove_match(at, cx);
                self.state = HarpoonState::Go(None);
            }),
            HarpoonState::Swap(Some(from), None) => self.picker.update(cx, |picker, cx| {
                picker.delegate.set_selected_index(from, cx);
            }),
            HarpoonState::Swap(Some(from), Some(to)) => self.picker.update(cx, |picker, cx| {
                picker.delegate.swap_match(from, to, cx);
                self.state = HarpoonState::Go(None);
            }),

            _ => {}
        }
        cx.notify();
    }

    // fn handle_modifiers_changed(
    //     &mut self,
    //     event: &ModifiersChangedEvent,
    //     cx: &mut ViewContext<Self>,
    // ) {
    //     let Some(init_modifiers) = self.init_modifiers else {
    //         return;
    //     };
    //     if !event.modified() || !init_modifiers.is_subset_of(event) {
    //         self.init_modifiers = None;
    //         if self.picker.read(cx).delegate.matches.is_empty() {
    //             // cx.emit(DismissEvent)
    //         } else {
    //             // cx.dispatch_action(menu::Confirm.boxed_clone());
    //         }
    //     }
    // }

    // fn handle_close_selected_item(&mut self, _: &CloseSelectedItem, cx: &mut ViewContext<Self>) {
    //     self.picker.update(cx, |picker, cx| {
    //         picker
    //             .delegate
    //             .close_item_at(picker.delegate.selected_index(), cx)
    //     });
    // }
}

impl EventEmitter<DismissEvent> for Harpoon {}

impl FocusableView for Harpoon {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for Harpoon {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex()
            .key_context("Harpoon")
            .w(rems(PANEL_WIDTH_REMS))
            // .on_action(cx.listener(Self::handle_close_selected_item))
            .child(self.picker.clone())
    }
}

struct TabMatch {
    item_index: usize,
    item: Box<dyn ItemHandle>,
    detail: usize,
    preview: bool,
}

pub struct HarpoonDelegate {
    select_last: bool,
    harpoon: WeakView<Harpoon>,
    selected_index: usize,
    pane: WeakView<Pane>,
    project: Model<Project>,
    matches: Vec<Path>,
}

impl HarpoonDelegate {
    fn new(
        project: Model<Project>,
        action: &Toggle,
        harpoon: WeakView<Harpoon>,
        pane: WeakView<Pane>,
        cx: &mut ViewContext<Harpoon>,
    ) -> Self {
        Self::subscribe_to_updates(&pane, cx);
        Self {
            select_last: action.select_last,
            harpoon,
            selected_index: 0,
            pane,
            project,
            matches: Vec::new(),
        }
    }

    fn subscribe_to_updates(pane: &WeakView<Pane>, cx: &mut ViewContext<Harpoon>) {
        let Some(pane) = pane.upgrade() else {
            return;
        };
        // cx.subscribe(&pane, |harpoon, _, event, cx| {
        //     match event {
        //         PaneEvent::AddItem { .. }
        //         | PaneEvent::RemovedItem { .. }
        //         | PaneEvent::Remove { .. } => harpoon.picker.update(cx, |picker, cx| {
        //             let selected_item_id = picker.delegate.selected_item_id();
        //             // picker.delegate.update_matches(cx);
        //             if let Some(item_id) = selected_item_id {
        //                 picker.delegate.select_item(item_id, cx);
        //             }
        //             cx.notify();
        //         }),
        //         _ => {}
        //     };
        // })
        // .detach();
    }

    fn add_match(&mut self, cx: &mut WindowContext) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let pane = pane.read(cx);
        let Some(active_tab) = pane.active_item() else {
            return;
        };
        if self.matches.len() < MAX_HARPOON_LEN {
            let Some(project_path) = active_tab.project_path(cx) else {
                return;
            };
            self.matches.push(Path::from(project_path.path));
            // self.matches.push(TabMatch {
            //     item_index: 0,
            //     item: active_tab.boxed_clone(),
            //     detail: 0,
            //     preview: false,
            // });
        }
    }

    fn remove_match(&mut self, at: usize, cx: &mut WindowContext) {
        if at < self.matches.len() {
            self.matches.remove(at);
        }
    }

    fn swap_match(&mut self, from: usize, to: usize, cx: &mut WindowContext) {
        // let Some(from) = self.matches..get_mut(&from) else {
        //     return;
        // };
        // let Some(to) = self.matches.get_mut(&to) else {
        //     return;
        // };
        if from < self.matches.len() && to < self.matches.len() {
            self.matches.swap(from, to);
        }
    }

    fn update_matches(&mut self, cx: &mut WindowContext) {
        self.matches.clear();
        for mark in cx.global::<GlobalMarks>().get_dynamic_marks() {
            self.matches.push(TabMatch {
                item_index: 0,
                item: mark.entry.item.upgrade()?,
                detail: 0,
                preview: false,
            });
        }
    }

    // fn update_matches(&mut self, cx: &mut WindowContext) {
    //     self.matches.clear();
    //     let Some(pane) = self.pane.upgrade() else {
    //         return;
    //     };

    //     let pane = pane.read(cx);
    //     let mut history_indices = HashMap::default();
    //     pane.activation_history().iter().rev().enumerate().for_each(
    //         |(history_index, history_entry)| {
    //             history_indices.insert(history_entry.entity_id, history_index);
    //         },
    //     );

    //     let items: Vec<Box<dyn ItemHandle>> = pane.items().map(|item| item.boxed_clone()).collect();
    //     items
    //         .iter()
    //         .enumerate()
    //         .zip(tab_details(&items, cx))
    //         .map(|((item_index, item), detail)| TabMatch {
    //             item_index,
    //             item: item.boxed_clone(),
    //             detail,
    //             preview: pane.is_active_preview_item(item.item_id()),
    //         })
    //         .for_each(|tab_match| self.matches.push(tab_match));

    //     let non_history_base = history_indices.len();
    //     self.matches.sort_by(move |a, b| {
    //         let a_score = *history_indices
    //             .get(&a.item.item_id())
    //             .unwrap_or(&(a.item_index + non_history_base));
    //         let b_score = *history_indices
    //             .get(&b.item.item_id())
    //             .unwrap_or(&(b.item_index + non_history_base));
    //         a_score.cmp(&b_score)
    //     });

    //     if self.matches.len() > 1 {
    //         if self.select_last {
    //             self.selected_index = self.matches.len() - 1;
    //         } else {
    //             self.selected_index = 1;
    //         }
    //     }
    // }

    // fn selected_item_id(&self) -> Option<EntityId> {
    //     self.matches
    //         .get(self.selected_index())
    //         .map(|tab_match| tab_match.item.item_id())
    // }

    // fn select_item(
    //     &mut self,
    //     item_id: EntityId,
    //     cx: &mut ViewContext<'_, Picker<HarpoonDelegate>>,
    // ) {
    //     let selected_idx = self
    //         .matches
    //         .iter()
    //         .position(|tab_match| tab_match.item.item_id() == item_id)
    //         .unwrap_or(0);
    //     self.set_selected_index(selected_idx, cx);
    // }

    // fn close_item_at(&mut self, ix: usize, cx: &mut ViewContext<'_, Picker<HarpoonDelegate>>) {
    //     let Some(tab_match) = self.matches.get(ix) else {
    //         return;
    //     };
    //     let Some(pane) = self.pane.upgrade() else {
    //         return;
    //     };
    //     pane.update(cx, |pane, cx| {
    //         pane.close_item_by_id(tab_match.item.item_id(), SaveIntent::Close, cx)
    //             .detach_and_log_err(cx);
    //     });
    // }
}

impl PickerDelegate for HarpoonDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _cx: &mut WindowContext) -> Arc<str> {
        Arc::default()
    }

    fn no_matches_text(&self, _cx: &mut WindowContext) -> SharedString {
        "a        to add current tab\n\nd #     to delete\n\ns # #   to swap".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut ViewContext<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        Vec::new()
    }

    fn update_matches(
        &mut self,
        _raw_query: String,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Task<()> {
        self.update_matches(cx);
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, cx: &mut ViewContext<Picker<HarpoonDelegate>>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let Some(selected_match) = self.matches.get(self.selected_index) else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.activate_item(selected_match.item_index, true, true, cx);
        });
    }

    fn dismissed(&mut self, cx: &mut ViewContext<Picker<HarpoonDelegate>>) {
        self.harpoon
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        // let tab_match;
        // // self.project.read_with(cx, |project, cx| {
        // //     tab_match = cx.global_mut::<ProjectSettings>().harpoon.paths.get(ix);
        // // });
        // for (key, mark) in cx.global::<GlobalMarks>().marks {
        //     // if ('A'..'Z').contains(key.get(0)?) {
        //     tab_match = TabMatch {
        //         item_index: 0,
        //         item: mark.entry.item.upgrade()?,
        //         detail: 0,
        //         preview: false,
        //     };
        //     break;
        //     // }
        // }

        let tab_match = self.matches.get(ix)?;

        let params = TabContentParams {
            detail: Some(tab_match.detail),
            selected: true,
            preview: tab_match.preview,
        };
        let label = tab_match.item.tab_content(params, cx);

        let icon = tab_match.item.tab_icon(cx).map(|icon| {
            let git_status_color = ItemSettings::get_global(cx)
                .git_status
                .then(|| {
                    tab_match
                        .item
                        .project_path(cx)
                        .as_ref()
                        .and_then(|path| self.project.read(cx).entry_for_path(path, cx))
                        .map(|entry| {
                            entry_git_aware_label_color(
                                entry.git_status,
                                entry.is_ignored,
                                selected,
                            )
                        })
                })
                .flatten();

            icon.color(git_status_color.unwrap_or_default())
        });

        let indicator = render_item_indicator(tab_match.item.boxed_clone(), cx);
        let indicator_color = if let Some(ref indicator) = indicator {
            indicator.color
        } else {
            Color::default()
        };
        let indicator = h_flex()
            .flex_shrink_0()
            .children(indicator)
            .child(div().w_2())
            .into_any_element();
        let number = Label::new(String::from((ix).to_string()));
        let close_button = div()
            // We need this on_mouse_up here instead of on_click on the close
            // button because Picker intercepts the same events and handles them
            // as click's on list items.
            // See the same handler in Picker for more details.
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |picker, _: &MouseUpEvent, cx| {
                    cx.stop_propagation();
                    // picker.delegate.close_item_at(ix, cx);
                }),
            )
            .child(
                IconButton::new("close_tab", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(indicator_color)
                    .tooltip(|cx| Tooltip::text("Close", cx)),
            )
            .into_any_element();

        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .selected(selected)
                .child(h_flex().w_full().child(label))
                .start_slot::<Icon>(icon)
                .map(|el| {
                    if self.selected_index == ix {
                        el.end_slot::<AnyElement>(close_button)
                    } else {
                        el.end_slot::<AnyElement>(indicator)
                            .end_hover_slot::<AnyElement>(close_button)
                    }
                })
                .map(|el| el.start_slot::<Label>(number)),
        )
    }
}
