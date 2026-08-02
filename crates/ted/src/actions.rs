//! How a surface opens at all (SPEC §24.2).
//!
//! `ctrl-tab` is `tab_switcher::Toggle`, `ctrl-g` is `go_to_line::Toggle`, and
//! each opens a `Picker` modal whose job `ted` does with a surface of its own.
//! Rather than duplicate the keymap to find those keystrokes — which is exactly
//! the drift SPEC §13.1 is trying to avoid — `ted` rewrites the *binding*: GPUI
//! still resolves the keystroke, and only the action at the end of it changes,
//! so multi-keystroke bindings and anything in the user's `keymap.json` keep
//! working.
//!
//! Only the modals `ted` answers *differently* are in this table. Everything
//! else Zed opens is projected as it is by [`crate::mirror`], which is the
//! general mechanism and the reason this table stays short.
//!
//! The table is consulted in two places, and both are load-bearing. Rewriting
//! the keymap catches every keystroke and nothing else; `:ls` and `:buffers`
//! reach the same modal by a different road, resolving inside vim's interceptor
//! to `tab_switcher::ToggleAll` — an action no keymap pass has seen. So the same
//! table is applied again when the `:` line dispatches. With only the keymap
//! half, `ctrl-tab` and `:ls` do different things.

use gpui::{Action, App, Global, KeyBinding, actions};

actions!(
    ted,
    [
        /// Opens `ted`'s buffer switcher.
        OpenBufferSwitcher,
        /// Jumps to a line number.
        GoToLine,
        /// Shows the diagnostics and documentation for the cursor's position.
        Hover,
        /// Turns terminal mouse reporting on or off (SPEC §17). While it is on
        /// the terminal's own selection and copy do not see the mouse, which is
        /// why this is a toggle rather than a setting alone.
        ToggleMouse,
    ]
);

/// One of `ted`'s own surfaces, and the reason an upstream action never reaches
/// the modal it was written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Switcher,
    GoToLine,
    Hover,
}

impl Surface {
    pub fn action(self) -> Box<dyn Action> {
        match self {
            Self::Switcher => OpenBufferSwitcher.boxed_clone(),
            Self::GoToLine => GoToLine.boxed_clone(),
            Self::Hover => Hover.boxed_clone(),
        }
    }
}

/// The surface an action opens in `ted`, whether it is one of `ted`'s own or the
/// upstream action a binding or vim's interceptor named.
///
/// `editor::Hover` is in here for the same reason as the modals: SPEC §24.8
/// builds its panel from the buffer's own diagnostics and the project's hover
/// request, so the editor's hover machinery — whose popover is a view `ted`
/// cannot read — is not wanted.
pub fn surface_for(action: &dyn Action) -> Option<Surface> {
    let action = action.as_any();
    if action.is::<OpenBufferSwitcher>()
        || action.is::<tab_switcher::Toggle>()
        || action.is::<tab_switcher::ToggleAll>()
        || action.is::<tab_switcher::OpenInActivePane>()
    {
        Some(Surface::Switcher)
    } else if action.is::<GoToLine>() || action.is::<editor::actions::ToggleGoToLine>() {
        Some(Surface::GoToLine)
    } else if action.is::<Hover>() || action.is::<editor::actions::Hover>() {
        Some(Surface::Hover)
    } else {
        None
    }
}

/// The namespaces whose actions only ever drive a modal `ted` replaces. Hidden
/// from the `:` line's action-name fallback the way the font-size actions are,
/// so `ted`'s own actions take their place in the completion list (SPEC §24.2).
pub const REPLACED_NAMESPACES: [&str; 2] = ["go_to_line", "tab_switcher"];

/// Rewrites every binding that would have opened a modal `ted` owns so that it
/// dispatches `ted`'s action instead, keeping the keystrokes, the context
/// predicate and the source that binding came from.
pub fn retarget(bindings: &mut [KeyBinding], cx: &App) {
    for binding in bindings.iter_mut() {
        let Some(surface) = surface_for(binding.action()) else {
            continue;
        };
        let keystrokes = binding
            .keystrokes()
            .iter()
            .map(|keystroke| keystroke.inner().unparse())
            .collect::<Vec<_>>()
            .join(" ");
        let replacement = KeyBinding::load(
            &keystrokes,
            surface.action(),
            binding.predicate(),
            false,
            None,
            cx.keyboard_mapper().as_ref(),
        );
        let Ok(mut replacement) = replacement else {
            continue;
        };
        if let Some(meta) = binding.meta() {
            replacement.set_meta(meta);
        }
        *binding = replacement;
    }
}

