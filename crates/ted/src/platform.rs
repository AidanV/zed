//! `TerminalPlatform` (SPEC §6): a `gpui::Platform` that delegates to the OS's
//! headless platform for executors and services, and substitutes a cell-metric
//! text system plus a window whose frame, resize and activation callbacks are
//! real.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use collections::HashMap;
use futures::channel::oneshot;
use parking_lot::Mutex;
use uuid::Uuid;

use gpui::{
    Action, AnyWindowHandle, AppLifecyclePhase, AtlasKey, AtlasTextureId, AtlasTile,
    BackgroundExecutor, Bounds, Capslock, ClipboardItem, CursorStyle, DevicePixels,
    DispatchEventResult, DisplayId, ForegroundExecutor, GpuSpecs, Keymap, Menu, MenuItem,
    Modifiers, OwnedMenu, PathPromptOptions, Pixels, Platform, PlatformAtlas, PlatformDisplay,
    PlatformGestures, PlatformInput, PlatformInputHandler, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Point, PromptButton, PromptLevel,
    RequestFrameOptions, Scene, ScreenCaptureSource, Size, SystemNotification,
    SystemNotificationResponse, Task, ThermalState, TileId, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowButtonLayout, WindowControlArea, WindowParams,
};

use crate::cell::grid_size;
use crate::text_system::CellTextSystem;

/// A `gpui::Platform` for `ted`. Every method not listed below is a mechanical
/// delegation to `inner`, which is the real headless platform for the current
/// OS (calloop on Linux, `CFRunLoopRun` on macOS) — that's what supplies a
/// genuine multithreaded background executor and foreground run loop without
/// `ted` writing its own dispatcher.
pub struct TerminalPlatform {
    inner: Rc<dyn Platform>,
    text_system: Arc<CellTextSystem>,
    display: Rc<TerminalDisplay>,
    window: RefCell<Option<Rc<TerminalWindowState>>>,
}

impl TerminalPlatform {
    /// `columns` x `rows` is the initial terminal grid size, used to size the
    /// single `TerminalDisplay` window-placement logic clamps against.
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            inner: gpui_platform::current_platform(true),
            text_system: Arc::new(CellTextSystem::new()),
            display: Rc::new(TerminalDisplay::new(columns, rows)),
            window: RefCell::new(None),
        }
    }

    /// The state behind the window `open_window` most recently created, if
    /// any. This is how the frame loop (a later task) reaches
    /// `request_frame`, `resize_to_cells` and `activate_once` from outside
    /// GPUI's own call chain.
    pub fn window(&self) -> Option<Rc<TerminalWindowState>> {
        self.window.borrow().clone()
    }
}

