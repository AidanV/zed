//! Building the backend `ted` drives (SPEC §9): a real `Workspace` over a real
//! `Project` and `Client`, with vim attached.
//!
//! The init *order* below is the subset of `crates/zed/src/main.rs` that an
//! editing session requires, and it is load-bearing: `theme_settings` needs the
//! `SettingsStore`, `workspace::init` needs an `AppState` whose `Client` has
//! already been through `client::init`, and `vim::init` needs the workspace
//! actions to exist before it registers its own against them.

use std::any::TypeId;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use client::{Client, UserStore};
use editor::Editor;
use fs::{Fs, RealFs};
use gpui::{
    Action as _, App, AppContext as _, Entity, IntoElement as _, Styled as _, Task,
    UpdateGlobal as _, Window, WindowHandle,
};
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::project_settings::ProjectSettings;
use search::{BufferSearchBar, project_search::ProjectSearchBar};
use session::{AppSession, Session};
use settings::{KeybindSource, KeymapFile, Settings as _, SettingsStore};
use terminal_view::terminal_panel::TerminalPanel;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenMode, Pane, Workspace, WorkspaceStore};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

use crate::cell;
use crate::platform::terminal_window_options;

/// The non-persisted override layer (SPEC §9), applied through the settings
/// store's *server* layer, which merges last and so wins over the user's
/// `settings.json` while leaving that file untouched
/// (`crates/settings/src/settings_store.rs`, `recompute_values`).
///
/// `buffer_font_size` and `buffer_line_height` are what make [`crate::cell`]'s
/// constants true; `soft_wrap` is what makes wrap boundaries a function of the
/// terminal width; and `when_closing_with_no_tabs` keeps a close on an empty
/// pane from asking the *window* to close, which `ted` has no use for — an empty
/// pane is a state it sits in, and ending the session is the frame loop's to do
/// (SPEC §24.7). The rest is cosmetic — `ted` paints whatever cell rect the
/// editor reports (SPEC §10.2), so hiding chrome only widens that rect.
///
/// The tab bar is the one piece of chrome that is *on* from M4: `ted` replaces
/// the element with one exactly a cell tall ([`install_pane_tab_bars`]) and
/// paints its own strip into the row Zed's layout then leaves in each pane
/// (SPEC §25.2). The terminal panel's line height is pinned for the same reason
/// the buffer's is — the panel's pty is sized from the element's bounds divided
/// by it, so anything but 1.0 would size the child in something other than
/// `ted`'s own cells (SPEC §25.5) — and its height is pinned because the
/// panel's own default is 320 pixels, which is a fifth of a GUI window and
/// twenty rows of a terminal's twenty-four. That is the GUI-scale assumption
/// §22 warns about, and the answer is the one §5 gives everywhere else: say it
/// in cells. A resize the user makes afterwards is serialised with the
/// workspace and wins over this.
const SETTINGS_OVERRIDE: &str = r#"
    "buffer_font_size": 16,
    "buffer_line_height": { "custom": 1.0 },
    "soft_wrap": "editor_width",
    "when_closing_with_no_tabs": "keep_window_open",
    "tab_bar": { "show": true },
    "terminal": {
        "font_size": 16,
        "line_height": { "custom": 1.0 },
        "default_height": TERMINAL_PANEL_HEIGHT,
        "toolbar": { "breadcrumbs": false },
        "scrollbar": { "show": "never" }
    },
    "status_bar": { "experimental.show": false },
    "toolbar": {
        "breadcrumbs": false,
        "quick_actions": false,
        "selections_menu": false,
        "agent_review": false,
        "code_actions": false
    },
    "scrollbar": { "show": "never" },
    "minimap": { "show": "never" },
    "indent_guides": { "enabled": false },
    "gutter": {
        "runnables": false,
        "bookmarks": false,
        "breakpoints": false,
        "folds": false
    }
"#;

/// Actions that mutate the `BufferFontSize` global, which would change `CELL`
/// underneath a window already sized in the old cells (SPEC §5.5). They are
/// dropped from the keymap and hidden from the command line rather than left to
/// fail an assertion mid-session.
fn filtered_action_names() -> [&'static str; 3] {
    [
        IncreaseBufferFontSize::name_for_type(),
        DecreaseBufferFontSize::name_for_type(),
        ResetBufferFontSize::name_for_type(),
    ]
}

pub struct Backend {
    pub window: WindowHandle<MultiWorkspace>,
    pub workspace: Entity<Workspace>,
    /// Whether vim is attached. The hint screen an empty pane sits on names the
    /// ways out of it, and three of the four are typed into a `:` line a
    /// `--no-vim` session does not have (SPEC §24.7, §24.5).
    pub vim: bool,
}

