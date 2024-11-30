use crate::ItemHandle;
use crate::Pane;
use collections::HashMap;
use gpui::Global;
use gpui::Model;
use gpui::View;
use gpui::ViewContext;
use gpui::{AppContext, Entity};
use project::ProjectPath;
use std::path::PathBuf;
use ui::Context;

use crate::{NavigationEntry, Workspace};

#[derive(PartialEq)]
pub enum MarkType {
    DynamicMark,
    StaticMark,
}

pub struct Mark {
    pub project_path: ProjectPath,
    pub absolute_path: Option<PathBuf>,
    pub mark_type: MarkType,
    pub entry: NavigationEntry,
}

pub struct GlobalMarks {
    pub marks: HashMap<String, Mark>,
}

pub fn init(cx: &mut AppContext) {
    cx.set_global(GlobalMarks::new());
}

impl GlobalMarks {
    fn new() -> Self {
        Self {
            marks: HashMap::new(),
        }
    }
    // pub fn get_marks(cx: &AppContext) -> Self {
    //     cx.global::<GlobalGlobalMarks>()
    // }
    // pub fn get_marks_mut(cx: &mut AppContext) -> &mut Self {
    //     cx.global_mut::<GlobalGlobalMarks>()
    // }
    // pub fn get_marks(cx: &AppContext) -> Self {
    //     cx.global::<GlobalGlobalMarks>()
    // }
    // pub fn get_marks_mut(cx: &mut AppContext) -> &mut Self {
    //     cx.global_mut::<GlobalGlobalMarks>()
    // }
    pub fn get_dynamic_marks(&self) -> Vec<&Mark> {
        let mut v = Vec::new();
        for (_, value) in &self.marks {
            if value.mark_type == MarkType::DynamicMark {
                v.push(value);
            }
        }
        v
    }
    pub fn navigate_mark(
        &mut self,
        mark_name: String,
        workspace: &mut Workspace,
        cx: &mut ViewContext<Workspace>,
    ) {
        match get_pane(mark_name.clone(), workspace, cx) {
            Some(pane) => pane.update(cx, |pane, cx| {
                if let Some(mark) = self.marks.get(&mark_name) {
                    pane.focus(cx);
                    if let Some(index) = mark
                        .entry
                        .item
                        .upgrade()
                        .and_then(|v| pane.index_for_item(v.as_ref()))
                    {
                        let prev_active_item_index = pane.active_item_index();
                        pane.activate_item(index, true, true, cx);

                        if let Some(active_item) = pane.active_item() {
                            if let Some(data) = mark.entry.data {
                                active_item.navigate(data, cx);
                            }
                        }
                    }
                }
            }),
            None => {
                if let Some(mark) = self.marks.get(&mark_name) {
                    workspace
                        .launch_path(
                            workspace.active_pane().downgrade(),
                            mark.project_path.clone(),
                            mark.absolute_path.clone(),
                            &mark.entry,
                            crate::NavigationMode::Normal,
                            cx,
                        )
                        .detach();
                }
            }
        };
    }
}

impl Global for GlobalMarks {}

fn get_pane<'a>(
    mark_name: String,
    workspace: &'a mut Workspace,
    cx: &'a AppContext,
) -> Option<View<Pane>> {
    let mark = cx.global::<GlobalMarks>().marks.get(&mark_name)?;
    return workspace.center.find(|pane| {
        let project_item = mark.entry.item.upgrade()?;
        let entry_id = project_item.project_entry_ids(cx)[0];
        let project_path = project_item.project_path(cx);

        let mut item = pane.read(cx).item_for_entry(entry_id, cx);
        if item.is_none() {
            if let Some(project_path) = project_path {
                item = pane.read(cx).item_for_path(project_path, cx);
            }
        }

        item.and_then(|item| item.downcast::<Pane>())
    });
}
