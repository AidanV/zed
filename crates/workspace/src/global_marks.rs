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
    pub fn get_dynamic_marks(&self) -> HashMap<usize, &Mark> {
        let mut hm = HashMap::new();
        for (key, value) in &self.marks {
            let i = key.parse::<usize>();
            if i.is_ok() && value.mark_type == MarkType::DynamicMark {
                hm.insert(i.expect("Failed to convert to int"), value);
            }
        }
        hm
    }
}

impl Global for GlobalMarks {}

pub fn navigate_mark(
    mark_name: String,
    workspace: &mut Workspace,
    cx: &mut ViewContext<Workspace>,
) {
    println!("2");
    match get_pane(mark_name.clone(), workspace, cx) {
        Some(pane) => pane.update(cx, |pane, cx| {
            pane.focus(cx);

            println!("3");
            let Some(mark) = cx.global::<GlobalMarks>().marks.get(&mark_name) else {
                return;
            };

            println!("4");
            if let Some(index) = mark
                .entry
                .item
                .upgrade()
                .and_then(|v| pane.index_for_item(v.as_ref()))
            {
                let prev_active_item_index = pane.active_item_index();
                pane.activate_item(index, true, true, cx);

                println!("5");
                // if let Some(active_item) = pane.active_item() {
                //     if let Some(data) = mark.entry.data {
                //         active_item.navigate(data, cx);
                //     }
                // }
            }
        }),
        None => {
            println!("we went none");
            // if let Some(mark) = self.marks.get(&mark_name) {
            //     workspace
            //         .launch_path(
            //             workspace.active_pane().downgrade(),
            //             mark.project_path.clone(),
            //             mark.absolute_path.clone(),
            //             &mark.enty,
            //             crate::NavigationMode::Normal,
            //             cx,
            //         )
            //         .detach();
            // }
        }
    };
}

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

        println!("a {:?}", entry_id);
        let mut item = pane.read(cx).item_for_entry(entry_id, cx);
        println!("b {}", item.is_some());
        if item.is_none() {
            if let Some(project_path) = project_path {
                item = pane.read(cx).item_for_path(project_path, cx);
            }
        }
        println!("c {}", item.is_some());

        if item.is_some() {
            return Some(pane.clone());
        }
        None
    });
}
