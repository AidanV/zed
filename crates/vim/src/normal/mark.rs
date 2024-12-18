use std::{ops::Range, sync::Arc};

use crate::{
    motion::{self, Motion},
    state::Mode,
    Vim,
};
use editor::{
    display_map::{DisplaySnapshot, ToDisplayPoint},
    movement,
    scroll::Autoscroll,
    Anchor, Bias, DisplayPoint,
};
use gpui::{Entity, ViewContext};
use language::SelectionGoal;
use workspace::{
    global_marks::{self, GlobalMarks},
    ItemHandle, WeakItemHandle,
};

impl Vim {
    pub fn create_mark(&mut self, text: Arc<str>, tail: bool, cx: &mut ViewContext<Self>) {
        let Some(anchors) = self.update_editor(cx, |_, editor, _| {
            editor
                .selections
                .disjoint_anchors()
                .iter()
                .map(|s| if tail { s.tail() } else { s.head() })
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        if text.starts_with(|c: char| c.is_digit(10)) {
            if let Some(editor) = self.editor.upgrade() {
                let Some(project_path) = editor.project_path(cx) else {
                    // We cannot harpoon this editor if it does not have an associated file
                    self.clear_operator(cx);
                    return;
                };
                println!("!");
                let navigation_data = editor.update(cx, |editor, cx| {
                    editor.get_navigation_data(editor.selections.newest_anchor().head(), cx)
                });
                let absolute_path = editor.read(cx).abs_path_current_buffer(cx);
                cx.global_mut::<GlobalMarks>().marks.insert(
                    text.to_string(),
                    global_marks::Mark {
                        project_path,
                        absolute_path,
                        mark_type: global_marks::MarkType::DynamicMark,
                        entry: workspace::NavigationEntry {
                            item: editor.downgrade_item().into(),
                            data: None, //navigation_data, //self.editor.upgrade()?.map(|data| Box::new(data) as Box<dyn Any + Send>),
                            timestamp: 0,
                            is_preview: false,
                        },
                    },
                );
            }
        } else {
            self.marks.insert(text.to_string(), anchors);
        }

        self.clear_operator(cx);
    }

    // When handling an action, you must create visual marks if you will switch to normal
    // mode without the default selection behavior.
    pub(crate) fn store_visual_marks(&mut self, cx: &mut ViewContext<Self>) {
        if self.mode.is_visual() {
            self.create_visual_marks(self.mode, cx);
        }
    }

    pub(crate) fn create_visual_marks(&mut self, mode: Mode, cx: &mut ViewContext<Self>) {
        let mut starts = vec![];
        let mut ends = vec![];
        let mut reversed = vec![];

        self.update_editor(cx, |_, editor, cx| {
            let (map, selections) = editor.selections.all_display(cx);
            for selection in selections {
                let end = movement::saturating_left(&map, selection.end);
                ends.push(
                    map.buffer_snapshot
                        .anchor_before(end.to_offset(&map, Bias::Left)),
                );
                starts.push(
                    map.buffer_snapshot
                        .anchor_before(selection.start.to_offset(&map, Bias::Left)),
                );
                reversed.push(selection.reversed)
            }
        });

        self.marks.insert("<".to_string(), starts);
        self.marks.insert(">".to_string(), ends);
        self.stored_visual_mode.replace((mode, reversed));
    }

    pub fn jump(&mut self, text: Arc<str>, line: bool, cx: &mut ViewContext<Self>) {
        self.pop_operator(cx);

        if (*text).starts_with(|c: char| c.is_digit(10)) {
            println!("1");
            if let Some(workspace) = self.workspace(cx) {
                workspace.update(cx, |workspace, cx| {
                    global_marks::navigate_mark((*text).to_string(), workspace, cx);
                })
            }
            return;
        }
        let anchors = match &*text {
            "{" | "}" => self.update_editor(cx, |_, editor, cx| {
                let (map, selections) = editor.selections.all_display(cx);
                selections
                    .into_iter()
                    .map(|selection| {
                        let point = if &*text == "{" {
                            movement::start_of_paragraph(&map, selection.head(), 1)
                        } else {
                            movement::end_of_paragraph(&map, selection.head(), 1)
                        };
                        map.buffer_snapshot
                            .anchor_before(point.to_offset(&map, Bias::Left))
                    })
                    .collect::<Vec<Anchor>>()
            }),
            "." => self.change_list.last().cloned(),
            _ => self.marks.get(&*text).cloned(),
        };

        let Some(anchors) = anchors else { return };

        let is_active_operator = self.active_operator().is_some();
        if is_active_operator {
            if let Some(anchor) = anchors.last() {
                self.motion(
                    Motion::Jump {
                        anchor: *anchor,
                        line,
                    },
                    cx,
                )
            }
        } else {
            self.update_editor(cx, |_, editor, cx| {
                let map = editor.snapshot(cx);
                let mut ranges: Vec<Range<Anchor>> = Vec::new();
                for mut anchor in anchors {
                    if line {
                        let mut point = anchor.to_display_point(&map.display_snapshot);
                        point = motion::first_non_whitespace(&map.display_snapshot, false, point);
                        anchor = map
                            .display_snapshot
                            .buffer_snapshot
                            .anchor_before(point.to_point(&map.display_snapshot));
                    }
                    if ranges.last() != Some(&(anchor..anchor)) {
                        ranges.push(anchor..anchor);
                    }
                }
                editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                    s.select_anchor_ranges(ranges)
                })
            });
        }
    }
}

pub fn jump_motion(
    map: &DisplaySnapshot,
    anchor: Anchor,
    line: bool,
) -> (DisplayPoint, SelectionGoal) {
    let mut point = anchor.to_display_point(map);
    if line {
        point = motion::first_non_whitespace(map, false, point)
    }

    (point, SelectionGoal::None)
}