impl Backend {
    /// The editor the pane is currently showing, or `None` when the active item
    /// is not one (SPEC §9: a missing editor at any step renders an empty
    /// buffer view rather than failing).
    pub fn active_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
    }

    /// Whether every pane is empty.
    ///
    /// No longer an exit condition on its own. Through M3 this was how `:q`
    /// reached the frame loop, which conflated closing a file with ending a
    /// session; M3.5 makes the empty pane a state `ted` sits in and quitting
    /// explicit (SPEC §24.7). It is now half of that test: the session ends
    /// when every pane is empty *and* something asked for it to.
    pub fn is_empty(&self, cx: &App) -> bool {
        self.workspace
            .read(cx)
            .panes()
            .iter()
            .all(|pane| pane.read(cx).items_len() == 0)
    }

    /// The terminal panel, once it has been loaded and added to the dock
    /// (SPEC §25.5). `None` in a session where loading it failed, which costs
    /// the panel and nothing else.
    pub fn terminal_panel(&self, cx: &App) -> Option<Entity<TerminalPanel>> {
        self.workspace.read(cx).panel::<TerminalPanel>(cx)
    }

    /// Whether the keyboard is in the terminal panel rather than in a pane.
    ///
    /// The frame loop asks because `ctrl-c` belongs to whatever owns the
    /// keyboard, and a shell in the panel needs it to mean what it means in a
    /// shell rather than "end the session" (SPEC §25.5).
    pub fn terminal_has_focus(&self, window: &Window, cx: &App) -> bool {
        use gpui::Focusable as _;
        self.terminal_panel(cx)
            .is_some_and(|panel| panel.focus_handle(cx).contains_focused(window, cx))
    }

    pub fn active_pane_search_bar(&self, cx: &App) -> Option<Entity<BufferSearchBar>> {
        self.workspace
            .read(cx)
            .active_pane()
            .read(cx)
            .toolbar()
            .read(cx)
            .item_of_type::<BufferSearchBar>()
    }
}

/// `vim` selects both the settings layer and the keymap layered on top of
/// the base one, so it has to be known before either is applied.
pub fn init(vim: bool, rows: u16, cx: &mut App) -> Result<Arc<AppState>> {
    zlog::init();
    release_channel::init(semver::Version::new(0, 0, 0), cx);
    gpui_tokio::init(cx);
    menu::init();
    zed_actions::init();

    settings::init(cx);
    cx.set_global(db::AppDatabase::new());

    let fs = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);
    // Reads the user's real `settings.json` so language config, tab size,
    // formatters and LSP settings are shared with GUI Zed (SPEC §9), then keeps
    // watching it. `apply_settings_override` runs after this so ted's pins land
    // in a layer the file cannot displace.
    SettingsStore::update_global(cx, |store, cx| {
        store.watch_settings_files(fs.clone(), cx, |_, _, _| {});
    });
    apply_settings_override(vim, rows, cx)?;

    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    if let Err(error) = assets::Assets.load_fonts(cx) {
        // `CellTextSystem` defines its own metrics and ignores real fonts, so
        // this is not fatal — but other bootstrap paths expect it to have run.
        log::warn!("could not load bundled fonts: {error}");
    }

    let client = Client::production(cx);
    cx.set_http_client(client.http_client());
    client::init(&client, cx);
    project::Project::init(&client, cx);

    let mut languages = LanguageRegistry::new(cx.background_executor().clone());
    languages.set_language_server_download_dir(paths::languages_dir().clone());
    let languages = Arc::new(languages);
    let node_runtime = node_runtime(&client, cx);
    languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);
    languages.set_theme(cx.theme().clone());

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
    let session = cx.background_executor().spawn(Session::new(
        uuid::Uuid::new_v4().to_string(),
        db::kvp::KeyValueStore::global(cx),
    ));
    let session = cx.foreground_executor().block_on(session);
    let session = cx.new(|cx| AppSession::new(session, cx));

    let app_state = Arc::new(AppState {
        languages,
        client,
        user_store,
        workspace_store,
        fs,
        build_window_options: |_, cx| terminal_window_options(cx),
        node_runtime,
        session,
    });
    AppState::set_global(app_state.clone(), cx);

    editor::init(cx);
    workspace::init(app_state.clone(), cx);
    search::init(cx);
    command_palette_hooks::init(cx);
    command_palette::init(cx);
    go_to_line::init(cx);
    file_finder::init(cx);
    // Mirrored rather than replaced (SPEC §13.1): each is a `Picker` whose
    // delegate answers in plain text, so `ted` paints the real modal's matches.
    outline::init(cx);
    project_symbols::init(cx);
    // Registers the dock and the actions that open it; the panel itself is
    // loaded per window, in `open` (SPEC §25.5).
    terminal_view::init(cx);
    if vim {
        vim::init(cx);
    }
    crate::actions::init(cx);

    // Again, and this time after `vim::init`: vim installs the `:` interceptor
    // (§13.2) from a `SettingsStore` observer, and an observer only runs when
    // the store *changes* — never for the value it already holds. The override
    // above ran before `vim::init` registered that observer, so without a
    // second settings change here the interceptor is installed only if
    // something else happens to touch settings later (opening a file with a
    // language does; opening a plain-text one does not), and until it is, `:w`
    // and `:q` fall through to matching action names and quietly do nothing.
    apply_settings_override(vim, rows, cx)?;

    hide_filtered_actions(cx);
    install_pane_search_bars(cx);
    install_pane_tab_bars(cx);
    load_keymap(vim, cx)?;
    verify_pinned_metrics(cx).map(|()| app_state)
}