impl Platform for TerminalPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.inner.background_executor()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.inner.foreground_executor()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        self.inner.run(on_finish_launching)
    }

    fn quit(&self) {
        self.inner.quit()
    }

    fn restart(&self, binary_path: Option<PathBuf>) {
        self.inner.restart(binary_path)
    }

    fn activate(&self, ignoring_other_apps: bool) {
        self.inner.activate(ignoring_other_apps)
    }

    fn hide(&self) {
        self.inner.hide()
    }

    fn hide_other_apps(&self) {
        self.inner.hide_other_apps()
    }

    fn unhide_other_apps(&self) {
        self.inner.unhide_other_apps()
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.inner.active_window()
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        self.inner.window_stack()
    }

    fn is_screen_capture_supported(&self) -> bool {
        self.inner.is_screen_capture_supported()
    }

    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<anyhow::Result<Vec<Rc<dyn ScreenCaptureSource>>>> {
        self.inner.screen_capture_sources()
    }

    fn open_window(
        &self,
        _handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        // Never delegate this one to `inner`: on macOS, `MacPlatform::open_window`
        // ignores its own `headless` flag and unconditionally builds a real
        // `NSWindow` plus Metal renderer, which would steal keyboard focus from
        // the terminal and fail outright over SSH where there is no
        // window-server session. On every platform the stock headless window
        // also no-ops the frame/resize/activation callbacks the frame loop
        // depends on (§6.1), so a `TerminalWindow` is required either way.
        let state = Rc::new(TerminalWindowState::new(options.bounds, self.display.clone()));
        *self.window.borrow_mut() = Some(state.clone());
        Ok(Box::new(TerminalWindow(state)))
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.inner.window_appearance()
    }

    fn set_window_appearance(&self, appearance: Option<WindowAppearance>) {
        self.inner.set_window_appearance(appearance)
    }

    fn button_layout(&self) -> Option<WindowButtonLayout> {
        self.inner.button_layout()
    }

    fn open_url(&self, url: &str) {
        self.inner.open_url(url)
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.inner.on_open_urls(callback)
    }

    fn register_url_scheme(&self, url: &str) -> Task<Result<()>> {
        self.inner.register_url_scheme(url)
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        self.inner.prompt_for_paths(options)
    }

    fn prompt_for_new_path(
        &self,
        directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        self.inner.prompt_for_new_path(directory, suggested_name)
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        self.inner.can_select_mixed_files_and_dirs()
    }

    fn reveal_path(&self, path: &Path) {
        self.inner.reveal_path(path)
    }

    fn open_with_system(&self, path: &Path) {
        self.inner.open_with_system(path)
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_quit(callback)
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_reopen(callback)
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_system_wake(callback)
    }

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.inner.on_app_lifecycle(callback)
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_memory_warning(callback)
    }

    fn gestures(&self) -> Option<Rc<dyn PlatformGestures>> {
        self.inner.gestures()
    }

    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {
        self.inner.set_menus(menus, keymap)
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.inner.get_menus()
    }

    fn set_dock_menu(&self, menu: Vec<MenuItem>, keymap: &Keymap) {
        self.inner.set_dock_menu(menu, keymap)
    }

    fn perform_dock_menu_action(&self, action: usize) {
        self.inner.perform_dock_menu_action(action)
    }

    fn add_recent_document(&self, path: &Path) {
        self.inner.add_recent_document(path)
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.inner.on_app_menu_action(callback)
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_will_open_app_menu(callback)
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.inner.on_validate_app_menu_command(callback)
    }

    fn thermal_state(&self) -> ThermalState {
        self.inner.thermal_state()
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_thermal_state_change(callback)
    }

    fn set_app_identity(&self, identifier: &str, name: &str) {
        self.inner.set_app_identity(identifier, name)
    }

    fn show_system_notification(&self, notification: SystemNotification) {
        self.inner.show_system_notification(notification)
    }

    fn dismiss_system_notification(&self, tag: &str) {
        self.inner.dismiss_system_notification(tag)
    }

    fn on_system_notification_response(
        &self,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        self.inner.on_system_notification_response(callback)
    }

    fn compositor_name(&self) -> &'static str {
        self.inner.compositor_name()
    }

    fn app_path(&self) -> Result<PathBuf> {
        self.inner.app_path()
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        self.inner.path_for_auxiliary_executable(name)
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        self.inner.set_cursor_style(style)
    }

    fn hide_cursor_until_mouse_moves(&self) {
        self.inner.hide_cursor_until_mouse_moves()
    }

    fn is_cursor_visible(&self) -> bool {
        self.inner.is_cursor_visible()
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        self.inner.should_auto_hide_scrollbars()
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.inner.read_from_clipboard()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        self.inner.write_to_clipboard(item)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.inner.read_from_primary()
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, item: ClipboardItem) {
        self.inner.write_to_primary(item)
    }

    #[cfg(target_os = "macos")]
    fn read_from_find_pasteboard(&self) -> Option<ClipboardItem> {
        self.inner.read_from_find_pasteboard()
    }

    #[cfg(target_os = "macos")]
    fn write_to_find_pasteboard(&self, item: ClipboardItem) {
        self.inner.write_to_find_pasteboard(item)
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        self.inner.write_credentials(url, username, password)
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        self.inner.read_credentials(url)
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        self.inner.delete_credentials(url)
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        self.inner.keyboard_layout()
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        self.inner.keyboard_mapper()
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_keyboard_layout_change(callback)
    }
}

/// The terminal grid's bounds in pixels, standing in for a monitor so
/// window-placement logic has something to clamp against. There is exactly
/// one, and it tracks the current terminal size.
#[derive(Debug)]
pub struct TerminalDisplay {
    bounds: RefCell<Bounds<Pixels>>,
}

impl TerminalDisplay {
    fn new(columns: u16, rows: u16) -> Self {
        Self {
            bounds: RefCell::new(Bounds::new(Point::default(), grid_size(columns, rows))),
        }
    }

    /// Called when the terminal is resized (SIGWINCH), so displayed bounds
    /// stay in step with the window that fills them.
    fn resize(&self, columns: u16, rows: u16) {
        *self.bounds.borrow_mut() = Bounds::new(Point::default(), grid_size(columns, rows));
    }
}

impl PlatformDisplay for TerminalDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        // Stable identity: there is exactly one display, the terminal itself.
        Ok(Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        *self.bounds.borrow()
    }
}

