# `ted` — a terminal UI for Zed

**Status:** M0, M1 and M2 implemented (§21). M3 is next
**Scope:** a new crate + binary in this repository that presents Zed's editor as a
full-screen terminal application, using Ratatui for presentation and Zed's own
`editor` + `vim` + `workspace` + `project` crates for all behaviour.

---

## 1. Goals

1. **Real Zed, not a lookalike.** Every buffer edit, motion, selection, undo
   entry, fold, soft-wrap decision, LSP interaction and vim keybinding is
   executed by the crates that ship in Zed today (`editor`, `vim`,
   `multi_buffer`, `language`, `project`, `workspace`). `ted` contributes
   presentation and input translation only.
2. **Editor and vim motions are the parity bar.** If a motion, text object,
   operator, register, mark, macro or `:` command works in Zed, it works in
   `ted`.
3. **MVP ergonomics of `nano` / macOS `edit`.** Open a file, type, save, quit.
   A single full-screen editor plus one status line. No panels, no icons, no
   tabs required for M1.
4. **Additive.** A new crate `crates/ted` with its own `[[bin]]`. No behavioural
   change to `zed` the GUI app. Changes to existing crates are limited to
   additive, defaulted accessors (§20).
5. **Collaboration-ready by construction.** Because `ted` hosts a real
   `Project` and `Workspace` backed by a real `client::Client`, joining a
   shared project is a matter of rendering collaborator state that already
   flows into the entities we already read (§18).

## 2. Non-goals

- Reimplementing Zed's chrome (icons, tabs, docks, agent panel, git panel).
  These may be added incrementally; none are required for MVP.
- A generic "GPUI renders to a terminal" backend. Explicitly rejected in §4.3.
- Pixel-accurate visual parity. `ted` is a character-cell projection of Zed's
  state, not a downsampled screenshot.
- Mouse-first workflows. Mouse support is a stretch goal (§17).

## 3. Naming and layout

```
crates/ted/
  Cargo.toml            # [lib] path = "src/ted.rs", [[bin]] name = "ted"
  SPEC.md               # this document
  src/
    ted.rs              # lib root: public entry `ted::run(args)`
    main.rs             # thin bin: arg parsing -> ted::run
    platform.rs         # TerminalPlatform, TerminalWindow, TerminalDisplay
    text_system.rs      # CellTextSystem (§5.6)
    bootstrap.rs        # App/AppState/Workspace construction (§9)
    config.rs           # ted.json: the settings that are ted's alone (§9)
    frame.rs            # frame loop, dirty tracking, present (§7)
    input.rs            # terminal event -> gpui::Keystroke / PlatformInput (§8)
    actions.rs          # ted's own actions, and the surface table (§24.2)
    overlay.rs          # the file finder and the buffer switcher (§24.1)
    hover.rs            # diagnostics and documentation on demand (§24.8)
    suspend.rs          # handing the terminal to a child process (§7.1)
    explore.rs          # :Explore over a file manager (§13.4)
    snapshot.rs         # ViewSnapshot: the backend -> frontend projection (§10)
    render/
      render.rs         # ratatui root
      buffer.rs         # editor text area + gutter (§11)
      status.rs         # status line, mode indicator, pending keys
      command_line.rs   # `:` and `/` lines (§13)
      overlay.rs        # modal projection (§13)
    palette.rs          # theme Hsla -> terminal colors (§12)
```

`ted` stands for **T**UI **Z**ed. It is short, does not collide with an
existing crate, and nothing in this document depends on it. Rename before
merge if desired.

### 3.1 Implementation conventions

`ted` follows the repository's Rust guidelines in `CLAUDE.md`. The ones this
design will repeatedly brush against:

- **Comments explain non-obvious "why", nothing else.** No organisational
  headers, no comments that restate the code. The places in `ted` that warrant a
  comment are exactly the ones where the reason is invisible locally: why
  `advance` and `typographic_bounds` must agree (§5.1), why a fold width is
  rounded up (§5.4), why `open_window` is not delegated (§6), why a paste is one
  edit rather than replayed keystrokes (§8.2).
- **Document current behaviour, not history.** No "this replaces", no "formerly",
  no comparisons to an earlier iteration — in code, doc comments, or this
  document. §4.3 records rejected *approaches* because it prevents re-deciding;
  it is not a changelog.
- **No `unwrap()`, no `expect()` on fallible paths, no silently discarded
  errors.** This matters more than usual here: `ted` owns the terminal's mode,
  so an unhandled panic leaves the user with a broken shell. Propagate with `?`
  and surface failures as notifications (§13.3).
- `[lib] path = "src/ted.rs"`, no `mod.rs` files, full words for identifiers,
  shadowed clones for async captures.
- Build with `./script/clippy`.

---

## 4. The central architectural decision

### 4.1 GPUI's role

GPUI is load-bearing here, and which parts of it are load-bearing shapes
everything else.

`editor::Editor` is a GPUI entity. `vim::Vim` is an addon on that entity
(`editor.read(cx).addon::<VimAddon>()`, `crates/vim/src/vim.rs:342`). Vim's
own actions are registered against `workspace::Workspace`
(`crates/vim/src/vim.rs:291`), and `Vim::workspace()` resolves through
`Workspace::for_window(window, cx)` (`crates/vim/src/vim.rs:1058`). Almost
every editor and vim entry point takes `&mut Window`. Three consequences:

- **Actions require a rendered element tree.** GPUI resolves keystrokes against
  the `DispatchTree` built during the previous frame's paint. `Editor`'s
  actions are registered inside its element (`register_action` →
  `window.on_action`, `crates/editor/src/element.rs:10630`). If nothing draws,
  no keybinding resolves (§4.2.1 has the exact call chain, and why it's paint
  specifically, not layout, that this depends on).
- **Layout produces state the editor needs.** Soft-wrap width, visible line
  count and horizontal viewport are computed during
  `EditorElement::prepaint` (`crates/editor/src/element.rs:8050-8055`,
  `calculate_wrap_width` at `:10648`), then written back onto the editor.
  Autoscroll requests are resolved there too. Skipping layout means no
  wrapping, no `visible_line_count`, no autoscroll, no `ctrl-d`/`ctrl-u`.
- **Focus, IME text insertion and pending-keystroke state live on `Window`.**
  Typing a printable character reaches the editor through
  `Window::dispatch_keystroke` → `PlatformInputHandler::dispatch_input`
  (`crates/gpui/src/window.rs:4832-4856`).

So GPUI stays — as the **state, action and layout engine**. What we discard is
GPUI's *painting*: the scene it produces is thrown away, unread.

### 4.2 The chosen model: layout by Zed, paint by Ratatui

```
                    ┌──────────────────────────────────────────────┐
                    │  terminal (crossterm)                        │
                    └───────────┬──────────────────────┬───────────┘
             key/mouse/resize   │                      │  cell grid
                                ▼                      ▲
  ┌──────────────────────────────────────────────────────────────────┐
  │ ted                                                              │
  │  input.rs ──► Keystroke/PlatformInput      ViewSnapshot ──► render│
  │        │                                          ▲              │
  │        │  TerminalPlatform / TerminalWindow       │              │
  └────────┼──────────────────────────────────────────┼──────────────┘
           ▼                                          │
  ┌──────────────────────────────────────────────────────────────────┐
  │ GPUI (headless: real entities, real layout, discarded scene)      │
  │   Window ── DispatchTree ── focus ── element tree                 │
  └──────────────────────────────────────────────────────────────────┘
           │                                          │
  ┌────────┴──────────────────────────────────────────┴──────────────┐
  │ Zed: MultiWorkspace / Workspace / Pane / Editor / Vim /           │
  │      MultiBuffer / DisplayMap / Project / LanguageRegistry / Client│
  └──────────────────────────────────────────────────────────────────┘
```

Each frame:

1. Terminal events are translated to `gpui::Keystroke` / `PlatformInput` and
   dispatched into the headless `Window` exactly as a real platform would.
2. GPUI lays out the real Zed element tree at a size derived from the terminal
   grid, and discards the resulting scene (§4.2.1).
3. `ted` reads a **`ViewSnapshot`** (§10) out of the entities — display rows,
   syntax chunks, selections, cursor, scroll offset, vim mode, active modal —
   and paints it with Ratatui.

The backend is authoritative for *geometry*; `ted` mirrors it rather than
dictating it. Geometry cannot drift between the two, and drawing tabs, docks or
a project panel later needs no new mechanism: those elements report their cell
rects the same way. `ted` tells the backend the terminal size, which is the
window size, and reads cell rectangles back (§10.2).

### 4.2.1 What "layout, then discard" means precisely

Step 2 is doing two GPUI-internal passes at once, and the shipped code never
lets them be pulled apart. The call chain from a platform redraw event down to
the actual discard:

| Stage | What runs | Citation |
|---|---|---|
| Platform registers the frame callback | `platform_window.on_request_frame(callback)` | `crates/gpui/src/window.rs:1551`; trait method at `platform.rs:837` |
| Platform invokes it on vsync/redraw | e.g. macOS's `CVDisplayLink` calling the stored closure | `gpui_macos/src/window.rs:2701-2831` |
| Callback builds the frame | `window.draw(cx)` | entry point `window.rs:2811`; its own doc comment reads "Produces a new frame ... To actually show the contents ... use `Self::present`" |
| ↳ layout (`DrawPhase::Prepaint`) | `draw_roots`: `root_element.request_layout` then `prepaint_as_root` | `window.rs:2987-3054` |
| ↳ paint (`DrawPhase::Paint`) | `draw_roots`: `root_element.paint` | `window.rs:3057-3071` |
| Callback shows the frame | `window.present()` | `window.rs:1656`; body at `window.rs:2960` |
| ↳ hand the `Scene` to the platform | `self.platform_window.draw(&self.rendered_frame.scene)` | `window.rs:2962` |
| `ted`'s override of that call — the actual discard | `TerminalWindow::draw(&Scene)` is a no-op | §6.1 |

`Window::draw` only *builds* `Scene` into `self.rendered_frame`; nothing is
shown yet, by GPUI's own design — that separation between "build the frame"
and "show the frame" is an existing seam `ted` reuses, not one it has to carve
out. `Window::present` is the single call site that hands the finished `Scene`
to the platform. On a real platform that reaches a Metal or wgpu renderer
(`gpui_macos/src/metal_renderer.rs:446`;
`gpui_linux/src/linux/{x11,wayland}/window.rs:1703`/`:1707`) which walks the
scene and issues GPU commands. `TerminalWindow` implements that same trait
method and does nothing with its argument — so
"discards the resulting scene" names one specific call site, not a general
skipping of work.

**Layout and paint are separable at the `Element` API, but not in the pipeline
`ted` runs.** `Element::request_layout`, `::prepaint` and `::paint`
(`crates/gpui/src/element.rs:73-104`) are independent trait methods, and
`AnyElement::layout_as_root` (`element.rs:499-549`, `:632-639`) does call
layout without ever touching paint. But `draw_roots` — the function
`Window::draw` actually calls — runs prepaint and paint back to back
unconditionally; there is no parameter or branch that stops after prepaint.
The only "don't draw" switch that exists is `GpuiMode::Test { skip_drawing }`
(`app.rs:651-673`), a test-only escape hatch that skips both passes together,
not paint alone. So `ted` cannot get "layout without painting" for free through
the normal `request_frame` path.

**That turns out to be fortunate: `ted` needs paint to run anyway.**
`EditorElement` registers vim's and the editor's keybindings into the dispatch
tree from inside `paint`, not prepaint — `register_actions` /
`register_key_listeners` are called at `element.rs:9478-9479`, which call the
free function `register_action` (`element.rs:10630`) → `window.on_action`
(`window.rs:5814`), and `on_action` opens with
`self.invalidator.debug_assert_paint()` (`window.rs:5819`) — calling it from
prepaint would trip that assertion. So §4.1's "actions require a rendered
element tree" is stricter than "layout": specifically the *paint* phase must
run every frame for vim's keybindings to exist in the dispatch tree at all.
This is also why §7's frame loop cannot economize by asking GPUI to
layout-only on a quiet frame — the moment a keystroke needs to resolve, paint
must have run since the last input.

By contrast, `Editor::last_bounds` (§10.2) — the geometry `ted`'s
`ViewSnapshot` reads — is written during *prepaint*, before paint starts:
`editor.last_bounds = Some(bounds)` at `element.rs:8048`, inside
`EditorElement::prepaint` (`element.rs:7971-9456`). So the geometry `ted`
depends on is already committed by the time the (discarded) painting begins;
paint's only effect `ted` relies on is the dispatch-tree registration above,
not any further geometry.

**What's actually thrown away, and why that costs almost nothing extra.**
`Scene` (`crates/gpui/src/scene.rs:41-53`) is seven parallel vectors —
`shadows`, `quads`, `paths`, `underlines`, `monochrome_sprites`,
`subpixel_sprites`, `polychrome_sprites`, `surfaces` — each populated through
`Scene::insert_primitive` (`scene.rs:87`). Text specifically:
`Window::paint_glyph` (`window.rs:4143`) runs once per shaped glyph, called
from `text_system/line.rs:535`, and rasterizes into the sprite atlas before
pushing a `MonochromeSprite` / `SubpixelSprite` that carries a `tile:
AtlasTile` (`scene.rs:711-764`) — by the time a glyph reaches `Scene` it is
already an opaque atlas-tile handle, not even a `GlyphId` any more; the
`GlyphId` + `FontId` pair exists only transiently as the atlas lookup key
(`RenderGlyphParams`, `text_system.rs:1023`). This is the same fact §4.3's
rejected-approaches table invokes against painting the real `Scene` as text —
by the time a primitive is in `Scene`, there is nothing left to reverse-map.
It is also why discarding it costs `ted` almost nothing beyond the layout and
paint traversal itself: `CellTextSystem::rasterize_glyph` (§5.6) is a 1×1
transparent-tile stub, so every atlas lookup `paint_glyph` performs is
trivial, and the `Scene` left behind is a handful of small vectors of
already-cheap primitives, not a rendered framebuffer. The real cost of step 2
is the traversal — accounted for in §19's frame budget — not the scene it
happens to leave behind.

### 4.3 Rejected approaches

Recorded so they are not re-litigated.

| Alternative | Why rejected |
|---|---|
| Implement GPUI's renderer against a cell grid, i.e. paint Zed's real element tree as text | The `Scene` carries `GlyphId`s and quads, not characters. Text is unrecoverable from it without reverse-mapping the font's cmap, and layout designed for 1px granularity does not degrade gracefully to cell granularity. |
| Reimplement the editor over `text`/`multi_buffer`/`language` only, no GPUI | Loses `DisplayMap` (folds, inlays, wrapping, block decorations), all of `editor`, and all of `vim`. That is the entire value proposition. |
| Drive Zed over a wire protocol from a separate frontend process (like `remote_server`) | Attractive for isolation, but the protocol surface needed for editor rendering is large and there is no existing message set for it. Recommended as a *later* refactor (§21, M5) once `ViewSnapshot` has stabilised in-process. The `ViewSnapshot` boundary is deliberately designed to be serialisable so this stays open. |
| Reuse `gpui::HeadlessAppContext` (`crates/gpui/src/app/headless_app_context.rs`) | It is exactly the right shape — `TestPlatform` + pluggable `PlatformTextSystem` + real windows — but it is built on `TestDispatcher`, a deterministic single-threaded simulator. An editor doing LSP, git and file watching needs a real thread pool. Use it for *tests* (§20.2), not for the app. |
| Use the stock headless platform unmodified (`gpui_platform::headless()`) | Viable on Linux only. `HeadlessWindow` (`crates/gpui_linux/src/linux/headless/window.rs`) no-ops `on_request_frame`, `on_input`, `on_resize` and `on_active_status_change`, and exposes no way to resize; and on macOS `MacPlatform::new(true)` still opens a real `NSWindow` from `open_window`, so a GUI window would appear behind the TUI. We need our own window regardless. |