/// Opens `paths` into a workspace whose window is the terminal grid minus the
/// rows `ted` paints itself.
///
/// `Workspace::new_local` rather than hand-rolled window creation, so workspace
/// serialisation, worktree trust and project restoration all behave (SPEC §9).
pub fn open(
    paths: Vec<PathBuf>,
    vim: bool,
    app_state: Arc<AppState>,
    cx: &mut App,
) -> Task<Result<Backend>> {
    let task = Workspace::new_local(paths, app_state, None, None, None, OpenMode::Activate, cx);
    cx.spawn(async move |cx| {
        let opened = task.await?;
        let backend = Backend {
            window: opened.window,
            workspace: opened.workspace,
            vim,
        };

        // Loaded here rather than in `init`, because the panel is a view over a
        // window that does not exist until the workspace is open. It stays
        // closed until something asks for it — loading it only registers the
        // dock it would appear in (SPEC §25.5).
        let panel = cx
            .update_window(backend.window.into(), {
                let workspace = backend.workspace.downgrade();
                move |_, window, cx| TerminalPanel::load(workspace, window.to_async(cx))
            })?
            .await;
        match panel {
            Ok(panel) => {
                cx.update_window(backend.window.into(), {
                    let workspace = backend.workspace.clone();
                    move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.add_panel(panel, window, cx);
                        });
                    }
                })?;
            }
            // A terminal `ted` cannot open is a feature missing from the
            // session, not a session that failed to start.
            Err(error) => log::warn!("the terminal panel could not be loaded: {error}"),
        }

        // Focus explicitly once the workspace exists: combined with
        // `TerminalWindow::is_active() == true`, this is what makes the editor
        // render as focused and routes keystrokes down the intended dispatch
        // path (SPEC §9, "Focus").
        //
        // `update_window` rather than `WindowHandle::update`: the latter leases
        // the root `MultiWorkspace` for the whole closure, and anything that
        // reads it — including `Workspace::for_window` — would then panic.
        cx.update_window(backend.window.into(), {
            let workspace = backend.workspace.clone();
            move |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.focus_active_item(window, cx);
                    });
                });
            }
        })?;

        Ok(backend)
    })
}

/// SPEC §8.2: `ted` loads the Linux base keymap on every OS, because its
/// bindings are control-based and a terminal never receives `cmd-*`. Layered
/// base → vim → user, so a user binding wins, which is the same precedence
/// `zed::load_default_keymap` establishes.
fn load_keymap(vim: bool, cx: &mut App) -> Result<()> {
    bind_asset_keymap("keymaps/default-linux.json", KeybindSource::Default, cx)?;
    if vim {
        bind_asset_keymap(settings::VIM_KEYMAP_PATH, KeybindSource::Vim, cx)?;
    }

    if let Some(user_keymap) = std::fs::read_to_string(paths::keymap_file()).log_err() {
        match KeymapFile::load(&user_keymap, cx) {
            settings::KeymapFileLoadResult::Success { mut key_bindings } => {
                crate::actions::retarget(&mut key_bindings, cx);
                cx.bind_keys(key_bindings);
            }
            settings::KeymapFileLoadResult::SomeFailedToLoad {
                mut key_bindings,
                error_message,
            } => {
                log::warn!("some bindings in keymap.json could not be loaded: {error_message}");
                crate::actions::retarget(&mut key_bindings, cx);
                cx.bind_keys(key_bindings);
            }
            settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                log::warn!("keymap.json could not be parsed, using defaults: {error}");
            }
        }
    }

    Ok(())
}