/// The shared state behind a `TerminalWindow`. `ted`'s frame loop (§7) drives
/// GPUI through the inherent methods below; GPUI itself drives it through the
/// `PlatformWindow` impl on the `TerminalWindow` wrapper.
pub struct TerminalWindowState {
    bounds: RefCell<Bounds<Pixels>>,
    display: Rc<TerminalDisplay>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    title: RefCell<Option<String>>,
    is_fullscreen: Cell<bool>,
    atlas: Arc<TerminalAtlas>,
    on_request_frame: RefCell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    on_input: RefCell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    on_active_status_change: RefCell<Option<Box<dyn FnMut(bool)>>>,
    on_resize: RefCell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    // Stored so nothing registered by GPUI is silently dropped, but `ted` has
    // no driver for these yet: there is no OS compositor to report hover, no
    // window manager to move the window, no theme switch to react to, and
    // shutdown (§7) is currently orchestrated from the frame loop rather than
    // through `on_should_close`/`on_close`.
    #[allow(dead_code)]
    on_hover_status_change: RefCell<Option<Box<dyn FnMut(bool)>>>,
    #[allow(dead_code)]
    on_moved: RefCell<Option<Box<dyn FnMut()>>>,
    #[allow(dead_code)]
    on_should_close: RefCell<Option<Box<dyn FnMut() -> bool>>>,
    #[allow(dead_code)]
    on_close: RefCell<Option<Box<dyn FnOnce()>>>,
    #[allow(dead_code)]
    on_appearance_changed: RefCell<Option<Box<dyn FnMut()>>>,
}

impl TerminalWindowState {
    fn new(bounds: Bounds<Pixels>, display: Rc<TerminalDisplay>) -> Self {
        Self {
            bounds: RefCell::new(bounds),
            display,
            input_handler: RefCell::new(None),
            title: RefCell::new(None),
            is_fullscreen: Cell::new(false),
            atlas: Arc::new(TerminalAtlas::default()),
            on_request_frame: RefCell::new(None),
            on_input: RefCell::new(None),
            on_active_status_change: RefCell::new(None),
            on_resize: RefCell::new(None),
            on_hover_status_change: RefCell::new(None),
            on_moved: RefCell::new(None),
            on_should_close: RefCell::new(None),
            on_close: RefCell::new(None),
            on_appearance_changed: RefCell::new(None),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        *self.bounds.borrow()
    }

    /// Sets the window's bounds to the pixel size of `columns` x `rows` and
    /// invokes the callback GPUI registered via `on_resize`, which calls
    /// `Window::bounds_changed` (`crates/gpui/src/window.rs:1674`) — exactly
    /// what SIGWINCH needs to trigger relayout.
    pub fn resize_to_cells(&self, columns: u16, rows: u16) {
        let size = grid_size(columns, rows);
        self.bounds.borrow_mut().size = size;
        self.display.resize(columns, rows);
        self.invoke_on_resize(size, 1.0);
    }

    /// Invokes the callback GPUI registered via `on_request_frame`, the pull
    /// that drives GPUI's whole draw pipeline (§4.2.1, §7): GPUI decides
    /// inside the callback whether the window is dirty enough to redraw.
    pub fn request_frame(&self) {
        self.invoke_on_request_frame(RequestFrameOptions::default());
    }

    /// Fires the `on_active_status_change` callback with `true`. Must be
    /// called once after the window is open and GPUI has registered its
    /// callback (i.e. not from inside `open_window` itself, which returns
    /// before that registration happens) — otherwise GPUI's own `active`
    /// bookkeeping (`window.rs:1719`) never becomes true, which suppresses
    /// cursor blink and focus styling even though `is_active()` reports true.
    pub fn activate_once(&self) {
        self.invoke_on_active_status_change(true);
    }

    /// Dispatches a platform input event through the callback GPUI registered
    /// via `on_input`. Not yet driven by anything (mouse support is future
    /// work), but the plumbing is real so that work has somewhere to call.
    pub fn dispatch_input(&self, input: PlatformInput) -> DispatchEventResult {
        let callback = self.on_input.borrow_mut().take();
        match callback {
            Some(mut callback) => {
                let result = callback(input);
                *self.on_input.borrow_mut() = Some(callback);
                result
            }
            None => DispatchEventResult::default(),
        }
    }

    // These callbacks are `FnMut` trait objects behind a `RefCell`, and
    // invoking one can re-enter this window: GPUI's `on_request_frame`
    // callback runs `Window::draw`, which reads window state, and
    // `on_resize`'s callback runs `Window::bounds_changed`, likewise. Taking
    // the callback out of its `RefCell` before calling it (and putting it
    // back afterward) means a re-entrant call finds the slot empty and
    // no-ops instead of hitting a `BorrowMutError` on an already-borrowed
    // `RefCell`.

    fn invoke_on_request_frame(&self, options: RequestFrameOptions) {
        let callback = self.on_request_frame.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(options);
            *self.on_request_frame.borrow_mut() = Some(callback);
        }
    }