---

## 5. The cell-metric contract

The load-bearing mechanism, and the one most likely to be got wrong. Zed
computes in `Pixels`; a terminal computes in cells. `ted` makes the two coincide
by defining the pixel.

### 5.1 What the text system fixes

`gpui` derives its typographic quantities from the platform text system at a
given font size, so the ratios are what `CellTextSystem` fixes, and the absolute
cell size follows from the font size:

| gpui call | routes to | `CellTextSystem` returns |
|---|---|---|
| `TextSystem::em_width` (`crates/gpui/src/text_system.rs:226`) | `typographic_bounds(font_id, 'm')` | one cell |
| `TextSystem::em_advance` (`:233`) | `advance(font_id, glyph)` | one cell |
| `TextSystem::ch_width` / `ch_advance` (`:240`, `:247`) | same two | one cell |
| `TextSystem::layout_width(font_id, size, ch)` (`:208`) | `layout_line` on a one-char string | `unicode_width(ch)` cells |
| shaped line width, glyph positions | `layout_line` | cell multiples |

**`em_width` and `em_advance` come from different trait methods**
(`typographic_bounds` and `advance` respectively) and both are consumed by
editor layout — `calculate_wrap_width` takes `em_width`
(`crates/editor/src/element.rs:10648`) while `set_visible_column_count` divides
by `em_advance` (`:8055`). If the two disagree, wrap width and column count
disagree, and the disagreement is silent. They must return the same value.

Both are size-scaled by gpui, so `CellTextSystem` fixes a ratio rather than an
absolute width. With an advance of half an em:

```
CELL_W = buffer_font_size / 2
CELL_H = buffer_line_height_multiplier * buffer_font_size
```

`line_height` does **not** come from font metrics: `TextStyle::line_height` is a
`DefiniteLength`, set to `relative(settings.buffer_line_height.value())`
(`crates/editor/src/editor.rs:10923`), so `CELL_H` is a pure function of the two
settings. Font sizes are clamped to `[6, 100]` px
(`crates/theme_settings/src/settings.rs:18`), so the pinned values below sit
comfortably inside the legal range.

`ted` pins `buffer_font_size = 16` and `buffer_line_height = custom(1.0)`,
giving `CELL_W = 8`, `CELL_H = 16` — a 1:2 aspect ratio, so any Zed chrome later
rendered lands on a sane row count. `ted` then *derives* `CELL` from the live
effective values each frame rather than hardcoding them, and asserts they match
the pinned ones. That assertion is the tripwire for §5.5.

### 5.2 What the identities buy

With window content size `size(cols * CELL_W, rows * CELL_H)`:

| Zed quantity | becomes |
|---|---|
| `calculate_wrap_width(EditorWidth, editor_width, em_width)` | wrap at `floor(editor_width / CELL_W)` columns |
| `set_visible_line_count(text_height / line_height)` (`element.rs:8050`) | the number of terminal text rows |
| `set_visible_column_count(editor_width / em_advance)` (`:8055`) | the number of terminal text columns |
| `Editor::scroll_position()` | scroll offset in display rows / columns |
| a `Bounds<Pixels>` reported by an element | a cell rect after dividing by `CELL` (§5.3) |
| double-width CJK, emoji | 2 cells in Zed *and* in the terminal, because both consult `unicode-width` |

Autoscroll, `zz`, `H`/`M`/`L`, `ctrl-d`/`ctrl-u`, `scrolloff` and mouse hit
testing then need no code in `ted` at all.

**Why not a real monospace font.** With a true monospace face `em_width` is
uniform and wrap columns land correctly, until a fallback font is reached: a CJK
or emoji fallback's advance is whatever that font says, not exactly two cells, so
Zed's column model diverges from the terminal's and the cursor lands in the wrong
cell on any line containing wide characters. Defining the metrics removes the
failure class and makes rendering independent of which fonts exist on the
machine.

### 5.3 Soft wrap is per-character, and rects are not cell-aligned

Two properties of the wrap implementation matter, both of which the design
depends on:

- `LineWrapper::wrap_line` accumulates **per-character** widths from
  `width_for_char` → `TextSystem::layout_width`
  (`crates/gpui/src/text_system/line_wrapper.rs:487`), not shaped-cluster widths.
  This is what makes `unicode-width` the right measure: a combining mark reports
  width 0, which is exactly what wrapping needs, and a cluster's cells are the
  sum of its chars' cells.
- The break test is `width > wrap_width`, strictly greater (`:106`). A line of
  exactly `N` cells against a wrap width of `N * CELL_W` does **not** wrap. `N`
  columns fit, as a terminal user expects.

Element rects, however, are **not** guaranteed to be cell multiples. Flex
padding, gutter margins and `rems`-derived spacing can land on fractional pixel
values, so `ted` floors reported origins and sizes into cells rather than
asserting exactness. This is safe because the two things that must be exact are
exact for a different reason: wrap boundaries fall between characters whose
widths are whole cells, and `floor(editor_width / CELL_W)` is the same column
count the wrapper used. Where a rect and a column count could disagree, the
floored column count is authoritative.

`visible_line_count` is an `f64` and may be fractional (a partial bottom row).
`ted` renders `floor` rows and drops the partial one; the editor already handles
fractional visible counts.

### 5.4 Grapheme columns, and widths `ted` does not control

`DisplayPoint::column()` is a **byte** offset within the display row, as
everywhere in Zed's `text` crate. Converting byte column → cell column means
walking the row's graphemes and summing `unicode-width`. That conversion is
computed once per visible row per frame and cached in the `ViewSnapshot` as
`byte_to_cell` (§10.1); every consumer — cursor, selections, highlights — reads
that one table. No ad-hoc conversions anywhere in `render/`.

Tabs need no handling at render time: `DisplayMap`'s `TabMap` has already
expanded them to spaces, so the text reaching `layout_line` and the renderer
contains no `\t`.

Two width sources are outside `CellTextSystem`'s control and can be fractional:

- **Fold and inlay placeholder widths.** `DisplayMap::update_fold_widths`
  (`crates/editor/src/display_map.rs:1250`) is fed widths measured from rendered
  elements, and `LineWrapper` consumes them as `WrapBoundaryCandidate::Element`
  (`line_wrapper.rs:92`). A fold rendered as plain text measures to whole cells;
  one rendered as a padded button does not. `ted` renders folds and inlays as
  text and rounds any width it supplies up to a whole cell.
- **Single-line editors.** `Editor::create_style` gives `SingleLine` and
  `AutoHeight` modes the *UI* font at `rems(0.875)`
  (`crates/editor/src/editor.rs:10906-10915`), so their cell size differs from
  the buffer editor's. `ted` does not render them — the `/` and `:` lines are its
  own (§13.2) — and must not assume one global `CELL` when reading any rect that
  belongs to one.

### 5.5 Runtime font-size changes

`editor::IncreaseBufferFontSize` and friends mutate a `BufferFontSize` global
(`crates/theme_settings/src/settings.rs:599`), which would silently change
`CELL` underneath a window sized in the old cells. `ted` filters these actions
out of its keymap and hides them from the command line, and the per-frame
derivation in §5.1 asserts the pinned values so a settings-file change is a loud
failure rather than a drifting cursor.

### 5.6 `CellTextSystem`

`src/text_system.rs` implements `gpui::PlatformTextSystem`
(`crates/gpui/src/platform.rs:1053`, 13 methods), modelled on
`gpui::NoopTextSystem` (`:1097`) with real width logic:

- `font_id` — interns every `Font` descriptor; all ids are equivalent.
- `font_metrics` — fixed `units_per_em`, ascent and descent. These do not affect
  line height (§5.1) but do affect baseline arithmetic in callers, so they must
  be self-consistent.
- `advance` and `typographic_bounds` — `unicode_width(char) * CELL_W` in em
  units, from a `GlyphId -> char` table populated by `glyph_for_char`. The two
  must agree (§5.1).
- `layout_line` — segments into grapheme clusters, emits one `ShapedGlyph` per
  cluster at the cluster's start byte index and `x = accumulated_cells * CELL_W`,
  and returns a `LineLayout` of width `total_cells * CELL_W`. One glyph per
  cluster makes `x_for_index` inside a cluster resolve to the cluster start,
  which is the correct cursor behaviour.
- `rasterize_glyph` / `glyph_raster_bounds` — a 1×1 transparent tile. Still
  called, because the atlas allocates tiles during paint, so they must succeed
  rather than error.
- `add_fonts` — accept and ignore. `Assets::load_fonts`
  (`crates/assets/src/assets.rs:42`) still runs during bootstrap because other
  code paths expect it to have.

`unicode-width` appears in exactly two places: here, for measurement, and in the
renderer, for placement. A unit test asserts the two agree over a corpus (§20.3).

## 6. `TerminalPlatform`

`src/platform.rs`. A `gpui::Platform` implementation that **delegates to the
OS's headless platform** for everything it doesn't care about, and overrides
four things.

```rust
pub struct TerminalPlatform {
    inner: Rc<dyn gpui::Platform>,        // gpui_platform::current_platform(true)
    text_system: Arc<CellTextSystem>,
    window: RefCell<Option<Rc<TerminalWindowState>>>,
    clipboard: RefCell<Clipboard>,
}
```

Delegated verbatim: `background_executor`, `foreground_executor`, `run`,
`quit`, credentials, `keyboard_layout`, `keyboard_mapper`,
`on_keyboard_layout_change`, `app_path`, `open_url`, `reveal_path`,
`open_with_system`, thermal state, and the remaining ~50 methods of the trait.
Delegation is mechanical one-liners; keeping the OS platform underneath is what
gives us a **real multithreaded background executor** and a real foreground
run loop (calloop on Linux, `CFRunLoopRun` on macOS in headless mode —
`crates/gpui_macos/src/platform.rs:491`) without writing a dispatcher.

Both delegated run loops are **blocking**: `LinuxPlatform::run` enters
`LinuxClient::run` (`crates/gpui_linux/src/linux/platform.rs:265`) and
`MacPlatform::run` enters `CFRunLoopRun` when headless. That is fine — `ted`
never wants the main loop; it wants a foreground task inside it (§7). It does
mean `Application::run_embedded` is **not** usable with either platform, so
`ted` uses plain `Application::run`.

Overridden:

1. **`text_system()`** → `CellTextSystem` (§5.6).
2. **`open_window()`** → a `TerminalWindow` of our own, never the inner
   platform's. The primary reason is platform-independent (§6.1): the stock
   headless window no-ops the frame, resize and activation callbacks the frame
   loop depends on. Secondarily, `open_window` must not be delegated on macOS —
   `MacPlatform::open_window` (`crates/gpui_macos/src/platform.rs:642`) ignores
   its own `headless` flag and calls `MacWindow::open` unconditionally, which
   builds a real `NSWindow` plus a Metal renderer. That would steal keyboard
   focus from the terminal, and fails outright over SSH where the process has no
   window-server session.
3. **`read_from_clipboard` / `write_to_clipboard`** (and `*_primary` on Linux)
   → §16.
4. **`displays()` / `primary_display()`** → a single `TerminalDisplay` whose
   bounds are the terminal grid in pixels, so window-placement logic has
   something sane to clamp against.

### 6.1 `TerminalWindow`

Modelled closely on `crates/gpui_linux/src/linux/headless/window.rs` (287
lines including its atlas — read it first; most of it is directly reusable) but
with the no-op callbacks made real. It must:

- Store and invoke `on_request_frame` (§7).
- Store and invoke `on_resize` — GPUI's registered callback calls
  `window.bounds_changed(cx)` (`crates/gpui/src/window.rs:1674`), which is
  exactly what SIGWINCH needs to trigger relayout.
- Report `is_active() == true` and fire `on_active_status_change(true)` once
  after open. A window that claims to be inactive gets its frame rate throttled
  to 30fps by GPUI (`window.rs:1590`) and suppresses cursor blink and focus
  styling.
- Keep `set_input_handler` / `take_input_handler` working (the headless window
  already does; this is what makes printable-character insertion work through
  `dispatch_keystroke`).
- Return `scale_factor() == 1.0`. Never introduce a device-pixel ratio; cells
  are already the unit of account.
- Provide a non-trait `resize_to_cells(cols, rows)` used by `ted` on SIGWINCH,
  which sets bounds and invokes the stored `on_resize` callback.
- `draw(&Scene)` — discard. `sprite_atlas()` — the tile-allocating stub from
  `headless/window.rs`, copied.
- `prompt()` → a real `oneshot::Receiver<usize>`, queueing the question for the
  frame loop to paint and answer (§13.3). Never `None`: that is what asks GPUI
  to render the prompt into the window instead, where nothing paints it and it
  still takes focus.

Window is opened with `WindowOptions { focus: true, show: false, window_bounds:
Some(Windowed(terminal_grid_in_px)), .. }`.

---

## 7. Frame loop

GPUI's redraw model is **pull**: the platform invokes the `on_request_frame`
callback, and GPUI decides inside it whether the window is dirty enough to
redraw (`crates/gpui/src/window.rs:1550-1600`). `ted` owns the pull.

The OS run loop stays in charge of the main thread (`Application::run`).
Terminal I/O is bridged in:

```
┌ reader thread ┐   crossterm::event::read() -> mpsc::UnboundedSender<TermEvent>
└───────────────┘
┌ foreground task (cx.spawn) ┐
│  loop {                                                                 │
│    select! {                                                            │
│      ev = events.next()  => dispatch(ev),   // §8                       │
│      _  = frame_timer    => {}                                          │
│    }                                                                    │
│    request_frame();          // invoke TerminalWindow's stored callback  │
│    if snapshot_changed() { paint(); }        // §10, §11                 │
│  }                                                                      │
└─────────────────────────────────────────────────────────────────────────┘
```

- The reader thread is a plain `std::thread` holding a
  `futures::channel::mpsc::UnboundedSender` (`Send`), consumed by a foreground
  GPUI task. This is portable and avoids integrating file descriptors into
  three different OS run loops. **This bridge is the main cross-platform risk
  and must be validated on both Linux and macOS in M0** (§21): the foreground
  executor's wake path differs (calloop channel vs. `CFRunLoop` source), and a
  channel send that fails to wake the run loop presents as a TUI that only
  redraws when another timer happens to fire.
- `frame_timer` is `cx.background_executor().timer(..)`. Its cadence is
  adaptive: ~8ms while input is arriving or an animation/blink is pending,
  backing off to ~100ms when idle. Calling `request_frame()` when nothing is
  dirty is cheap — GPUI early-returns without laying out.
- `paint()` is gated on a cheap change check. A full Zed layout of an 80×24
  window is on the order of a millisecond; a terminal repaint plus flush is the
  more expensive half, so the gate matters more for output than for layout. Two
  signals: (a) whether GPUI actually redrew, and (b) a hash of the
  `ViewSnapshot`'s scalar fields plus the display map's version. Start with (b)
  alone; it is simple and correct.
- Blink: rather than mirroring `editor::BlinkManager`, place the *real*
  terminal cursor at the primary cursor's cell and let the terminal blink it.
  This also makes `ted` behave correctly with screen readers and terminal
  cursor-shape escapes (block in normal mode, bar in insert mode).
- On quit, restore the terminal (leave alternate screen, disable raw mode,
  pop keyboard enhancement flags) from a panic hook *and* a normal shutdown
  path, so a panic never leaves the user with a broken terminal.

### 7.1 Suspending: handing the terminal to a child process