fn bind_asset_keymap(path: &str, source: KeybindSource, cx: &mut App) -> Result<()> {
    let filtered = filtered_action_names();
    let mut bindings = KeymapFile::load_asset_allow_partial_failure(path, cx)
        .with_context(|| format!("could not load {path}"))?;
    bindings.retain(|binding| !filtered.contains(&binding.action().name()));
    for binding in &mut bindings {
        binding.set_meta(source.meta());
    }
    // After the source is stamped, because retargeting carries it over: a
    // rewritten binding is the same binding with a different action at the end
    // of it (SPEC §24.2).
    crate::actions::retarget(&mut bindings, cx);
    cx.bind_keys(bindings);
    Ok(())
}

/// Keeps out of the `:` line's completions what cannot work there: the
/// font-size actions, which would move `CELL` under a window sized in the old
/// one (SPEC §5.5), and every action that only ever drove a modal `ted` has
/// replaced with a surface of its own (SPEC §24.2). Both resolve through the
/// same `CommandPaletteFilter` Zed's palette consults.
fn hide_filtered_actions(cx: &mut App) {
    let hidden = [
        TypeId::of::<IncreaseBufferFontSize>(),
        TypeId::of::<DecreaseBufferFontSize>(),
        TypeId::of::<ResetBufferFontSize>(),
    ];
    command_palette_hooks::CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&hidden);
        for namespace in crate::actions::REPLACED_NAMESPACES {
            filter.hide_namespace(namespace);
        }
    });
}

/// Vim's `/` and `?` dispatch into a `BufferSearchBar` in the pane's toolbar and
/// silently do nothing without one (SPEC §14.2). Subscribing to `PaneAdded` as
/// well as seeding the first pane means a `:vsplit` gets one too.
fn install_pane_search_bars(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        add_search_bars(workspace.active_pane(), window, cx);
        cx.subscribe_in(
            &cx.entity(),
            window,
            |_, _, event: &workspace::Event, window, cx| {
                if let workspace::Event::PaneAdded(pane) = event {
                    add_search_bars(pane, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();
}

/// Takes one cell row out of every pane for the strip `ted` paints there
/// (SPEC §25.2).
///
/// The strip itself is `ted`'s, because Zed's tab bar is a row of icons and
/// buttons a cell grid has no way to draw. The *space* for it cannot be: a row
/// withheld from the top of the window reaches only the topmost pane, and a
/// split needs the row inside each pane, wherever that pane is. So the element
/// installed here paints nothing and is exactly a cell tall, Zed's own layout
/// insets each pane's item by it, and the strip goes in the row it leaves —
/// which `ted` finds at the top of the pane's reported bounds like every other
/// rect in the projection.
///
/// Every `Pane` in the process, not just the workspace's: the terminal panel's
/// pane (SPEC §25.5) is a `Pane` too, and it gets its strip the same way.
fn install_pane_tab_bars(cx: &mut App) {
    cx.observe_new(|pane: &mut Pane, _, cx: &mut gpui::Context<Pane>| {
        pane.set_render_tab_bar(cx, |_, _, _| {
            gpui::div()
                .w_full()
                .h(cell::CELL_HEIGHT)
                .flex_none()
                .into_any_element()
        });
    })
    .detach();
}

fn add_search_bars(
    pane: &Entity<Pane>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let buffer_search_bar = cx.new(|cx| BufferSearchBar::new(None, window, cx));
            toolbar.add_item(buffer_search_bar, window, cx);
            let project_search_bar = cx.new(|_| ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
        });
    });
}

fn node_runtime(client: &Arc<Client>, cx: &mut App) -> NodeRuntime {
    let (mut sender, receiver) = watch::channel(None);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = &ProjectSettings::get_global(cx).node;
        sender
            .send(Some(NodeBinaryOptions {
                allow_path_lookup: !settings.ignore_system_version,
                allow_binary_download: true,
                use_paths: None,
            }))
            .log_err();
    })
    .detach();

    // The real runtime, not `NodeRuntime::unavailable()`: several language
    // servers cannot install without it, and "LSP silently does nothing" is a
    // worse failure than a slow first start (SPEC §9).
    NodeRuntime::new(client.http_client(), None, receiver)
}

/// A third of the grid, and never fewer than five rows: the panel is worth
/// having only if what is above it is still an editor (SPEC §25.5).
fn terminal_panel_height(rows: u16) -> f32 {
    const MINIMUM_ROWS: u16 = 5;
    f32::from((rows / 3).max(MINIMUM_ROWS)) * f32::from(cell::CELL_HEIGHT)
}

fn apply_settings_override(vim: bool, rows: u16, cx: &mut App) -> Result<()> {
    let settings = SETTINGS_OVERRIDE.replace(
        "TERMINAL_PANEL_HEIGHT",
        &terminal_panel_height(rows).to_string(),
    );
    let overrides = format!("{{ \"vim_mode\": {vim},{settings} }}");
    SettingsStore::update_global(cx, |store, cx| store.set_server_settings(&overrides, cx))
        .context("ted's settings override failed to parse")
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