    fn invoke_on_resize(&self, size: Size<Pixels>, scale_factor: f32) {
        let callback = self.on_resize.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(size, scale_factor);
            *self.on_resize.borrow_mut() = Some(callback);
        }
    }

    fn invoke_on_active_status_change(&self, active: bool) {
        let callback = self.on_active_status_change.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(active);
            *self.on_active_status_change.borrow_mut() = Some(callback);
        }
    }
}

/// GPUI's window handle. Wraps the shared `TerminalWindowState` so that
/// `TerminalPlatform` can keep driving the same window from outside GPUI's
/// own call chain after handing this box to GPUI.
struct TerminalWindow(Rc<TerminalWindowState>);

impl raw_window_handle::HasWindowHandle for TerminalWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        // The terminal is not backed by a native window.
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl raw_window_handle::HasDisplayHandle for TerminalWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for TerminalWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.bounds()
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.0.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.0.bounds().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        self.0.bounds.borrow_mut().size = size;
    }

    fn scale_factor(&self) -> f32 {
        // Cells, not device pixels, are the unit of account (SPEC §5).
        1.0
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.0.input_handler.borrow_mut() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.input_handler.borrow_mut().take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // Fall back to GPUI's own rendered prompts (SPEC §13).
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        // A window that reports itself inactive gets throttled to 30fps by
        // GPUI and suppresses cursor blink and focus styling; ted's terminal
        // window is conceptually always focused.
        true
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        *self.0.title.borrow_mut() = Some(title.to_owned());
    }

    fn get_title(&self) -> String {
        self.0.title.borrow().clone().unwrap_or_default()
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {
        self.0.is_fullscreen.set(!self.0.is_fullscreen.get());
    }

    fn is_fullscreen(&self) -> bool {
        self.0.is_fullscreen.get()
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.0.on_request_frame.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.0.on_input.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.0.on_active_status_change.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.0.on_hover_status_change.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.0.on_resize.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.0.on_moved.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.0.on_should_close.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(
        &self,
        _callback: Box<dyn FnMut() -> Option<WindowControlArea>>,
    ) {
        // No custom titlebar controls to hit-test.
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.0.on_close.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.0.on_appearance_changed.borrow_mut() = Some(callback);
    }

    fn draw(&self, _scene: &Scene) {
        // The discard named by SPEC §4.2.1: `Window::present` hands the fully
        // built `Scene` to this call, and this is where it stops. Nothing
        // reads `_scene` because ted paints the terminal grid from GPUI's
        // element tree (§10), never from rasterized primitives.
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

/// Allocates atlas tiles without uploading pixels, so glyph and sprite
/// painting completes without a real texture (copied from `gpui_linux`'s
/// `HeadlessAtlas`, `crates/gpui_linux/src/linux/headless/window.rs`). One
/// instance is shared by every `draw` call on a given window so tiles are
/// actually reused rather than reallocated per frame.
#[derive(Default)]
struct TerminalAtlas(Mutex<TerminalAtlasState>);

#[derive(Default)]
struct TerminalAtlasState {
    next_id: u32,
    tiles: HashMap<AtlasKey, AtlasTile>,
}

impl PlatformAtlas for TerminalAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<AtlasTile>> {
        {
            let state = self.0.lock();
            if let Some(&tile) = state.tiles.get(key) {
                return Ok(Some(tile));
            }
        }

        let Some((size, _)) = build()? else {
            return Ok(None);
        };

        let mut state = self.0.lock();
        state.next_id += 1;
        let texture_id = state.next_id;
        state.next_id += 1;
        let tile_id = state.next_id;
        let tile = AtlasTile {
            texture_id: AtlasTextureId {
                index: texture_id,
                kind: key.texture_kind(),
            },
            tile_id: TileId(tile_id),
            padding: 0,
            bounds: Bounds {
                origin: Point::default(),
                size,
            },
        };
        state.tiles.insert(key.clone(), tile);
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        self.0.lock().tiles.remove(key);
    }
}