`ted` runs other terminal programs by giving up the tty, waiting, and taking it
back — the `git commit` / `:!` handoff. This is the primitive behind `:Explore`
(§13.4), `:!`, and any later `:Git`-style integration; those are bindings on top
of it, not separate mechanisms. It is also why M4 does not need to embed a
terminal emulator to run one program (§21).

The terminal-mode half already exists and composes: `enter_terminal_mode` and
`restore_terminal_mode` (`src/frame.rs`) are an idempotent pair guarded by
`TERMINAL_IS_RAW`, so suspend is `restore` → spawn → wait → `enter`. Four things
have to be true around that.

**The reader thread must be parked, and must acknowledge it.** This is the only
hard part. `spawn_reader_thread` blocks in `crossterm::event::read()`, and a
blocking read on stdin cannot be cancelled. If the thread is still inside
`read()` when the child starts, both processes are reading the same file
descriptor and input is split between them nondeterministically — keystrokes
vanish into `ted`'s channel while the user is driving the child. So:

- The reader loop becomes `poll(POLL_INTERVAL)?` followed by `read()` only when
  `poll` reports input.
- Suspending sets a `paused` flag and then **waits for the reader to
  acknowledge** on a condvar. The reader acknowledges only from the point where
  `poll` timed out, which is the one place it is provably not inside `read()`,
  then parks until `paused` clears.
- A flag check without the acknowledgement leaves a permanent race. The
  handshake is the mechanism; the flag alone is not.

Worst-case handoff latency is one `POLL_INTERVAL` (50ms is invisible). Events
read after the flag was set but before the acknowledgement are delivered
normally — the user typed them before the suspend took effect, so they belong to
`ted`.

**The frame loop must stop painting.** GPUI keeps running throughout — the
background executor, language servers and file watching should not stall for the
lifetime of the child — but `paint()` must not run, because a single escape
sequence written while the child owns the screen corrupts it. `Session` carries a
suspended state that skips presentation while still pumping GPUI. Notifications
raised during the suspension queue and render on resume rather than being
dropped (§13.3).

**Resume re-asserts everything rather than assuming.** The child may have pushed
its own keyboard-enhancement flags, changed the cursor shape, or used the kitty
graphics protocol. `enter_terminal_mode` re-emits `ted`'s full setup
unconditionally, and the cursor shape is re-applied from the current vim mode.
Ratatui's diff buffer is stale after the child has painted, so the resume path
clears and forces a full repaint instead of diffing against a buffer that no
longer describes the screen.

**Resize during the suspension is invisible and must be recovered.** With the
reader parked, no `Event::Resize` is delivered, so the child may have been
resized without `ted` hearing about it. Resume queries
`crossterm::terminal::size()` directly and drives the result through the normal
`resize_to_cells` path (§10.2) rather than trusting the cached grid.