/// The surface a global action listener asked for, waiting for the frame loop to
/// pick it up.
///
/// `App::on_action` runs at the end of the bubble phase, so it fires only when
/// nothing in the window consumed the action — and nothing else registers
/// `ted`'s actions, so nothing else can.
#[derive(Default)]
struct Requested(Option<Surface>);

impl Global for Requested {}

/// Whether `ted::ToggleMouse` fired since the last frame. Mouse reporting is the
/// terminal's mode rather than anything GPUI owns, so the action can only ask.
#[derive(Default)]
struct MouseToggled(bool);

impl Global for MouseToggled {}

pub fn init(cx: &mut App) {
    cx.set_global(Requested::default());
    cx.set_global(MouseToggled::default());
    cx.on_action(|_: &OpenBufferSwitcher, cx| request(Surface::Switcher, cx));
    cx.on_action(|_: &GoToLine, cx| request(Surface::GoToLine, cx));
    cx.on_action(|_: &Hover, cx| request(Surface::Hover, cx));
    cx.on_action(|_: &ToggleMouse, cx| cx.set_global(MouseToggled(true)));
}

/// Whether the mouse was toggled since the last frame, cleared by reading it.
pub fn take_mouse_toggle(cx: &mut App) -> bool {
    if !cx
        .try_global::<MouseToggled>()
        .is_some_and(|toggled| toggled.0)
    {
        return false;
    }
    cx.set_global(MouseToggled::default());
    true
}

pub fn request(surface: Surface, cx: &mut App) {
    cx.set_global(Requested(Some(surface)));
}

/// What was asked for since the last frame, cleared by reading it.
pub fn take_request(cx: &mut App) -> Option<Surface> {
    let requested = cx.try_global::<Requested>()?.0?;
    cx.set_global(Requested::default());
    Some(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_modal_ted_replaced_resolves_to_the_surface_that_replaced_it() {
        let cases: [(Box<dyn Action>, Surface); 6] = [
            (
                tab_switcher::Toggle::default().boxed_clone(),
                Surface::Switcher,
            ),
            // How `:ls` and `:buffers` arrive: an action the keymap pass never
            // saw, which is why the table is consulted at dispatch too.
            (tab_switcher::ToggleAll.boxed_clone(), Surface::Switcher),
            (OpenBufferSwitcher.boxed_clone(), Surface::Switcher),
            (
                editor::actions::ToggleGoToLine.boxed_clone(),
                Surface::GoToLine,
            ),
            (editor::actions::Hover.boxed_clone(), Surface::Hover),
            (Hover.boxed_clone(), Surface::Hover),
        ];
        for (action, expected) in cases {
            assert_eq!(
                surface_for(action.as_ref()),
                Some(expected),
                "{} did not reach its surface",
                action.name()
            );
        }
    }

    #[test]
    fn an_action_ted_has_no_surface_for_is_left_alone() {
        assert_eq!(surface_for(&editor::actions::Undo), None);
        assert_eq!(surface_for(&workspace::CloseActiveItem::default()), None);
    }

    /// The file finder is Zed's own modal again, projected by [`crate::mirror`]
    /// rather than replaced — so a binding for it must reach it untouched.
    #[test]
    fn a_mirrored_modal_keeps_its_own_action() {
        assert_eq!(
            surface_for(&workspace::ToggleFileFinder::default()),
            None,
            "the file finder is mirrored, not replaced"
        );
        assert!(
            !REPLACED_NAMESPACES.contains(&"file_finder"),
            "a mirrored modal's namespace must stay reachable from the `:` line"
        );
    }

    /// Hiding a namespace from the `:` line is only safe when everything in it
    /// has somewhere else to go: an action hidden with nothing replacing it is
    /// unreachable rather than replaced.
    #[test]
    fn every_hidden_namespace_has_a_surface_behind_it() {
        for namespace in REPLACED_NAMESPACES {
            let covered = [
                editor::actions::ToggleGoToLine.boxed_clone(),
                tab_switcher::Toggle::default().boxed_clone(),
            ]
            .iter()
            .any(|action| {
                action.name().starts_with(namespace) && surface_for(action.as_ref()).is_some()
            });
            assert!(covered, "{namespace} is hidden but nothing replaces it");
        }
    }
}
