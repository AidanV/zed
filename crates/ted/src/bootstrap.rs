//! Building the backend `ted` drives (SPEC §9), reduced to what M0 needs: a
//! real `Editor` over a real `language::Buffer`, with no `Workspace`, no
//! `Project` and no `Client`. Those arrive in M1; the init *order* below is the
//! subset of `crates/zed/src/main.rs` that an editor alone requires, and it is
//! load-bearing.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{
    App, AppContext as _, Entity, Focusable as _, Window, WindowBounds, WindowHandle, WindowOptions,
};
use language::Buffer;
use settings::{Settings as _, SettingsStore};
use theme_settings::ThemeSettings;

use crate::cell::{self, grid_size};

/// The non-persisted override layer (SPEC §9). `buffer_font_size` and
/// `buffer_line_height` are what make [`crate::cell`]'s constants true, and
/// `soft_wrap` is what makes wrap boundaries a function of the terminal width.
const SETTINGS_OVERRIDE: &str = r#"{
    "buffer_font_size": 16,
    "buffer_line_height": { "custom": 1.0 },
    "soft_wrap": "editor_width"
}"#;

pub fn init(cx: &mut App) -> Result<()> {
    zlog::init();

    let settings_store = SettingsStore::new(cx, &settings::default_settings());
    cx.set_global(settings_store);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    release_channel::init(semver::Version::new(0, 0, 0), cx);
    // Installs `GlobalBlameRenderer`, which `Editor::gutter_dimensions` reads;
    // the rest of what it registers is workspace-scoped and inert here.
    editor::init(cx);

    if let Err(error) = assets::Assets.load_fonts(cx) {
        // `CellTextSystem` defines its own metrics and ignores real fonts, so
        // this is not fatal — but other bootstrap paths expect it to have run.
        log::warn!("could not load bundled fonts: {error}");
    }

    apply_settings_override(cx)?;
    load_keymap(cx)?;
    verify_pinned_metrics(cx)
}

/// SPEC §8.2: `ted` loads the Linux base keymap on every OS, because its
/// bindings are control-based and a terminal never receives `cmd-*`.
///
/// Without this the dispatch tree has no bindings at all and only printable
/// characters reach the editor, so this is also what exercises §4.2.1's claim
/// that `EditorElement::paint` — not prepaint — is what registers actions.
fn load_keymap(cx: &mut App) -> Result<()> {
    let mut bindings =
        settings::KeymapFile::load_asset_allow_partial_failure("keymaps/default-linux.json", cx)
            .context("could not load ted's base keymap")?;
    for binding in &mut bindings {
        binding.set_meta(settings::KeybindSource::Default.meta());
    }
    cx.bind_keys(bindings);
    Ok(())
}

fn apply_settings_override(cx: &mut App) -> Result<()> {
    use gpui::BorrowAppContext as _;

    let parse_status = cx.update_global::<SettingsStore, _>(|store, cx| {
        store.set_user_settings(SETTINGS_OVERRIDE, cx).parse_status
    });

    anyhow::ensure!(
        matches!(parse_status, settings::ParseStatus::Success),
        "ted's settings override failed to parse: {parse_status:?}"
    );
    Ok(())
}

/// SPEC §5.5: a settings change that moved the buffer font size out from under
/// a window sized in the old cells would show up as a cursor that drifts
/// further off with every column. Fail loudly instead.
fn verify_pinned_metrics(cx: &App) -> Result<()> {
    let settings = ThemeSettings::get_global(cx);
    let font_size = settings.buffer_font_size(cx);
    anyhow::ensure!(
        font_size == cell::BUFFER_FONT_SIZE,
        "buffer_font_size resolved to {font_size:?}, but ted's cell metrics assume {:?}",
        cell::BUFFER_FONT_SIZE
    );

    let line_height = settings.buffer_line_height.value();
    anyhow::ensure!(
        line_height == cell::BUFFER_LINE_HEIGHT,
        "buffer_line_height resolved to {line_height}, but ted's cell metrics assume {}",
        cell::BUFFER_LINE_HEIGHT
    );
    Ok(())
}

pub struct OpenedEditor {
    pub window: WindowHandle<Editor>,
    pub editor: Entity<Editor>,
    pub path: Option<PathBuf>,
}

/// Opens `path` into a new window whose root view is the editor itself.
///
/// The root view is the `Editor` entity rather than a wrapper element, so the
/// editor is handed the whole grid and nothing but its own overscroll insets it
/// (SPEC §10.2, and M0 acceptance (d), which `tests/cell_contract.rs` pins).
pub fn open_editor(
    path: Option<&Path>,
    columns: u16,
    rows: u16,
    cx: &mut App,
) -> Result<OpenedEditor> {
    let text = match path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?,
        None => String::new(),
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
            gpui::Point::default(),
            grid_size(columns, rows),
        ))),
        focus: true,
        show: false,
        ..Default::default()
    };

    // `WindowHandle::root` is `test-support`-gated, so the handle is taken from
    // the build closure rather than read back off the window.
    let mut created = None;
    let window = cx.open_window(options, |window, cx| {
        let editor = cx.new(|cx| build_editor(text, window, cx));
        created = Some(editor.clone());
        editor
    })?;

    let editor = created.context("newly opened window has no root view")?;

    // Focus explicitly after the window exists: combined with
    // `TerminalWindow::is_active() == true`, this is what makes the editor
    // render as focused and routes keystrokes down the intended dispatch path
    // (SPEC §9, "Focus").
    window.update(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    })?;

    Ok(OpenedEditor {
        window,
        editor,
        path: path.map(Path::to_path_buf),
    })
}

fn build_editor(text: String, window: &mut Window, cx: &mut gpui::Context<Editor>) -> Editor {
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let mut editor = Editor::for_buffer(buffer, None, window, cx);

    // Every remaining contributor to the editor's horizontal budget, zeroed, so
    // the geometry stays whole cells. `offset_content` matters even with the
    // gutter hidden: it contributes a margin of `-descent`
    // (`crates/editor/src/editor.rs:1263`), which is 0.4 of a cell under
    // `CellTextSystem`'s metrics and would shift every wrap boundary.
    editor.set_show_gutter(false, cx);
    editor.set_offset_content(false, cx);
    editor.disable_scrollbars_and_minimap(window, cx);
    editor
}