**Typeahead left behind by the child is dropped, because it cannot be trusted to
be visible.** Two mechanisms conspire. A byte that arrives while the terminal is
in canonical mode — which is what "restored" means — is held by the line
discipline and reported to nobody until the line is complete. And
`crossterm::event::poll` is edge-triggered underneath (mio's epoll), so once raw
mode makes that byte readable there is no new edge to report it: it surfaces
only when the *next* keystroke arrives, one keystroke behind the user forever
after. The same edge is also lost when crossterm's event source reports a
pending SIGWINCH, which it does without draining the terminal. So resume flushes
the terminal's input queue (`tcflush`) and drains crossterm's parsed events
before the reader restarts. Nothing is lost by it: what the user typed then was
typed at the child.

**The child runs in the foreground, so terminal signals reach `ted` too.** ctrl-C
and ctrl-\ go to every process in the foreground process group, and `ted`'s
default disposition would terminate it — losing unsaved buffers because the user
interrupted a `:!make`. For as long as `ted` is out of raw mode it installs a
handler that does nothing. A handler rather than `SIG_IGN`: `exec` resets a
*handled* signal to its default in the child but preserves an *ignored* one, so
ignoring would leave ctrl-C doing nothing to the child either.

**A child that printed to the screen gets a keypress before the screen is taken
back**, which is what vim's "Press ENTER" prompt is for: `ted` is one frame away
from painting over the output. It is a property of the child, not of the
primitive — a full-screen program like a file manager (§13.4) has already had
the user's attention and resumes without a prompt.

The child inherits `ted`'s stdio and runs in the foreground. `ted` awaits it
rather than waiting on it, so GPUI keeps running — language servers, file
watching and the rest do not stall for the child's lifetime. A child that fails
to start or exits non-zero is reported through the notification line, not
swallowed.

---

## 8. Input

### 8.1 Translation

`src/input.rs` maps `crossterm::event::KeyEvent` → `gpui::Keystroke`
(`crates/gpui/src/platform/keystroke.rs:18`):

```rust
Keystroke {
    modifiers: Modifiers { control, alt, shift, platform: false, function: false },
    key: String,          // "a", "escape", "enter", "tab", "f1", "left", ...
    key_char: Option<String>, // Some("a") / Some("A") for printable input
}
```

then `window.dispatch_keystroke(keystroke, cx)`. That call first runs the
keybinding dispatch tree; if nothing consumes the keystroke and `key_char` is
set, it routes the text through the window's `PlatformInputHandler`, inserting
it into the focused editor (`crates/gpui/src/window.rs:4832`). This is the same
path `TestAppContext::simulate_keystrokes` uses, so vim's entire test-verified
behaviour is reachable.

Key names must match Zed's keymap vocabulary exactly, since `assets/keymaps/*`
and `assets/keymaps/vim.json` (1258 lines) are parsed as-is. Derive the table
from `gpui::Keystroke::parse` rather than inventing one.

### 8.2 Terminal key fidelity

This is the hardest *product* constraint, and it must be confronted early
because vim keybindings collide with terminal legacy encodings:

| Problem | Mitigation |
|---|---|
| `ctrl-i` is indistinguishable from `tab`, `ctrl-m` from `enter`, `ctrl-[` from `escape` in legacy mode | Request the **Kitty keyboard protocol** (`crossterm::event::PushKeyboardEnhancementFlags` with `DISAMBIGUATE_ESCAPE_CODES | REPORT_ALTERNATE_KEYS | REPORT_EVENT_TYPES`). Supported by kitty, foot, WezTerm, Ghostty, recent Alacritty, and iTerm2 partially. |
| `cmd-*` bindings (macOS keymap) never arrive | `ted` loads `default-linux.json` on every OS as its base keymap — its bindings are control-based — and layers `vim.json` on top. Document that `ted`'s chrome bindings differ from GUI Zed on macOS. |
| Terminals without the enhancement protocol | Detect (the terminal simply doesn't answer the query) and fall back to a documented legacy mode: `ted` ships `assets`-adjacent overrides mapping the un-encodable bindings to alternatives, and prints a one-line hint at startup naming the degraded keys. |
| `escape` ambiguity vs. escape sequence prefix | crossterm handles this with its own timeout; in enhanced mode it disappears. Do not add a second timeout layer. |
| Bracketed paste | Enable it. A paste arrives as one `Event::Paste(String)` and must be inserted as a single edit through the input handler, **not** replayed as keystrokes — replaying would run vim motions over pasted text. This is a correctness requirement, not a nicety. |

### 8.3 Counting and pending keys

`window.pending_input_keystrokes()` already exposes multi-keystroke bindings in
flight (this is what `vim::ModeIndicator` renders,
`crates/vim/src/mode_indicator.rs:56`). Project it into the status line so
`d2` shows as pending exactly as in Zed.

---

## 9. Backend bootstrap

`src/bootstrap.rs`. A reduced version of `crates/zed/src/main.rs:466-790`. The
init *order* there is load-bearing and must be preserved for the subset we
take. Required, in order:

```
zlog::init / release_channel::init / gpui_tokio::init
settings::init(cx)                            # SettingsStore global
theme_settings::init(LoadThemes::All(Assets)) # theme + font settings
Assets.load_fonts(cx)
menu::init / zed_actions::init
client::init(&client, cx)
project::Project::init(&client, cx)
languages::init(languages, fs, node_runtime, cx)
editor::init(cx)
workspace::init(app_state, cx)
search::init(cx)                              # required for `/` (§14.2)
command_palette_hooks::init(cx)               # required for `:` (§13.2)
command_palette::init(cx)                     # registers the interceptor host
go_to_line::init / file_finder::init           # M2 overlays
vim::init(cx)
```

Deliberately **excluded** for MVP: `agent_ui`, `collab_ui`, `git_ui`,
`terminal_view`, `debugger_ui`, `repl`, `extensions_ui`, `settings_ui`,
`keymap_editor`, `onboarding`, `auto_update*`, `inspector_ui`,
`component_preview`, all `language_models` / `edit_prediction` wiring. Each is
a candidate for a later milestone; none is needed to edit text. Excluding them
also keeps startup fast, which matters for a terminal tool.

`workspace::AppState` (`crates/workspace/src/workspace.rs:1122`) is small:
`languages`, `client`, `user_store`, `workspace_store`, `fs`,
`build_window_options`, `node_runtime`, `session`. Construct it with the real
implementations (real `Fs`, real `Client` — collaboration depends on it), not
the `AppState::test` fakes. `build_window_options` returns the terminal window
options from §6.1.

**Settings.** `ted` reads the user's normal `settings.json` so language
config, tab size, formatters and LSP settings are shared with GUI Zed, then
applies a non-persisted override layer:

- `buffer_font_size` / `buffer_line_height` pinned to the values that make §5's
  identities hold.
- `soft_wrap` defaults to `editor_width` (a terminal has no horizontal scroll
  affordance worth speaking of), overridable.
- Chrome hidden: tab bar, status bar, title bar, breadcrumbs, docks,
  scrollbars, minimap, indent guides off by default. This is *cosmetic* — see
  §10.2; `ted` renders whatever cell rect the editor reports, so it is correct
  either way. Hiding chrome just means the editor gets the whole grid.
- `vim_mode: true` unless `--no-vim`.

**`ted`'s own settings.** The settings that exist only because `ted` is a
terminal host — `file_manager` (§13.4) so far — live in **`ted.json`**, beside
`settings.json` and `keymap.json` in the same config directory, read by `ted`
alone. The shared file is shared on purpose: language config, tab size,
formatters and LSP settings should mean the same thing in both. A key only one
of the two can act on is a different thing, and putting it in `settings.json`
would mean adding it to Zed's settings schema, offering it for completion in a
GUI that cannot honour it, and making a single run of `ted` a permanent addition
to a Zed user's configuration. Switching between GUI and TUI configures neither.

`ted.json` is JSON with comments and trailing commas, like every other file Zed
asks a user to write by hand. Absent means the defaults; unusable — malformed,
or carrying a key `ted` does not know — falls back to the defaults *and reports
on the notification line* (§13.3), because the alternative is a config that
silently does nothing until the day it matters.

**Keymap.** Load `assets/keymaps/default-linux.json` then, when vim is enabled,
`assets/keymaps/vim.json`, each stamped with the right `KeybindSource` — the
exact sequence in `crates/vim/src/test/vim_test_context.rs:100-119`. Then layer
the user's `keymap.json`, then `ted`'s terminal-specific overrides.

**Opening.** `Workspace::new_local(paths, app_state, None, env, Some(init),
open_mode, cx)` (`crates/workspace/src/workspace.rs:1898`) builds the
`Project`, opens the window, and returns the `Workspace`. The `init` callback is
where `ted` installs per-pane toolbar items (§14.2) and grabs the handles it
needs. Reusing this path rather than hand-rolling window creation means
workspace serialisation, worktree trust and project restoration all behave.

The window's root view is `MultiWorkspace`
(`crates/workspace/src/multi_workspace.rs:287`), not `Workspace`. Every snapshot
read walks root → `active_workspace` → active pane → `active_item().act_as::<Editor>(cx)`,
and a missing editor at any step renders an empty buffer view rather than
failing.

`NodeRuntime` is the real one, not `NodeRuntime::unavailable()`; several language
servers cannot install without it, and "LSP silently does nothing" is a worse
failure than a slow first start.

**Focus.** `ted` focuses the active item's editor explicitly after the first
frame. Combined with `TerminalWindow` reporting `is_active() == true` (§6.1),
this is what makes the editor render as focused, keeps cursor state live, and
routes keystrokes down the intended dispatch path.

---

## 10. The `ViewSnapshot` projection

The contract between backend state and Ratatui. One struct, rebuilt each frame
from GPUI-side reads, containing **no GPUI handles** — only plain data. That
constraint is what keeps §4.3's out-of-process option open, and it also makes
the renderer trivially unit-testable.

### 10.1 Shape

```rust
pub struct ViewSnapshot {
    pub grid: Size<u16>,                  // terminal size in cells
    pub editor: Option<EditorView>,
    pub status: StatusView,
    pub command_line: Option<CommandLineView>,
    pub overlay: Option<OverlayView>,     // §24.1
    pub notifications: Vec<NotificationView>,
    pub cursor: Option<CursorView>,       // terminal hardware cursor
}

pub struct EditorView {
    pub text_rect: Rect,                  // cells
    pub gutter_rect: Rect,                // cells; width 0 if hidden
    pub scroll: Point<f64>,               // display rows / columns
    pub rows: Vec<RowView>,               // one per visible display row
    pub selections: Vec<SelectionSpan>,   // local, in cell coordinates
    pub remote_selections: Vec<RemoteSelectionSpan>, // §18
    pub max_display_row: u32,
    pub soft_wrapped: bool,
}

pub struct RowView {
    pub display_row: u32,
    pub kind: RowKind,                    // Text | Block | BufferHeader | ExcerptHeader
    pub gutter: GutterView,               // line number, diff marker, fold marker
    pub spans: Vec<StyledSpan>,           // text + resolved style, cell-aligned
    pub byte_to_cell: Vec<u16>,           // §5: byte column -> cell column
    pub soft_wrap_indent: u16,
}
```

`StyledSpan` carries a resolved `TextStyle` (fg/bg `Hsla`, bold, italic,
underline, strikethrough) — not a GPUI `HighlightStyle` — so §12's colour
mapping is the only place theme types appear.

### 10.2 Size negotiation

Both directions:

- **Frontend → backend.** On startup and on every `Event::Resize(cols, rows)`,
  `ted` calls `TerminalWindow::resize_to_cells(cols, rows - reserved)`, where
  `reserved` is the rows `ted` paints itself: the status line, plus the `:` / `/`
  line and any notification line when present. The GPUI window is therefore
  **smaller than the terminal**, and the editor's rect can never overlap a row
  `ted` owns. Sizing the window to the full grid instead would put the status
  line inside the editor's reported rect and the two would fight over it.
  Changing `reserved` (a notification appearing) is a resize like any other.
- **Backend → frontend.** After the frame, `ted` reads
  `editor.last_bounds()` (`crates/editor/src/editor.rs:2768`, already public)
  and the gutter dimensions, and floors them into cells (§5.3). The renderer
  paints the editor there and nowhere else.

"The editor occupies rows 0..22, columns 5..80" is recovered from Zed's own
layout rather than assumed. Re-enabling the tab bar in a later milestone
surfaces its rect through the same path.

**Ordering.** The snapshot must be built *after* the draw: `Editor::style` and
`last_bounds` are populated during element layout, so a snapshot taken before
the first frame has neither. `ted` draws, then reads, then paints.

**Minimum size.** The editor's usable width is
`text_width - gutter_width - margin - em_width` (`element.rs:10695`). On a very
small grid that arithmetic goes negative, and a negative wrap width is not a
case Zed is expected to handle. `ted` enforces a floor — 20×5 including reserved
rows — and below it paints a "terminal too small" message without resizing the
GPUI window.

**Rewrap is asynchronous.** `DisplayMap::is_rewrapping(cx)`
(`crates/editor/src/display_map.rs:1373`) reports a wrap pass still running,
which happens on large files after a resize. `ted` paints the rows it has,
carries the flag in the snapshot so the status line can show it, and repaints
when the pass completes.

### 10.3 Sources for each field

| Field | Source |
|---|---|
| `rows[].spans` | `EditorSnapshot::display_snapshot.highlighted_chunks(rows, LanguageAwareStyling::Enabled, &editor_style)` (`crates/editor/src/display_map.rs:1863`) |
| `rows[].gutter` | `DisplaySnapshot::row_infos(start_row)` → `RowInfo { buffer_row, diff_status, expand_info, .. }` (`crates/multi_buffer/src/multi_buffer.rs:813`) |
| `rows[].kind` | `is_block_line`, `is_folded_buffer_header`, `blocks_in_range` (`display_map.rs:2183-2213`) |
| `soft_wrap_indent` | `DisplaySnapshot::soft_wrap_indent(row)` (`:2218`) |
| `selections` | `editor.selections.disjoint_anchors()` → `SelectionExt::display_range(&map)` (`editor.rs:11913`) |
| `remote_selections` | `EditorSnapshot::remote_selections_in_range(..)` (`editor.rs:11530`) |
| `scroll` | `Editor::scroll_position(cx)` (`crates/editor/src/scroll.rs:835`) |
| visible rows | `Editor::visible_line_count()` / `visible_row_count()` (`scroll.rs:699`) |
| `max_display_row` | `DisplaySnapshot::max_point()` (`display_map.rs:1801`) |
| vim mode | `editor.addon::<VimAddon>()` → `vim.entity.read(cx).mode` (`crates/vim/src/state.rs:44`, `Display` gives "NORMAL"/"VISUAL LINE"/…) |
| pending keys | `window.pending_input_keystrokes()` |
| dirty / path / language | `Item` impls on the pane's active item; `buffer.read(cx).is_dirty()` |
| diagnostics summary | `project.read(cx).diagnostic_summary(..)` |

`EditorStyle` is needed for `highlighted_chunks`. `Editor::create_style`
(`editor.rs:10902`) is private and the `style` field (`editor.rs:1066`) is
populated during element layout — so add a public accessor (§20.1) rather than
duplicating theme logic in `ted`, which would silently drift.

---

## 11. Rendering the buffer

A custom Ratatui `Widget`, not `Paragraph` — the styling is per-cell and the
coordinate model is Zed's, not Ratatui's line-wrapping model.

Per visible row:

1. Emit gutter: line number (right-aligned in `gutter_rect`), diff marker
   (`+`/`~`/`-` or a background tint from `RowInfo::diff_status`), fold chevron
   where `crease_for_buffer_row` reports one.
2. Walk `spans`, placing graphemes into cells using `byte_to_cell`. A
   double-width grapheme occupies its cell and leaves the next one blank
   (Ratatui expects exactly this).
3. Horizontal scroll: skip `scroll.x` cells from the left; clip at
   `text_rect.width`. Vertical: rows are already the window into the display
   map.
4. Overlay selections. For each `SelectionSpan` intersecting the row, apply the
   selection background. Visual-block mode yields a rectangular span set —
   `SelectionExt::display_range` plus the block's column range handles it.
5. Overlay the current-line highlight per `EditorSnapshot::current_line_highlight`.
6. Blocks (diagnostics, git blame, excerpt headers) render as their own rows
   with a distinct style; M1 renders a placeholder line, M3 renders their text.
7. Folds render as `⋯` (or `...` in ASCII mode) with the fold's background.

The cursor is *not* drawn as a cell; §7 places the terminal's real cursor.
Additional cursors (multi-cursor) do get drawn as inverted cells, since a
terminal has only one hardware cursor.

**Invisibles, tabs, wide chars.** `DisplayMap`'s `TabMap` has already expanded
tabs to spaces, so no tab handling is needed at render time — a common source
of column bugs in hand-rolled TUI editors, avoided for free.

---

## 12. Colour

`src/palette.rs`. Zed themes are `Hsla`. Terminals offer 24-bit, 256-colour, or
16-colour.

- Detect capability: `COLORTERM=truecolor|24bit` → truecolor; else `TERM`
  containing `256color` → 256; else 16.
- Truecolor: convert `Hsla` → `Rgba` → `ratatui::style::Color::Rgb`. Exact.
- 256: quantise to the 6×6×6 cube plus greyscale ramp, choosing by nearest
  distance in Oklab rather than sRGB so syntax colours stay distinguishable.
- 16: map to the terminal's ANSI palette by hue bucket + lightness. Accept that
  this is lossy; keep a hand-tuned table for the syntax categories that matter
  (keyword, string, comment, function, type, number) rather than a generic
  nearest-neighbour, which tends to collapse comments into the background.
- Background: default to the terminal's own background (emit no bg for the
  editor surface) unless `--opaque-background`, so `ted` composes with
  transparent terminals and doesn't fight the user's colour scheme at the
  edges.
- Cache every conversion; `Hsla → Color` is called per span per frame.

---

## 13. Overlays, modals, and the command line

### 13.1 The projection problem

Zed's palettes are `Picker<D>` views. Their row content is produced by
`PickerDelegate::render_match`, which returns GPUI elements — unusable as text.
Two honest options:

- **Mirror**: add a small, defaulted trait method to `PickerDelegate` that
  yields plain text for a match, implement it for the delegates we care about,
  and have `ted` read `match_count` / `selected_index` / that method. Zero
  duplicated logic; every filter, ordering and hook stays shared. Costs an
  additive upstream change per delegate we support.
- **Own**: `ted` implements its own palettes against the same data sources
  (`cx.all_action_names()` + `CommandPaletteFilter`, `project` file scan +
  `fuzzy`). No upstream change; duplicated behaviour that will drift.

**Recommendation:** Own for M2 (fast, unblocks `:` and file open), Mirror from
M3 onward as the general mechanism, because it is the only approach that scales
to the dozens of pickers Zed has. Own is also, for now, the only option that
needs no upstream change first: `FileFinderDelegate` is a public struct whose
constructor and every field are private (`crates/file_finder/src/file_finder.rs:368-390`),
so there is nothing to read even before the rendering question arises (§24.4). `ted` keeps a registry mapping modal `TypeId`
→ projection adapter, with a generic fallback that renders
`[modal: <type name> — not yet supported in ted]` so an unmirrored palette
degrades visibly instead of invisibly swallowing keys.

### 13.2 The `:` command line

This is the piece that makes vim's `:` work with almost no code, and it is
worth calling out. `ted` draws its own `:` line at the bottom of the grid, and
resolves the typed query through Zed's existing interceptor:

```rust
GlobalCommandPaletteInterceptor::intercept(&query, workspace.downgrade(), cx)
// -> Task<CommandInterceptResult { results: Vec<CommandInterceptItem>, exclusive }>
// CommandInterceptItem { action: Box<dyn Action>, string: String, positions: Vec<usize> }
```

(`crates/command_palette_hooks/src/command_palette_hooks.rs:95-153`.) `vim`
registers the interceptor that parses `:w`, `:wq`, `:q!`, `:42`,
`:%s/a/b/g`, `:vsplit`, ranges, and the rest of its command table
(`crates/vim/src/command.rs`). `ted` renders the returned strings as
completions and dispatches the chosen `action` via
`window.dispatch_action(action, cx)`. If the interceptor returns nothing,
fall back to matching action names — the same policy as Zed's palette.

So the command line is a **rendering** concern in `ted` and a **semantics**
concern in `vim`, with no duplicated parsing.

### 13.3 Prompts and notifications

`TerminalWindow::prompt()` answers the question itself rather than returning
`None`: it queues the message and its answers and hands back the receiver GPUI
awaits. `ted` paints the queue's head in two of its reserved rows — the question
above its answers, numbered from 1 — and a keystroke sends the index. `enter`
takes the first answer, GPUI's default; `esc` takes the last, which on every
prompt reachable here is `Cancel`.

Returning `None` instead is what makes GPUI render *its* prompt view, an element
tree inside the window. `ted` projects only the editor's rect (§10.2), so such a
prompt is never painted — but it still holds focus, which is indistinguishable
from a hang: the editor stops answering keys and whatever asked the question
waits forever. `:q` on a modified buffer is the shortest path to it
(`workspace::CloseActiveItem` with `SaveIntent::Close`), which makes this the
one platform method that cannot be stubbed out.

Workspace notifications (save errors, LSP failures) render as a transient line
above the status line — important, because a TUI that silently drops an "unable
to save" error is worse than useless.

### 13.4 Host commands, and `:Explore`

A few `:` commands mean something only because `ted` owns a tty. They cannot go
through §13.2's interceptor, because the interceptor resolves to a
`Box<dyn Action>` and these do not dispatch an action — they suspend the process
(§7.1). `ted` therefore keeps a small **host-command table**, checked before the
interceptor, in its own namespace.

This is the one place `ted` parses a `:` command itself, and the boundary is
worth stating precisely: the host table holds commands that exist *because of the
terminal host*, not a second copy of vim's command set. Adding `:Explore` to
`crates/vim/src/command.rs` would be wrong — GUI Zed has no tty to hand over.
Anything expressible as an action stays with the interceptor.

Mechanically, `command_line::Completion` carries an effect rather than an action
— either the `Box<dyn Action>` it carries today or a host command — and `enter`
dispatches or suspends accordingly.

**`:!`** is the primitive's own binding, and the boundary with vim's `:!` is
finer than it looks. `ShellExec` (`crates/vim/src/command.rs`) parses every form
of it, but resolves the *bare* one to a `SpawnInTerminal` aimed at a terminal
panel `ted` does not have — dead without a host that owns a tty. Its other forms
— `:%!sort`, `:.,.+3!fmt`, `:r!date` — filter buffer text through the command
and are real editor edits, so they stay with the interceptor. The host table
therefore claims exactly the queries starting with `!`; a range has been seeded
into the query as a prefix by then (§13.2), which is what makes the two
distinguishable without parsing either. The command runs through the user's
shell, so pipes, redirection and quoting mean what they mean at a prompt.

**`:Explore`** suspends and runs a file manager, defaulting to
[Yazi](https://yazi-rs.github.io). Yazi exits writing its selection to
`--chooser-file`, one path per line, so the round trip is:

```
:Explore  ->  suspend (§7.1)
          ->  yazi --chooser-file=<tmp> <start-dir>
          ->  resume, read the file, workspace.open_paths(paths, ..)
```

Multiple selected paths open as multiple buffers in one call. The start directory
is the active buffer's parent, falling back to the first worktree root.

The command is `ted.json`'s `file_manager` (§9) rather than a hard-coded binary —
an argument vector with `{chooser}` and `{directory}` substituted into it, so
`broot`, `nnn` or `ranger` work by configuration, and `ted` depends on none of
the child's output beyond the chooser file. Presence on `PATH` is checked with
the `which` crate before anything is handed over, so a missing binary surfaces
on the notification line (§13.3) with the screen still `ted`'s, rather than as a
spawn failure that flashes past between two full repaints.

**Why an external file manager instead of a project panel.** Yazi is better at
file *management* — bulk rename, move, delete, previews — than anything `ted`
would build, and the integration is nearly free because `ted` hosts a real
`Project` with real worktree fs-watching: mutations Yazi makes on disk propagate
back into open buffers with no coordination code on either side. This is why §21
carries no project-panel milestone.

**What stays native.** The fuzzy file finder (M2). Type-to-filter over project
files with Zed's ordering and history is a different interaction from browsing a
tree, and `file_finder` already provides it. Both coexist; neither replaces the
other.

**Scope.** M2 targets local worktrees, and `:Explore` declines on anything else
rather than browsing this machine's filesystem on a project's behalf when the
project's files are somewhere else. Yazi has since grown a VFS layer with a
built-in `sftp` scheme, which makes the same integration work against an
SSH-remote project — the chooser returns `sftp://host//path` and `ted` maps it
back onto the project's connection. That is deferred to M5, where the rest of the
remote and collaboration story lives (§18). A collab-*joined* project is the one
case that cannot work at all: the files exist only over Zed's collab protocol,
with no filesystem locally and no SSH access to the host by design, so `:Explore`
declines there and the finder handles it.

---

## 14. Vim specifics

### 14.1 Enabling

`vim_mode: true` in the settings override layer plus `vim.json` bound with
`KeybindSource::Vim`. `vim::init(cx)` then attaches a `VimAddon` to each
`Editor` it observes. Nothing else is required — the mode machine, registers,
marks, macros, text objects, `.` repeat and change list all come along.

### 14.2 What vim needs from the workspace

Two non-obvious dependencies, both discovered in
`crates/vim/src/test/vim_test_context.rs:127-141` and both easy to miss:

- **`/` and `?` require a `BufferSearchBar` in the pane toolbar.** Vim's search
  dispatches into it (`crates/vim/src/normal/search.rs:233`,
  `crates/vim/src/vim.rs:1544`, `crates/vim/src/command.rs:2185`). `ted` must
  add `BufferSearchBar::new(None, window, cx)` (and `ProjectSearchBar` for
  `:grep`-alikes) to every pane's toolbar in the workspace `init` callback,
  then project the search bar's query and match count into `ted`'s own `/`
  line. Without this, `/` silently does nothing.
- **`ModeIndicator` is a status-bar item.** `ted` does not need the widget —
  it reads `Vim::mode` directly — but registering it is harmless and keeps
  parity if the real status bar is ever rendered.

### 14.3 Vim commands that assume a GUI

`:vsplit`/`:split` map to `workspace::SplitRight` etc. and genuinely work:
`Workspace` maintains the pane tree regardless of who paints it. M1 renders only
the active pane, which means a split would create state the user cannot see. So
M1 also shows the pane count and index in the status line whenever more than one
pane exists — invisible state the user can still navigate into with `ctrl-w` is
the failure mode to avoid. M4 renders the pane tree as a cell-space split,
reading each pane's reported bounds exactly as §10.2 reads the editor's. Pane resizing
(`ResizePaneRight`, `crates/vim/src/vim.rs:390`) operates in pixels — with §5
those are cells, so `<C-w>>` widens by a column. Commands that open GUI-only
surfaces (`:Zed settings` etc.) will dispatch actions whose handlers we never
registered; the generic modal fallback (§13.1) makes that visible.

---

## 15. Scrolling and viewport

No custom scrolling logic. Because `visible_line_count`, wrap width and window
bounds are all in cells (§5), Zed's own scroll manager and autoscroll are
correct as-is:

- `ctrl-d`, `ctrl-u`, `zz`, `zt`, `zb`, `H`, `M`, `L` work because
  `visible_line_count` is exactly the terminal's row count.
- `scroll_beyond_last_line`, `vertical_scroll_margin` (vim's `scrolloff`) work
  because they're expressed in lines.
- Autoscroll on cursor move is resolved during layout, so it has already
  happened by the time `ted` reads `scroll_position()` for the frame.
- Terminal mouse wheel maps to `PlatformInput::ScrollWheel` with a delta of
  `n * CELL_H`, i.e. n lines.

The one thing `ted` must not do is maintain its own scroll offset. Any
divergence between `ted`'s idea of the top row and the editor's breaks
autoscroll and `H`/`L`.

---

## 16. Clipboard

`ClipboardItem` in, `ClipboardItem` out, through
`TerminalPlatform::{read,write}_from_clipboard`. Three tiers:

1. **OS clipboard** via `arboard`, or by delegating to `inner` where that
   works. Preferred when `ted` runs locally. Delegation is not reliable over
   SSH: `MacPlatform`'s implementation goes through `NSPasteboard`, which a
   process outside a window-server session cannot reach, so a failed read or
   write must fall through to the next tier rather than propagate.
2. **OSC 52** for writes when running inside a remote session (`SSH_TTY` set)
   and the terminal advertises support. Covers the common "`ted` over ssh, yank
   into my local clipboard" case, which is the whole point of a TUI editor for
   many users.
3. **Internal fallback** — an in-process clipboard so yank/put always works
   even with no OS clipboard and no OSC 52. OSC 52 *reads* are a security
   hazard and widely disabled; do not rely on them.

Vim registers are internal to `vim` and unaffected; only `"+` / `"*` and
`editor::Copy`/`Paste` touch the platform clipboard.

---

## 17. Mouse (stretch)

Enabling mouse reporting gives click, drag and wheel. Translation is
mechanical: cell `(col, row)` → `Point<Pixels>` by multiplying by `CELL`, then
`PlatformInput::{MouseDown, MouseMove, MouseUp, ScrollWheel}` into
`window.dispatch_event`. Because the element tree really was laid out at that
geometry, hit testing, click-to-place-cursor, drag-select and
double-click-to-select-word all work with no editor-side code. This is a
strong argument for the layout-mirroring model in §4.2 and a good early
validation that the cell contract holds.

Caveat: mouse reporting steals the terminal's own selection. Gate it behind a
setting and a toggle binding.

---

## 18. Collaboration readiness

Nothing in this design blocks it, and that is deliberate:

- `ted` constructs a **real** `client::Client`, `UserStore` and
  `WorkspaceStore` (§9), so sign-in, `channel`, and `call::ActiveCall` are
  available without restructuring.
- Buffers are `MultiBuffer`s over real `language::Buffer`s owned by a real
  `Project`. A remote project (`Project::remote`) substitutes cleanly; the
  editor and `ted` never learn the difference.
- Remote selections are already in the snapshot:
  `EditorSnapshot::remote_selections_in_range` (`editor.rs:11530`) yields
  `RemoteSelection { selection, participant_index, peer_id, user_name, color }`.
  Render each collaborator's cursor as a coloured cell and their selection as a
  tinted range; render the name in the status line or as a one-cell-tall inline
  label. This is a rendering task of a few dozen lines, not an architecture
  change.
- Following (`workspace::WorkspaceStore::handle_follow`) drives scroll and
  selection changes into the editor; `ted` mirrors whatever results.

The prerequisites to make it real later: sign-in flow usable in a terminal
(device-code style, since we can't open a browser reliably), and rendering
collaborator presence somewhere. Both are additive.

---

## 19. Performance budget

| Stage | Budget (80×24) | Notes |
|---|---|---|
| GPUI layout + paint of the Zed tree (§4.2.1 — the two run as one traversal, not two) | < 2ms | Only on dirty frames; GPUI's view cache skips clean subtrees. |
| `ViewSnapshot` construction | < 1ms | Dominated by `highlighted_chunks` over ≤ rows lines. |
| Ratatui render + diff | < 1ms | |
| Terminal write + flush | < 3ms | Usually the largest term; Ratatui's diffing keeps the byte count small. |
| **Idle CPU** | ~0% | Achieved by pulling frames only when dirty (§7); an unconditional 60Hz layout loop would burn several percent and is not acceptable for a terminal tool. |

Startup is the other budget that matters: a terminal editor that takes 400ms to
appear feels broken. Deferring language-server startup and worktree scanning
until after the first frame is painted is a hard requirement, not an
optimisation.

---

## 20. Changes to existing crates

Kept deliberately small, additive, and defaulted.

### 20.1 Required

| Crate | Change |
|---|---|
| `editor` | `pub fn style(&self) -> Option<&EditorStyle>` (field exists at `editor.rs:1066`). Needed for `highlighted_chunks`. Alternative — make `create_style` public — is also acceptable; the accessor is less surface. |
| `editor` | Make `PositionMap` (`element.rs:10100`, currently `pub(crate)`) readable, **or** confirm `ted` can compute everything from `DisplaySnapshot`. Prefer the latter; only escalate if a real gap appears. |
| workspace root `Cargo.toml` | add `crates/ted` to `members`; add `ratatui`, `crossterm`, `unicode-width`, `unicode-segmentation`, `arboard` to `[workspace.dependencies]`. |
| workspace root `Cargo.toml` | add `underline-color` to `ratatui`'s feature list. It is in Ratatui's defaults, and the workspace takes `default-features = false`, so severity-coloured underlines (§24.8) are otherwise unreachable. |

### 20.2 Likely, as milestones land

| Crate | Change |
|---|---|
| `picker` | Defaulted `PickerDelegate::text_for_match(&self, ix) -> Option<PickerRowText>`, for §13.1's Mirror strategy. The trait already has the precedent — `render_match_with_checkbox` defaults to `None` (`crates/picker/src/picker.rs:377-386`) — but note that `set_selected_index`, `update_matches`, `confirm` and `dismissed` all take `&mut Window`, so Mirror drives a picker through a window rather than beside one. |
| `editor` | A public reader for the completions menu — `context_menu` is private (`editor.rs:1006`) and `context_menu_visible()` / `context_menu_origin()` are the only public readers, so nothing outside the crate can see an entry, a label or the selection. Prefer a method returning plain data over `&CompletionsMenu`, which keeps §10's no-handles rule intact. One reader also unblocks signature help and code actions (§24.9). |
| `vim` | `pub fn status_label(editor, cx)`, in the shape of the `vim::mode` and `vim::take_command_line_prefix` helpers that already exist for embedders (`crates/vim/src/vim.rs:507`, `:517`). `Vim::status_label` is a public field (`:553`) but `VimAddon` is `pub(crate)`, so `ctrl-g` (`vim::ShowLocation`) shows nothing in `ted` (§24.5). |
| `gpui` | **Nothing required.** `Window::draw`, `dispatch_keystroke`, `dispatch_event` and `pending_input_keystrokes` are public; `Platform`, `PlatformWindow` and `PlatformTextSystem` are public traits. |
| `gpui_linux` | Nothing required — but `headless/window.rs` is the reference implementation for `TerminalWindow`. If duplication becomes annoying, promote the atlas stub to `gpui` as a shared `NullAtlas`. |

### 20.3 Testing

- Unit: `CellTextSystem` metrics vs. `unicode-width` over a corpus including
  CJK, emoji with ZWJ, combining marks, and RTL. Assert `layout_line` width
  equals the renderer's computed cell width for every string.
- Unit: `ViewSnapshot` → expected cell grid, as plain data with no terminal.
- Integration: drive `ted`'s backend with `gpui::HeadlessAppContext` +
  `CellTextSystem` and assert rendered grids after keystroke sequences — a
  golden-file harness in the spirit of `crates/vim/src/test`. This is where the
  editor/vim parity claim gets defended; borrow `NeovimBackedTestContext`'s
  approach where it applies.
- End-to-end smoke: run `ted` against a pty (`portable-pty`) in CI, send
  keystrokes, assert the emitted grid. Catches terminal-mode and escape-
  sequence regressions that no in-process test can.
- Suspend (§7.1), in the same pty harness, with a scripted child standing in for
  the file manager so the test has no external dependency: assert that input sent
  while the child runs reaches the child and not `ted`, that the grid is fully
  repainted on resume, that a resize performed during the suspension is picked
  up, and that terminal modes match their pre-suspend state afterwards. The
  input-stealing race is invisible to any in-process test.

---

## 21. Milestones

**M0 — Spike (proves or kills the design).** `TerminalPlatform` +
`CellTextSystem` + one headless window, **on Linux and macOS**. Open a
hard-coded file into a real `Editor` with no `Workspace`. Render display rows as
unstyled text. Dispatch `h/j/k/l` and printable keys. *Acceptance, on both
platforms:* (a) the cell contract holds — a window of `cols*CELL_W ×
rows*CELL_H` yields `visible_line_count == rows` and soft wrap at exactly `cols`
columns, verified by assertion, not by eye; (b) `em_width == em_advance == CELL_W`
across every font size the app can reach; (c) a keystroke arriving on the reader
thread wakes the run loop and produces a frame with no timer involved; (d) the
editor's reported rect is measured against the window with chrome hidden, which
settles how much inset §22 has to absorb.

**M1 — MVP, "nano parity", Linux + macOS.** Full bootstrap with `Workspace`,
`Project`, settings, keymaps, vim (vim on by default; `--no-vim` for plain Zed
bindings). All Zed chrome hidden — the editor owns the whole grid and `ted`
draws its own status line. Syntax highlighting, gutter with line numbers,
selections, cursor placement via the terminal cursor, status line with vim mode
and dirty indicator, `:w` / `:q` / `:wq` through the interceptor, `/` search
via `BufferSearchBar`, undo/redo, clipboard. Resize. Clean terminal restore on
panic. *Acceptance:* a user can edit and save a file end-to-end using only vim
motions on both platforms; every editor-crate feature listed here is exercised
by an integration test; the pty smoke test (§20.3) runs in CI for both.

macOS-specific M1 work, none of it deep but none of it free: pasteboard
clipboard via the delegated `MacPlatform` implementation (§16), key translation
for a keyboard where `alt` is `option` and the Kitty protocol support varies by
terminal (Terminal.app has none; iTerm2, WezTerm and Ghostty do), and
confirming `MacDispatcher`'s foreground wake path under §7's bridge.

**M2 — Navigation.** Own-implementation `:` completions, file finder, go-to-
line, buffer switching, multiple items in one pane, diagnostics rendered
inline — **each specified in §24**, which is where the design direction for them
lives. Language-server completions moved out with the status line (§24.9,
§21/M3.5), and with them the one upstream change M2 would have needed. The
suspend primitive (§7.1) with its reader-thread handshake, plus `:!` and
`:Explore` over a local worktree (§13.4).
*Acceptance:* §24.10, plus the suspend half asserted by the pty harness
(§20.3) — no input stolen, no stale grid, no altered terminal mode.

**M3 — Fidelity.** Mirror-strategy modal projection with the `PickerDelegate`
hook. Blocks, folds, inlay hints, git diff gutter, multi-cursor, visual block.
Mouse. 256/16-colour fallbacks. *Acceptance:* the modal registry covers command
palette, file finder, outline, project symbols, and the generic fallback is
never hit in normal use.

**M3.5 — The status line, and completions.** Two things M2 deliberately set
aside, grouped because each is a design of its own rather than a feature to
finish. The status line gets one: what it carries (mode, path, position,
diagnostics counts, vim's location string from `ctrl-g`, pane and item
indicators), how it behaves when the grid is narrow, and what belongs on it at
all rather than on a surface of its own — every §24 question that ended "waits
for the status line" lands here. Alongside it, the language-server completions
popup (§24.9), signature help and the code-action menu, which arrive together
behind one new `editor` reader (§20.2). This milestone also settles what
"quit" means once the last item in a pane closes (§24.7): exiting on an empty
pane is M2's placeholder, not the intended behaviour.

**M4 — Layout.** Pane splits rendered as cell-space splits driven by reported
bounds. No project panel: §13.4's external file manager covers browsing and file
management, and a sidebar would duplicate it. Terminal panel — note the
recursion, and that `terminal_view` is itself a GPUI view over `alacritty_terminal`;
projecting a terminal inside a terminal is best done by reading the
`terminal::Terminal` grid directly, not through `ViewSnapshot`. Note that §7.1
already covers running *one* program without embedding an emulator, so the panel
is only needed for a persistent terminal alongside the editor.

**M5 — Collaboration + optional process split.** Terminal-friendly sign-in,
collaborator cursors and presence, following. `:Explore` against an SSH-remote
project, mapping Yazi's `sftp://host//path` selections back onto the project's
connection (§13.4). Optionally move the frontend out of process across the
`ViewSnapshot` boundary, which also enables "attach a TUI to a running Zed."

---

## 22. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Terminals without the Kitty keyboard protocol can't express vim's full binding set | High — it is a product limitation, not a bug | Detect, degrade explicitly, document, ship an override keymap. Do not pretend the keys arrive. |
| `Workspace`'s element tree assumes GUI-scale pixels; at cell scale some chrome may collapse or assert | Medium | Chrome is hidden by default (§9); `ted` reads reported rects rather than assuming a layout. Any panic in a chrome element is a bug to fix upstream, and cheap to find. |
| Layout cost per frame if GPUI's view cache misses broadly | Medium | Measure in M0. Fall back to a minimal root view that renders only `EditorElement` + a `Workspace` kept in the dispatch tree but not in the visual tree, if the full tree proves too costly. |
| `unicode-width` disagrees with the user's terminal (emoji, ambiguous-width CJK) | Medium | Both sides use one crate and one version, so `ted` is self-consistent; residual disagreement is with the *terminal*. Offer a setting for ambiguous-width, matching what terminals themselves expose. |
| Zed's `Platform` trait churns, breaking the delegating shim | Low–Medium | 65 methods, mostly one-line delegations; breakage is a compile error, not a silent bug. Accepted cost. |
| Grapheme/byte column confusion | High if unaddressed | Exactly one conversion table per row, built once in `ViewSnapshot` (§10.1 `byte_to_cell`) and used by every consumer. No ad-hoc conversions anywhere in `render/`. |
| `em_width` and `em_advance` resolve through different trait methods and could disagree | High, and silent | §5.1. One shared implementation behind both, plus a unit test asserting equality across sizes. A disagreement shows up as wrap width and column count differing by a fraction of a cell, which is very hard to diagnose from the symptom. |
| Runtime buffer-font-size changes invalidate `CELL` under a window sized in old cells | Medium | §5.5: filter the actions, derive `CELL` per frame, assert against the pinned values. |
| Fold and inlay placeholder widths are measured from elements, not from `CellTextSystem` | Medium | §5.4: render them as text and round supplied widths up to whole cells. A fractional fold width shifts every wrap boundary on its line. |
| Very small terminals drive editor width arithmetic negative | Medium | §10.2: enforce a 20×5 floor and paint a message instead of resizing the window. |
| Reserved rows put `ted`'s status line inside the editor's reported rect | Medium | §10.2: the GPUI window is sized to the grid *minus* reserved rows, so the rects cannot overlap by construction. |
| Splits create state M1 cannot render | Low | §14.3: surface pane count in the status line so it is visible even when unrendered. |
| Chrome hidden by settings still leaves the editor inset by a row or column | Low | `ted` paints at the reported rect and fills the remainder with the editor background, so the result is correct if slightly wasteful. M0 measures the actual inset and settles whether settings can reach full-bleed. |
| The reader thread and a suspended-to child both read stdin | High, and nondeterministic | §7.1: `poll`-based reader loop plus an explicit acknowledgement handshake before the child is spawned. A `paused` flag *without* the acknowledgement leaves a permanent race whose symptom is occasional swallowed keystrokes in the child — very hard to attribute. |
| A child process leaves terminal modes, cursor shape or the screen altered | Medium | §7.1: resume re-emits `ted`'s full mode setup unconditionally rather than assuming the flag stack returned as it was left, forces a full repaint instead of diffing against a stale Ratatui buffer, and re-queries the grid size because no `Event::Resize` arrives while the reader is parked. |
| Input typed while the terminal was cooked is invisible to `poll` afterwards, so every later keystroke lands one behind | High if unaddressed, and unattributable | §7.1: the line discipline holds it and the edge-triggered poll underneath crossterm never reports it, so resume flushes the terminal's input queue rather than hoping to read it. The `:!` prompt reads its key with a level-triggered `poll(2)` for the same reason. |
| ctrl-C at a child kills `ted` too, losing unsaved buffers | High | §7.1: a do-nothing handler for SIGINT and SIGQUIT for exactly as long as `ted` is out of raw mode, which `exec` resets to the default in the child so the key still interrupts what it was aimed at. |
| `:Explore` depends on a binary that may be absent or an unexpected version | Low | §13.4: the command is a setting rather than a hard-coded binary, presence is detected with `which`, and a miss reports on the notification line. `ted` reads only the chooser file, so it does not depend on the child's output format. |

---

## 23. Decisions and remaining questions

### Decided

| Question | Decision |
|---|---|
| Chrome policy | **Hide all Zed chrome.** The editor owns the whole grid; `ted` draws its own status line. Reported-bounds mirroring (§10.2) keeps re-enabling chrome additive. §9, §21/M1. |
| Platform coverage for M1 | **Linux and macOS together.** M0 validates the cell contract and the reader-thread bridge on both before M1 starts. §6, §7, §21. |
| Vim default | **Vim on by default**, `--no-vim` for plain Zed bindings. A nano-style keymap is deferred past M2 if wanted at all. §14.1. |
| Overlay strategy | **Own implementations for M2, Mirror from M3** via a defaulted `PickerDelegate::text_for_match` hook. §13.1, §20.2. |
| `:` command line | **`ted`'s own bottom line**, resolved through vim's existing interceptor rather than projecting Zed's palette modal. §13.2. |
| File browsing and management | **Suspend to an external file manager**, Yazi by default, instead of building a project panel. Better at file management than anything `ted` would write, and its disk mutations propagate through the real `Project`'s fs watching for free. The fuzzy finder stays native. §7.1, §13.4, §21/M4. |
| Terminal-host `:` commands | **A `ted`-local host-command table checked before the interceptor**, for commands that exist only because `ted` owns a tty. Not a second vim command parser; anything expressible as an action stays with the interceptor. §13.4. |
| File finder shape | **Helix's picker, two columns, no preview pane** — a bordered box, query and match count on the top row, then file name and dimmed directory. Ranked by score alone; recents only when the query is empty. §24.4. |
| Go to line | **Nothing new.** `:42` through vim's interceptor is the feature; no preview, no `ted`-owned surface. The only code is a bare number prompt for `--no-vim`, where there is no `:` line. §24.5. |
| Buffer switching | **A small box in the top middle, most recent first**, where Zed puts its own switcher. Cycling only — no query row, no preview, no closing from the list. One surface, one verb. §24.6. |
| Several items in one pane | **A tab strip along the top, shaped like Zed's**, which costs one rect offset in the projection. No numbers, overflow scrolls to keep the active tab visible, labels from `Pane::tab_details`. `:q` closes a tab and the rest take its place. §24.7. |
| The `:` line | **Ghost text, not a list.** One row: the rest of the best-matching command dimmed after the cursor, `right` accepts it. Nothing ever covers the buffer for a half-typed command. Its right-hand end takes the first thing that fits — the selected action's keybinding, then the candidate count. §24.3. |
| Keys in `ted`'s own surfaces | **`ctrl-n` / `ctrl-p` move a selection, and only those** — not `ctrl-j`/`ctrl-k`, not the arrows. Every surface opens with an empty query, and there is no `:` command history. §24.2, §24.3. |
| Overlay behaviour | **Floats over the editor and never reserves rows**, so the buffer behind it never relayouts — and therefore **grows to fit its matches**, downward from a fixed top edge, up to a cap. The query row never moves; only the bottom border does. The selected row is the theme's selection background and nothing else — no bar, no caret — and the editor behind is left undimmed. §24.1. |
| Terminals `ted` targets | **kitty and Ghostty, with a complete font.** No ASCII fallback for box drawing and no capability checks around it. This also makes §8.2's legacy keyboard mode and §12's 16-colour tier work for terminals `ted` no longer aims at — both are M3 items and can be dropped rather than built. §24.1. |
| Diagnostics | **Severity in the buffer, message on demand.** A straight coloured underline (not a curl, which would cost a hand-written Ratatui backend), severity by colour with no glyph, dead code dimmed, and nothing that moves the code being read. `shift-k` opens a railed panel over the editor carrying the diagnostic *and* the language server's documentation, the rail's colour saying which is which. §24.8. |
| Status line, and completions | **Both out of M2** (§21/M3.5). The status line needs a design rather than another field, and the completions popup is the one M2 surface that needed an upstream `editor` change — deferring it leaves M2 requiring no change to any crate but `ted`. §24.9, §24.10. |
| Where `ted`'s own settings live | **`ted.json`, beside `settings.json`**, read by `ted` alone — not a `ted` section in Zed's settings schema. `settings.json` is shared because its keys mean the same thing in both; a key only `ted` can act on does not, and putting it there would make one run of `ted` a permanent addition to a GUI user's configuration. Switching between GUI and TUI configures neither. §9. |

### Remaining

1. **Crate/binary name.** `ted` assumed from the branch name; nothing depends
   on it.
2. **Windows.** Out of scope through M4 as specified. Terminal key encoding
   there differs enough to be its own effort; revisit once Linux and macOS are
   stable.
3. **Ambiguous-width characters.** `unicode-width` and the user's terminal can
   disagree on East Asian ambiguous-width. `ted` is self-consistent either way,
   but the *setting* needs a default chosen — narrow (matches most modern
   terminals) is the likely answer, with an override.
4. **How much of a language server's markdown the hover panel should render.**
   M2 flattens it to text (§24.8), which is enough to read a type signature and a
   doc comment. What fences, emphasis and lists should look like in cells is a
   small renderer of its own and travels with §21/M3.5.

---

## 24. M2 — Navigation, surface by surface

§21's M2 line names seven features. This section says what each one *is*: the
state it reads, the keys it answers to, what it puts on the grid, and the choices
still open. M1 made one buffer editable; M2 is the milestone where `ted` stops
being one buffer, and every feature in it answers one of two questions — *what
else is there* (the finder, the switcher, several items in a pane) or *what is
here* (`:` completions, go-to-line, diagnostics, language-server completions).

| Feature | The surface it needs | § |
|---|---|---|
| `:` completions | the `:` line itself — ghost text, no list | 24.3 |
| File finder | a Helix-shaped picker, name and directory columns | 24.4 |
| Go to line | nothing new — `:42` already does it | 24.5 |
| Buffer switching | a small box, top-middle, most recent first | 24.6 |
| Several items in one pane | a tab strip, and what `:q` closes | 24.7 |
| Diagnostics inline | underline in the buffer, message on `shift-k` | 24.8 |
| Language-server completions | **deferred past M2**; §24.9 records the shape | 24.9 |

Two of them — the finder and the switcher — are the same widget with different
content, which is why §24.1 specifies that widget once and those two sections
only say what fills it. Nothing else in M2 is an overlay: the `:` line stays one
row, the tab strip is a row of its own, and diagnostics live in the buffer until
asked about.

**Two things M2 deliberately does not touch.** The status line needs a design of
its own and gets a milestone of its own (§21), so nothing here adds to it —
where a surface wants to report something the status line would carry, it waits.
And language-server completions move out with it (§24.9): the popup is the one
M2 surface that needed an upstream `editor` change, and deferring it leaves M2
with no required change to any crate but `ted`.

Two constraints from earlier sections govern all of it. §13.1 decided **Own for
M2**: these are `ted`'s own lists over Zed's own data, not projections of Zed's
`Picker` views, and the Mirror hook that replaces them is M3. And §10's rule
still holds — the `ViewSnapshot` carries no GPUI handles, so every list below is
plain rows of text and byte offsets by the time the renderer sees it.

### 24.1 The list overlay: one widget, two consumers

**What it is.** A rectangle of rows painted over the editor, with an optional
query line, a selected row, and a footer. §10.1 already reserves the field
(`overlay: Option<OverlayView>`); M2 is where it acquires a shape:

```rust
pub struct OverlayView {
    /// Painted into the top border, Helix-style: "files", "buffers".
    pub title: Option<String>,
    pub query: Option<QueryView>,        // absent when the list is not filtered
    pub rows: Vec<OverlayRow>,
    pub selected: Option<usize>,
    pub footer: Option<String>,          // "3/412", "no matches"
    /// Grid (§24.4) | TopCentre (§24.6) — AtCell arrives with §24.9
    pub placement: OverlayPlacement,
}

pub struct OverlayRow {
    pub label: MatchedText,
    /// The second column, dimmed: a file's directory. Left-aligned at a column
    /// computed from the widest label rather than right-aligned, because two
    /// files called `snapshot.rs` are told apart by a column that starts in the
    /// same place on every row (§24.4).
    pub detail: Option<MatchedText>,
    /// `•` after the label: unsaved work (§24.6, §24.7).
    pub modified: bool,
}

/// Byte offsets that matched the query, emphasised by the renderer. Both
/// `fuzzy_nucleo`'s matchers and `CommandInterceptItem` already report their
/// matches in exactly this form.
pub struct MatchedText {
    pub text: String,
    pub matched: Vec<usize>,
}
```

The rows carry no `StyledSpan`s. Neither M2 consumer has anything to put in
them — the syntax-coloured row belongs to the completions popup (§24.9) — and a
field nothing fills is a field the renderer has to guess the meaning of.

**It floats; it does not reserve.** This is the one structural decision in
§24 and it is not cosmetic. `ted`'s bottom lines are *reserved* rows: the GPUI
window is sized to the grid minus them (§10.2), so a line appearing resizes the
window (`frame.rs`, `wanted != reserved` → `resize_window`). A resize relays out
the editor and, on a large file, starts an asynchronous rewrap
(`DisplayMap::is_rewrapping`). A ten-row list that grows and shrinks with every
keystroke of a query would therefore reflow the whole buffer on every keystroke,
and the user would watch the text behind the finder shuffle while typing. So the
overlay is painted *over* the editor's cells, after the editor and before the
bottom lines, and the window keeps its size. The editor underneath is unchanged
and simply obscured.

Two consequences worth stating because they are easy to get wrong:

- **The hardware cursor moves into the overlay.** §7 places the terminal's real
  cursor at the editor's primary cursor; while an overlay owns the keyboard the
  cursor belongs in its query field, as a bar, regardless of vim's mode. An
  overlay with no query (§24.9) hides the cursor rather than leaving it under
  the popup.
- **Overlay text goes through the same placement path as buffer text.**
  `render::write` already walks graphemes and honours cell widths; `matched`
  offsets are byte offsets, so they map through `byte_to_cell_table` like every
  other conversion (§5.4). No second conversion path.

**Decided: a box grows to fit its matches, downward, from a fixed top edge.** No
blank rows are ever painted inside a box. The top edge — and with it the query
row, which is the row being typed on — stays where it is; only the bottom edge
moves, so the text under the cursor never shifts while the box is open.

This is free precisely because §24.1's other decision went the way it did. If the
overlay reserved rows, a height that changed on every keystroke would resize the
GPUI window on every keystroke, and the buffer behind it would relayout and
rewrap each time. Floating over the editor means a changing height costs one more
row of painting and nothing else.

**Scrolling.** The box grows only to a cap — half the grid, since a finder that
covers the file it is about to open is a worse finder. Past that the list scrolls
to keep the selection visible, and the finder's footer says how many of the files
it searched matched. The switcher has no second number to report and carries no
footer.

**Both consumers are bordered boxes**, differing only in size and where they sit:
the finder fills the grid (§24.4), the switcher is small and centred at the top
(§24.6). Both are places you go, rather than lines you type on — which is why the
`:` line, which you type on, stayed one row (§24.3). Nothing is anchored to a
cell until completions arrive (§24.9).

**No ASCII fallback.** `ted` targets kitty and Ghostty with a complete font, so
box drawing, `▌`, `•` and `⋯` are assumed to render. Nothing in §24 needs a
`+-|` twin, and none of it is guarded by a capability check.

**Decided: the selected row is a background tint, and nothing else.** No `▌` bar,
no `>` caret, so every row starts at the same column and no cells are spent on
marking three-quarters of the list as *not* selected. The tint is the theme's
own — `EditorStyle`'s selection background, the same colour a selection in the
buffer uses (§10.3) — so it moves with the theme rather than being a colour `ted`
invented.

**The editor behind a box is not dimmed.** The border and the tint already say
what is in front, and dimming would mean repainting every cell outside the box
with a modified style on every frame — the one thing §19's budget says the
terminal write should not be doing.

### 24.2 Who owns the keyboard, and how a surface opens

**What it is.** M1 had two keyboard owners: the `:` line, and everything else
(GPUI's dispatch tree). M2 adds a third — an open overlay — and the rule stays
the same: exactly one owner per keystroke, and `ted`'s own surfaces sit in front
of GPUI, so a key that reaches an overlay never reaches the dispatch tree.

The interesting half is not who owns a key but **how the surface opens at all**.
`ctrl-p` is `file_finder::Toggle` (`assets/keymaps/default-linux.json`), `ctrl-g`
is `go_to_line::Toggle`, `ctrl-tab` is `tab_switcher::Toggle`, and vim's own
keymap adds `space f` and `space b`. Those bindings resolve inside GPUI, and what
they open is a `Picker` modal `ted` cannot paint — today that lands on the "a
modal is open that `ted` cannot render" notification (`snapshot.rs`).

Three ways to get in front of that, in descending order of preference:

1. **Rewrite the binding, not the key.** `ted` already loads and edits the keymap
   at bootstrap — `filtered_action_names` drops the font-size bindings (§5.5).
   The same pass can *retarget* every binding whose action is a modal `ted` owns
   onto a `ted`-local action (`ted::OpenFileFinder`, `ted::OpenBufferSwitcher`,
   `ted::GoToLine`, and `ted::Hover` for `shift-k` — §24.8 builds that box from
   the buffer's diagnostics, so the editor's own hover machinery is not wanted),
   handled by `App::on_action`
   (`crates/gpui/src/app.rs:2164`) — a global listener, which runs at the end of
   the bubble phase and therefore only when nothing in the window consumed the
   action. Nothing else registers these, so nothing else can. The handler raises
   a flag the frame loop reads. Every binding keeps working — including
   multi-keystroke vim ones and anything in the user's `keymap.json` — because
   GPUI still resolves the keystroke; only the action at the end of it changes.
   Recommended.
2. **A keystroke table checked before dispatch**, the shape `opens_command_line`
   already has. Simple, but it can only see single keystrokes: `space f` is a
   two-key binding resolved inside GPUI, and duplicating the keymap to find it
   would be exactly the drift §13.1 is trying to avoid.
3. **Let the modal open and mirror it.** The right long-run answer, and it is
   M3's Mirror strategy (§13.1, §20.2). Not M2.

**The table is consulted in two places, not one.** Rewriting the keymap catches
every *keystroke*, and nothing else. A `:` command reaches the same modal by a
different road: `:ls` and `:buffers` resolve inside vim's interceptor to
`tab_switcher::ToggleAll` (`crates/vim/src/command.rs:1665-1666`), which arrives
at `ted` as a `Box<dyn Action>` that no keymap pass has seen and that
`CommandPaletteFilter` does not touch, because the interceptor is asked before
any filtering. So the same action → surface table is applied a second time at the
moment `ted` dispatches an effect from the `:` line (`Effect::Dispatch`), turning
an action `ted` owns into its own overlay. With only the keymap half, `ctrl-tab`
and `:ls` do different things.

With both halves in place, the originals are hidden from the `:` line's
action-name fallback through `CommandPaletteFilter`, the way the font-size
actions are, and `ted`'s own actions take their place in the completion list.

**Keys, once a surface is open.** One vocabulary across every consumer, so
nothing has to be learnt twice:

| Key | Effect |
|---|---|
| printable, `backspace`, `delete`, `left`/`right`, `home`/`end` | edit the query |
| `ctrl-n` / `ctrl-p` | move the selection — and *only* these |
| `enter` | confirm the selection |
| `esc`, `ctrl-c` | dismiss, leaving the editor as it was |
| `ctrl-s` / `ctrl-v` | confirm into a horizontal / vertical split (M4 renders them) |

`esc` dismisses the overlay rather than quitting `ted`, exactly as it cancels an
open `:` line today.

**Two decisions inside that table, both deliberate.** `ctrl-j`/`ctrl-k` do not
move the selection, and neither do the arrow keys: one pair of keys does one job
everywhere, and nothing else claims `up`/`down` (§24.3 does not — there is no
command history). And **a surface always opens with an empty query** —
nothing is recalled from the last time it was open, so the first character typed
is always the first character of the search rather than an edit to a query the
user has to notice and clear.

### 24.3 The `:` line's completions

**Decided: ghost text on the line, and no list.** The `:` line stays one row. The
rest of the selected command appears dimmed after the cursor, `right` accepts it
into the query, and a count on the right says how many other candidates the
matcher found. Nothing ever covers the buffer for a half-typed command, and the
`:` line is not a consumer of §24.1's overlay after all.

```
  41     let snapshot = build_snapshot(&session, reserved)?;
  42
  43     render(&snapshot, &session.palette, frame.buffer_mut());
  44 }
  45
 :wq — write and quit                                        ctrl-n: 4 more
 NORMAL crates/ted/src/frame.rs [+]                                  128:12
```

**Keys.** `ctrl-n` / `ctrl-p` move through the candidates, replacing the ghost;
`right` at the end of the query accepts the ghost; `enter` runs the selected
candidate whether or not the ghost was accepted; `esc` cancels. `up` / `down` do
nothing here.

Two smaller gaps go with it:

- **Matching.** Action names are matched today by a hand-rolled subsequence test
  ordered by label length (`command_line.rs`, `matching_action_names`), which
  decides which candidate the ghost shows — so with only one candidate visible,
  the ordering *is* the feature. Zed's palette scores with
  `fuzzy_nucleo::match_strings_async(.., Case::Smart, LengthPenalty::On, ..)`
  (`crates/command_palette/src/command_palette.rs:491`; the matcher itself at
  `crates/fuzzy_nucleo/src/strings.rs:100`). Calling it with the palette's own
  arguments means `:w` ghosts whatever `:w` would have selected in GUI Zed, and
  `fuzzy_nucleo` is the matcher the finder uses too.
- **The keybinding, when there is room.** `Window::keystroke_text_for(&dyn
  Action)` (`crates/gpui/src/window.rs:4861`) renders an action's binding as
  text. With one row to work with it competes with the candidate count for the
  right-hand end, which is the open question below — but showing it is what makes
  `:save` teach you that `ctrl-s` was faster.

**Where the state comes from.** Unchanged from §13.2 — the host table first
(§13.4), then `GlobalCommandPaletteInterceptor::intercept`, then action names
when the interceptor is not exclusive. M2 changes the ranking and the rendering,
not the resolution.

**Decided: the right-hand end takes the first thing that fits**, in one order —
the selected action's keybinding when it has one, and the candidate count
(`ctrl-n: 4 more`) otherwise. Neither is worth truncating and neither is worth
displacing the ghost, so when even the count does not fit beside what is already
on the row, nothing is painted there rather than something misleading.

**Decided: a host command's ghost reads like any other.** `:Explore` and `:!` are
`ted`'s own (§13.4), and a colour saying so would answer a question nobody asks
at the moment they are typing it — the host table is checked first, so what a
host command actually gets is the top of the list, which is the answer that
matters.

**No command history.** vim recalls previous `:` commands on `up`/`down`; `ted`
does not, and it is not on the roadmap — so `up`/`down` stay unbound on the `:`
line rather than being reserved for it.

### 24.4 The file finder

**What it is.** Type a few characters, get the project's files ranked, press
`enter`, the file opens in the active pane. It is the only navigation surface
with no vim-command equivalent to fall back on: Zed maps `:e`/`:edit` to
`editor::actions::ReloadFile` rather than to opening a file
(`crates/vim/src/command.rs:1475`), and the commands that do take a path —
`:tabe <file>`, `:sp <file>`, `:vs <file>` (`:1499-1524`) — require typing it in
full. §13.4 already settled that the finder and `:Explore` both exist and neither
replaces the other: one searches, the other browses.

**Where the state comes from.** Zed's own delegate cannot be reused —
`FileFinderDelegate` is a public struct whose constructor and every field are
private (`crates/file_finder/src/file_finder.rs:368-390`, `:960`), which is the
concrete reason §13.1's "Own for M2" is the only option that does not need an
upstream change first. Everything underneath it is public, and `ted` calls it
directly:

| What | Call |
|---|---|
| The worktrees to search | `WorktreeStore::visible_worktrees_and_single_files(cx)` (`crates/project/src/worktree_store.rs:441`) |
| A candidate set per worktree | `project::PathMatchCandidateSet { snapshot, include_ignored, include_root_name, candidates: Candidates::Files }` (`crates/project/src/project.rs:6477-6489`) |
| Ranking | `fuzzy_nucleo::match_path_sets(sets, query, relative_to, Case::Ignore, max, &cancel_flag, executor)` (`crates/fuzzy_nucleo/src/paths.rs:264`) → `PathMatch { score, positions, path, .. }` (`:46`) |
| An empty query's rows | `Workspace::recent_navigation_history(limit, cx)` (`crates/workspace/src/workspace.rs:2763`) |
| Opening | `Workspace::open_path`, the same call vim's `:e <file>` handler makes (`crates/vim/src/command.rs:641-666`) |

That is the sequence `FileFinderDelegate::spawn_search` runs
(`file_finder.rs:1043-1090`). What `ted` does not inherit is the merge of recents
with fresh results and its comparator (`Matches::cmp_matches`,
`file_finder.rs:629`), which is private — so the ranking policy is a decision
`ted` makes rather than a port.

**The search is asynchronous and cancellable, and the frame loop keeps
painting.** `match_path_sets` takes an `&AtomicBool` and a background executor; a
keystroke arriving while a search is in flight sets the flag and starts the next
one. Until it lands, the overlay keeps showing the previous result set rather
than blanking — on a repository this size the query changes faster than the scan
finishes, and a list that empties between keystrokes reads as broken.

**Decided: Helix's picker, in two columns, with no preview pane.** A bordered box
the width of the grid, its top edge fixed and its height following the match
count (§24.1); the query on the top row behind a `>` prompt with the count
right-aligned beside it; a rule; then one row per match, the file name in a left
column and its directory dimmed in a right one. Two files called
`snapshot.rs` are told apart by the column next to them rather than by reading a
path from the left, and no width goes to previewing a file that is one keystroke
from being open anyway.

```
╭─ files ──────────────────────────────────────────────────────────────────────╮
│ > ted/sna▏                                                             3/412 │
├──────────────────────────────────────────────────────────────────────────────┤
│ snapshot.rs          crates/ted/src                                          │
│ render.rs            crates/ted/src                                          │
│ snapshot.rs          crates/ted/tests                                        │
│                                                                              │
╰──────────────────────────────────────────────────────────────────────────────╯
```

**Also decided, by consequence rather than by preference:**

- **No `ctrl-h`.** Ignored files stay out; `include_ignored` is left `false`.
- **Ranking is the matcher's score alone**, which is what Helix does and what the
  borrowed shape implies. An empty query has nothing to score, so it lists
  recents from `Workspace::recent_navigation_history`; the moment a character is
  typed, score decides and recency stops mattering.
- **Open files are not marked.** The tab strip (§24.7) is already on screen and
  already says what is open, so a second answer in the finder would only be a
  second thing to keep true.
- **No icons.** They need a patched font to be anything but mojibake, and the
  directory column is doing the work an icon would.

**Direction wanted.** Nothing here — the surface is fully specified. What is left
belongs to §24.1: how tall the box is, and how the selected row is marked.

### 24.5 Go to line

**Decided: `ted` builds nothing.** `:42` and `:$` already parse in vim's
interceptor into a `vim::GoToLine` action and dispatch like any other
(`crates/vim/src/command.rs:1859-1865`, handler `:865-887`), with no modal
anywhere on the path, and that is the whole feature. No preview, no `ted`-owned
line-jump surface, no second syntax. The rest of this section records what M2
therefore does *not* do, so none of it is rediscovered later as a gap.

- **No preview.** Zed's modal highlights the target row and centres it as the
  number is typed, restoring the view on cancel
  (`crates/go_to_line/src/go_to_line.rs:198-207`). `:42` jumps on `enter`, and
  nothing happens before that.
- **The line lands where vim puts it.** The modal confirms with
  `SelectionEffects::scroll(Autoscroll::center())` (`:294`); vim's handler uses
  default effects (`command.rs:865-887`), so the target lands wherever the
  minimum scroll puts it — often the bottom row. Only vim's path exists in `ted`,
  so there is nothing to disagree with until the modal is mirrored (M3), at which
  point the same keystroke would have two behaviours and one of them has to give.
- **`row:column` stays unavailable.** The modal parses `row:column` and relative
  `+N`/`-N`/`fN`/`bN` (`go_to_line.rs:238-278`); vim's range parser handles `$`,
  `.`, `%`, `+N`, `-N` (`command.rs:1264-1291`) and no columns. §13.4 forbids
  `ted` from parsing `:` commands itself, so `:42:8` is an upstream addition to
  vim or it is nothing.
- **`ctrl-g` is a status-line question, and moves with it.** Under vim it is
  `vim::ShowLocation` (`assets/keymaps/vim.json:75`), which writes
  `Vim::status_label` (`crates/vim/src/vim.rs:553`, set at
  `crates/vim/src/normal.rs:1027`) for a status-bar item `ted` does not render.
  Nothing appears. That belongs to the status-line milestone (§21), not here.
- **Without vim, `ctrl-g` is the one thing left to build**, because there is no
  `:` line at all in a `--no-vim` session (§13.2 opens it only from a vim mode)
  and `go_to_line::Toggle` would open a modal `ted` cannot paint. §24.2's table
  routes it to a bare number prompt drawn like the `:` line, with no interceptor
  behind it — a dozen lines, and the only go-to-line code in `ted`.

### 24.6 Buffer switching

**What it is.** The list of what is already open, most-recently-used first, with
type-to-filter over it — Zed's tab switcher, as one of `ted`'s own lists.

**What works today, and what is dead.** vim's command table resolves
`:bn[ext]`, `:bN[ext]`, `:bp[revious]`, `:bf[irst]`, `:br[ewind]`, `:bl[ast]` and
the whole `:tab*` family to real `workspace` actions, with counts
(`crates/vim/src/command.rs:1655-1685`), and those reach a real `Pane` in `ted`
today. `:ls` and `:buffers` resolve to `tab_switcher::ToggleAll` (`:1665-1666`),
and `ctrl-tab` to `tab_switcher::Toggle` — both open a `Picker` modal `ted`
cannot paint, so both are worse than missing: they swallow keys until `esc`.
Those two are what M2 fixes.

`:ls` is the case that forces §24.2's table to be consulted at dispatch as well
as at keymap load: it arrives as an action the keymap pass never saw.

**Where the state comes from.** The items as in §24.7, and the order from
`Pane::activation_history()` / `Workspace::recently_activated_items(cx)` — the
same two sources Zed's own switcher sorts by
(`crates/tab_switcher/src/tab_switcher.rs:430`, `:502`), so "the second entry is
where I just came from" holds in both. Filtering is `fuzzy::match_strings` over
the labels, as everywhere else in §24.

**One terminal-specific caveat.** ctrl-tab in a GUI is a *hold*: keep ctrl down,
press tab to walk the list, release to confirm. That requires key-release events,
which arrive only under the Kitty protocol's `REPORT_EVENT_TYPES` (§8.2) — and
`ted` filters releases out today. Building the interaction on a key the terminal
may never report would make the switcher behave differently on Terminal.app than
on Ghostty, so the list confirms on `enter` and cancels on `esc` everywhere, and
`ctrl-tab` while it is open simply moves the selection down.

**Decided: a box in the top middle, most recent first.** Where Zed puts its own
switcher, in the vocabulary §24.4 borrowed from Helix — a bordered box, the label
column then a dimmed directory column, the selection on the second row because
the first is the buffer you are already in. It floats like every other overlay
(§24.1), so nothing behind it moves.

```
   1 //! The frame loop (SPEC §7) and the terminal's mode.
   2          ╭──────────────────────────────────────────────╮
   3          │ frame.rs •       crates/ted/src              │
   4          │ snapshot.rs      crates/ted/src              │   ← selected
   5          │ SPEC.md          crates/ted                  │
   6          │ element.rs       crates/editor/src           │
   7          ╰──────────────────────────────────────────────╯
   8 use std::io::{Write, stdout};
```

**Decided: cycling only, with no query row.** The switcher is not a search — it
is the list of what is open, and there are rarely enough of them to filter.
`ctrl-tab` (and `ctrl-n`/`ctrl-p`) walks it, `enter` switches, `esc` cancels.
That keeps it three rows shorter than the finder and keeps the two surfaces from
blurring into each other: one is where you go to find a file, the other is where
you go back to one.

**No preview.** The selection moving does not activate anything; `enter` does.
Zed's switcher behaves this way, and previewing would make `esc` a state change
rather than a no-op.

**No closing from the list either.** `tab_switcher::CloseSelectedItem` has no
counterpart here; `:q` and `:bd` close buffers, and the switcher only switches.
One surface, one verb.

**Direction wanted.** Nothing, beyond §24.1's shared questions.

### 24.7 Several items in one pane

**What it is.** A pane holds a list of items and shows one. `ted` paints the
active one and says nothing about the rest, because the tab bar is hidden (§9)
and the status line reports a single path. That was honest in M1, when nothing
could open a second file; from M2 on, `:Explore` returning three paths and the
finder opening a fourth make it a hole of exactly the kind §14.3 refuses to
accept — state the user can navigate into but cannot see.

The navigation itself already works. vim's keymap binds `] b` / `[ b` to
`pane::ActivateNextItem` / `ActivatePreviousItem`, `g t` / `g T` to
`vim::GoToTab` / `GoToPreviousTab`, and `[ B` to `pane::ActivateItem(0)`
(`assets/keymaps/vim.json`), all of which reach a real `Pane` through the
dispatch tree and are correct today. What M2 owes is *sight* of it, and the
closing semantics that follow.

**Where the state comes from.** `Pane::items()` and `items_len()` for the list,
the pane's active index for the marker, `Item::tab_content_text(0, cx)` for each
label — the same call `snapshot::build` already makes for the status path, so
labels are exactly the ones GUI Zed's tabs would carry — and `Item::is_dirty` for
the modified marker.

**Decided: a tab strip along the top, shaped like Zed's.** One row, the active
tab carrying the editor's own background and the inactive ones a darker ground,
separated the way Zed separates them. The strip is `ted`'s to paint, like the
status line, and it is the only thing M2 adds above the editor.

```
 snapshot.rs │ frame.rs • │ render.rs │ SPEC.md
   1 //! The frame loop (SPEC §7) and the terminal's mode.
   2
   3 use std::io::{Write, stdout};
```

**What that costs, mechanically** — this is the "rect offset". `ted` reserves
rows only at the *bottom* today: `render::reserved_rows` counts upward from the
status line, the GPUI window is sized to the grid minus that count, and the
editor is painted at the rectangle the editor reports, whose row 0 is the
terminal's row 0. A strip on top has nothing to do with that count; it means
every rectangle the editor reports has to be shifted down by the strip's height
before anything is painted from it. That is one addition in one place in
`snapshot::build`, applied to `text_rect`, `gutter_rect` and the cursor together.
Ten lines. The only way to get it wrong is to apply it to two of the three, which
puts the cursor a row off its text — §5.4's failure mode wearing a new costume.

**Decided: `:q` closes a tab and the others take its place.** vim's interceptor
resolves it to `workspace::CloseActiveItem`, which does exactly that already. Two
details follow: a save prompt (§13.3) must name the file it is about, since the
item being closed need not be the one last looked at; and `ctrl-c` is the only
single-key way out while any item remains.

**Deferred: what happens when the last one closes.** `ted` exits when every pane
is empty (`Backend::is_empty`) — "no buffers" is how quit reaches the frame loop
today. That conflates closing a file with ending a session, and a later milestone
replaces it: an empty pane should be a state `ted` can sit in, with quitting made
explicit. Not M2; recorded here so the current behaviour is understood as a
placeholder rather than a decision.

**Decided, in the strip's details:**

- **The active tab is the editor's own background**, inactive ones a darker
  ground, exactly as Zed paints them — one row, no rule underneath, no second row
  spent on marking which tab is live.
- **No numbers.** `3gt` works and stays undiscoverable; the strip is a list of
  file names, not a keyboard reference, and two cells per tab buys more room for
  the names themselves.
- **`•` after the label** is unsaved work, matching the switcher (§24.6).
- **Overflow scrolls** so the active tab is always visible, the way Zed's tab bar
  behaves. Eliding the middle would keep two tabs the user is not looking at and
  hide the one they are.
- **Labels come from `Pane::tab_details(items, window, cx)`**
  (`crates/workspace/src/pane.rs:4913`), which is public and computes exactly the
  detail level each tab needs — so two files called `mod.rs` grow a directory in
  their labels and nothing else does.

**Direction wanted.** Nothing.

### 24.8 Diagnostics in the buffer

**What it is.** Two things, deliberately kept apart: a mark under the offending
text, which is always on, and the message, which appears only when asked for.
`ted` gets the mark nearly free and has to reconstruct the message, because it is
not readable where Zed keeps it.

**The mark is already flowing, and its colour is being thrown away.**
`highlighted_chunks(.., LanguageAwareStyling { diagnostics: true }, ..)` — the
call `snapshot.rs` already makes — bakes severity into each chunk's
`HighlightStyle` through `diagnostic_style(severity, ..)`
(`crates/editor/src/display_map.rs:1907-1941`). `span_style` keeps only
`underline: highlight.underline.is_some()` and discards the `UnderlineStyle`'s
colour and its `wavy` flag, so an error and a warning look identical today.
Carrying the colour through is the highest-value line in §24.8. What a terminal
can do with it:

- **Coloured underline** is `SGR 58`, which crossterm can emit
  (`SetUnderlineColor`) and Ratatui carries as `Style::underline_color` — but
  only with its `underline-color` feature, which is in Ratatui's default set and
  therefore *off* here, since the workspace takes `default-features = false` with
  `crossterm_0_29` and `std` (root `Cargo.toml`). A one-line change.
- **Undercurl** (`CSI 4:3 m`) has no `Modifier` bit in Ratatui at all — there is
  a single `UNDERLINED` — although crossterm knows `Attribute::Undercurled`.
  Reaching it means `ted` implementing `Backend` itself rather than using
  `CrosstermBackend`, since that is where cell attributes become escape
  sequences; writing the escape around Ratatui instead would leave its diff
  holding a wrong belief about the cell. Open, below.

Where the raw severity is wanted rather than the styled colour — a gutter marker,
a count — it is one layer down and fully public: `MultiBufferSnapshot::chunks`
yields `Chunk { diagnostic_severity, is_unnecessary, underline, .. }`
(`crates/language/src/buffer.rs:530-557`).

**The message is not readable where Zed puts it.** Jumping to a diagnostic makes
`Editor::activate_diagnostics` render the group into *block* rows
(`crates/editor/src/diagnostics.rs:381-421`) behind a `RenderBlock` closure —
`Arc<dyn Fn(&mut BlockContext) -> AnyElement>`
(`display_map/block_map.rs:159`) — which is a private field on `CustomBlock` and
returns an opaque element even if it were not. `blocks_in_range` hands `ted` the
row and its height and nothing else. So `] d` in `ted` today jumps to the
diagnostic and opens blank rows where the message belongs.

`ted` reads the diagnostics from the buffer instead, which is public end to end:
`EditorSnapshot` → `DisplaySnapshot::buffer_snapshot()` (`display_map.rs:1589`) →
`MultiBufferSnapshot::diagnostics_in_range(range)`
(`crates/multi_buffer/src/multi_buffer.rs:6198`) or `diagnostic_group(..)`
(`:6176`) → `DiagnosticEntryRef { range, diagnostic }` → `Diagnostic { severity,
message, group_id, is_primary, is_unnecessary, source, .. }`, every field public
(`crates/language/src/diagnostic.rs:7-48`). That is the same source
`Editor::inline_diagnostics` privately caches, so `ted` reads the original rather
than a copy of it.

Two things follow. Any message `ted` shows is its own text laid out in cells, not
a projection of anything. And the editor's blocks are still there, occupying
display rows and painting nothing — so `ted` keeps them from being inserted at
all, which the settled design below makes easy: with the message on `shift-k`
rather than under the line, there is nothing the blocks would have contributed.

**Decided: the buffer shows severity and nothing else.** A coloured underline
under the offending range, the colour carrying the severity with no glyph beside
it, and dead code (`Diagnostic::is_unnecessary`, which is what Zed fades) dimmed.
No end-of-line text, no rows inserted under the line, no gutter marker — the diff
marker keeps the gutter cell it has, and nothing the language server says is ever
allowed to move the code the user is reading.

**Decided: a straight coloured underline, not a curl.** `SGR 58` through
Ratatui's `Style::underline_color`, behind the `underline-color` feature (§20.1).
A real undercurl would mean `ted` writing its own `Backend` in place of
`CrosstermBackend` — around 150 lines, most of it Ratatui's own diff logic — for
a difference the colour is already carrying.

**Decided: the message appears on `shift-k`, in a railed panel over the editor.**
`shift-k` is `editor::Hover` (`assets/keymaps/vim.json:78`), so the binding
exists and needs no keymap work; `ted` retargets it the way §24.2 describes. The
panel is a tinted block with a coloured bar down its left edge — no border, so
nothing needs an ASCII fallback and four more cells go to the text. It dismisses
on the next key.

**The rail's colour says what kind of thing is talking.** The severity's colour
for a diagnostic; a neutral accent for documentation from the language server. A
position with both stacks them in one panel, diagnostics first, each with its own
rail, so the two are never confused for one message.

```
  39     let reserved = reserved_rows(command_line.is_some(), prompt);
  40     let snapshot = build_snapshot(&session, reserved)?;
  41   ▌ error E0061
  42   ▌ this function takes 5 arguments but 2 were supplied
  43   ▌ rust-analyzer · 1 of 3 · ]d for the next
  44   ▌
  45   ▌ fn build_snapshot(session: &Session, reserved: u16, cx: &mut AsyncApp)
  46   ▌ Reads the projection out of the entities. Must run after the draw.
```

**Documentation needs no upstream change either.** Zed's own hover popover keeps
its text as an `Entity<Markdown>` behind `HoverState::info_popovers`
(`crates/editor/src/hover_popover.rs:879-886`), which would mean reading rendered
markdown back out of a view. `ted` asks the project instead:
`LspStore::hover(&buffer, position, cx) -> Task<Option<Vec<Hover>>>`
(`crates/project/src/lsp_store.rs:8180`), reached through `Project::lsp_store()`
(`project.rs:2200`), returns `Hover { contents: Vec<HoverBlock { text: String,
kind }>, range, language }` — all public (`project.rs:895-918`), all plain
strings. `ted` lays them out itself, which is what it would do with the markdown
anyway.

Navigation between diagnostics already works and is unaffected: `] d` / `[ d` and
`g ]` / `g [` are bound to `editor::GoToDiagnostic` / `GoToPreviousDiagnostic` in
`assets/keymaps/vim.json`, `f8` / `shift-f8` in the Linux keymap.

**Markdown is flattened in M2 and rendered later.** `HoverBlock::kind` says
whether a block is markdown or plain text; M2 strips the markup and lays out the
text, which is enough to read a type signature and a doc comment. Turning fences,
emphasis and lists into terminal styling is a small renderer of its own and
travels with the other deferred work (§21/M3.5) — the flattened version is not a
placeholder that gets thrown away, it is the same panel with a better text pass
behind it.

**Direction wanted.**

- **Counts.** A tally of the project's errors and warnings —
  `Project::diagnostic_summary(false, cx)` returns exactly `{ error_count,
  warning_count }` (`crates/project/src/project.rs:5080`) — would live in the
  status line, so it waits for that milestone with everything else that would go
  there.

### 24.9 Completions from the language server — deferred

**Deferred past M2**, and with it the one upstream change M2 would otherwise have
required. What is settled is the shape: an outlined box at the cursor, placed
where Zed places its menu, as text.

```
  39     let reserved = reserved_rows(command_line.is_some(), prompt);
  40     let snapshot = build_sna▏
  41                    ╭────────────────────────────────────────────╮
  42                    │ build_snapshot      fn  (&Session, u16) -> │
  43                    │ build_editor_view   fn  (&mut Editor, u16, │
  44                    │ BUILD_PROFILE    const  &'static str       │
  45                    ╰────────────────────────────────────────────╯
```

The rest of this section is the survey the milestone that picks it up starts
from.

**What it is.** The popup that appears as you type — and the only one of these
surfaces that is a *pure projection*. Unlike the finder and the switcher, `ted`
would not own the keyboard while it is up: the menu is the editor's own state,
driven by editor actions that are already bound. `Editor && showing_completions` maps `enter` to
`ConfirmCompletion`, `tab` to `ComposeCompletion`, `ctrl-n`/`ctrl-p`/`up`/`down`
to `ContextMenuNext`/`Previous` and `pageup`/`pagedown` to First/Last
(`assets/keymaps/default-linux.json:823-880`); vim adds `ctrl-x ctrl-o` →
`ShowCompletions` (`assets/keymaps/vim.json:361`). That key context is derived
from the editor's own state (`crates/editor/src/editor.rs:2683-2696`), so every
one of those bindings resolves correctly in `ted` today. Keystrokes keep going
exactly where they go now; `ted` has only to *see* the menu and paint it.

**Seeing it is precisely what it cannot do.** `Editor::context_menu` is private
(`editor.rs:1006`) and the only public readers are `context_menu_visible() ->
bool` (`:4606`) and `context_menu_origin() -> Option<ContextMenuOrigin>`
(`:4615`). `ted` can know that *a* menu is open and roughly what it is anchored
to, and nothing about its contents — not the entries, not the selection, not even
whether it is completions or code actions. **This is why the feature is
deferred**: it is the only surface in §24 that cannot be built without an
upstream change (§20.2) — a reader in the shape of
`Editor::completions_menu(&self) -> Option<&CompletionsMenu>`, or better, a small
method returning entries and selection as plain data, which fits §10's
no-handles rule without `ted` touching editor-internal types at all.

Everything behind that reader is already public: `CompletionsMenu::entries` and
`selected_item` (`crates/editor/src/code_context_menus.rs:259`), `completions:
Rc<RefCell<Box<[Completion]>>>`, `Completion::label: CodeLabel { text, runs,
filter_range }` (`crates/language_core/src/code_label.rs:75-83`),
`Completion::documentation`, and `Completion::kind()`
(`crates/project/src/project.rs:6740`). `editor::styled_runs_for_code_label`
(`editor.rs:12118`) turns a label's runs into styled ranges — the same call the
GUI menu makes — so completion labels can carry syntax colour in the terminal for
free once the entries are reachable.

**Position is `ted`'s to compute.** The popup's bounds are decided inside
`EditorElement::layout_cursor_popovers` (`element.rs:3783-3924`) and never
written back onto the editor, so there is nothing to read — which is fine,
because `ted` already knows the cursor's cell. The list opens on the row below
the cursor, flips above when the rows are not there, and clamps to the text
rect. `context_menu_origin()` still matters for the anchor that is not the cursor
(`GutterIndicator(DisplayRow)`, which is how code actions deploy).

**Blocked on the same reader:** signature help (`Editor::signature_help_state`,
private, with no public reader at all — `editor.rs:1012`) and the code-action
menu (the same `context_menu` field). All three arrive together or not at all,
which is another reason they travel to a later milestone as one piece.

**Still open when it is picked up.** Whether the box carries a documentation line
under the list or nothing; whether the kind is a word (`fn`, `const`), a glyph or
a colour; whether the signature column earns its width; whether labels take their
syntax colour from `styled_runs_for_code_label` (free, and matches the GUI) or
stay plain with only the matched characters emphasised; and what to paint when
the open menu is code actions rather than completions.

### 24.10 What M2 leaves out, what it needs upstream, and when it is done

**Left out on purpose.**

| Left out | Where it goes |
|---|---|
| Language-server completions, signature help, code actions (§24.9) | a later milestone, together, since one reader unblocks all three |
| Markdown rendering in the hover panel (§24.8) | the same milestone; M2 flattens the text |
| The status line's design — counts, `ctrl-g`'s location string, an item counter, anything else that would live there | its own milestone (§21) |
| What "quit" means once the last item closes (§24.7) | the same milestone; `ted` exits on an empty pane until then |
| Mirror-strategy projection of Zed's pickers (§13.1) | M3 — every list in §24 is `ted`'s own until then |
| Project-wide search, outline and project-symbol pickers, mouse | M3 |
| Rendered pane splits | M4 |
| Edit predictions | not deferred so much as absent: §9 does not wire `edit_prediction` into the bootstrap at all |

**Upstream changes.** With completions deferred, **M2 requires no change to any
crate but `ted`**. What remains is one dependency flag and three proposals:

| Crate | Change | Status |
|---|---|---|
| `ratatui` feature | enable `underline-color` in the workspace dependency | done; severity-coloured underlines (§24.8) reach the terminal through it |
| `editor` | a public reader for the completions menu | deferred with §24.9 |
| `vim` | `pub fn status_label(editor, cx)`, in the shape of `vim::mode` | deferred with the status line (§24.5) |
| `vim` | `:b <name>` / `:b <n>`, absent from the command table | proposal; expressible as `ActivateItem`, so §13.4 puts it upstream rather than in `ted`'s host table (§24.6) |
| `vim` | `row:column` in the `:` range parser | proposal, so `:42:8` means the same in both (§24.5) |

**Acceptance** (this expands the M2 line in §21): a real editing session on this
repository without leaving `ted` — open it, find a file by name with the finder,
jump to a line with `:42`, move between five open buffers and see all five in the
tab strip, read a rust-analyzer error first as an underline where the code is
wrong and then as a message on `shift-k`, and reach a command through the `:`
line's ghost text without typing it out. The pty harness (§20.3) asserts the
finder's grid and what `enter` opens, that `esc` leaves the editor as it was, the
switcher's order and the tab strip around a `:q`, the ghost text after two
characters and what `right` does with it, and the bare number prompt `ctrl-g`
opens without vim. The suspend half of M2 keeps the acceptance it already has.

**What the harness cannot assert, and why.** `shift-k` over a *diagnostic* needs
a language server, which the pty harness has no way to depend on: it runs the
real binary against a temporary directory, and a test that installed
rust-analyzer would be measuring the network. So the panel's placement, its
rails and its wrapping are asserted against a `ViewSnapshot` in the renderer's
own tests, its text against the diagnostic and markdown it is built from, and
the pty harness asserts only the half that needs no server — that a position with
nothing to say puts nothing on screen and leaves the editor holding the keyboard.
Diagnostics *arriving* is `editor`'s to get right, and it already does.
