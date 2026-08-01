# `ted` — a terminal UI for Zed

**Status:** M0 and M1 implemented (§21); M2 in progress — the suspend primitive
(§7.1) and `:!` (§13.4) are in, the rest of M2 is not started
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
    frame.rs            # frame loop, dirty tracking, present (§7)
    input.rs            # terminal event -> gpui::Keystroke / PlatformInput (§8)
    suspend.rs          # handing the terminal to a child process (§7.1)
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
- `prompt()` → `None`, so GPUI falls back to its own rendered prompts, which we
  then project as a modal (§13).

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
    pub overlay: Option<OverlayView>,
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
to the dozens of pickers Zed has. `ted` keeps a registry mapping modal `TypeId`
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

`TerminalWindow::prompt()` returns `None`, so GPUI renders its own prompt view;
project it as a centred modal with numbered answers. Workspace notifications
(save errors, LSP failures) render as a transient line above the status line —
important, because a TUI that silently drops an "unable to save" error is worse
than useless.

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
[Yazi](https://yazi-rs.github.io) when it is on `PATH` (detected with the `which`
crate, already a workspace dependency). Yazi exits writing its selection to
`--chooser-file`, one path per line, so the round trip is:

```
:Explore  ->  suspend (§7.1)
          ->  yazi --chooser-file=<tmp> --cwd-file=<tmp> <start-dir>
          ->  resume, read the file, workspace.open_paths(paths, ..)
```

Multiple selected paths open as multiple buffers in one call. The start directory
is the active buffer's parent, falling back to the first worktree root. The
command is a setting, not a hard-coded binary, so `broot`, `nnn` or `ranger` work
by configuration; a missing binary surfaces on the notification line (§13.3)
rather than failing silently or panicking.

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

**Scope.** M2 targets local worktrees. Yazi has since grown a VFS layer with a
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

### 20.2 Likely, as milestones land

| Crate | Change |
|---|---|
| `picker` | Defaulted `PickerDelegate::text_for_match(&self, ix) -> Option<PickerRowText>`, for §13.1's Mirror strategy. |
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
inline, LSP completions as a popup. The suspend primitive (§7.1) with its
reader-thread handshake, plus `:!` and `:Explore` over a local worktree (§13.4).
The primitive and `:!` are implemented; `:Explore` and the navigation half are
not.
*Acceptance:* a real editing session on this repository without leaving `ted`;
suspending to a child and resuming leaves no input stolen, no stale grid and no
altered terminal mode, asserted by the pty harness (§20.3).

**M3 — Fidelity.** Mirror-strategy modal projection with the `PickerDelegate`
hook. Blocks, folds, inlay hints, git diff gutter, multi-cursor, visual block.
Mouse. 256/16-colour fallbacks. *Acceptance:* the modal registry covers command
palette, file finder, outline, project symbols, and the generic fallback is
never hit in normal use.

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
