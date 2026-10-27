# windows-reactor 0.100.0: API reference for noise-filter

Written 2026-10-26 from the crate source and TMR's use of it. Nothing was compiled or run, so the
code below has not been through `cargo check`. Path prefixes:

- `src/` = `C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\windows-reactor-0.100.0\src\`
- `TMR/` = `C:\Users\Administrator\Downloads\TMR\TMR-APP\` (read only)
- `std/` = `C:\Users\Administrator\.rustup\toolchains\nightly-x86_64-pc-windows-msvc\lib\rustlib\src\rust\library\std\src\`

Facts about the package (`Cargo.toml` in the crate root): edition 2024, `rust-version = "1.95"`
(our workspace says 1.91, which is lower; building on nightly 1.101 is unaffected). Dependencies:
windows-core, -collections, -future, -numerics, -reference, -threading and -time, all 0.100.0,
plus rustc-hash 2. The only feature is `test`. Our workspace pins `windows-reactor = "=0.100.0"`
and uses `windows = "0.62.2"`. That gives two windows-core versions, which link side by side
(TMR checked this, `TMR/doc/todo/7-gui.md`). Their `Error`/`HRESULT` types are **different types**.

`use windows_reactor::*;` brings in everything: `App`, `Component`, the contexts, every control,
the traits (`LayoutControl`, `GridChildExt`, `ContentControl`, `ChildrenControl`, `SlotsControl`,
`TooltipExt`, ...), and `DateTime`/`TimeSpan` (`src/lib.rs:17-25`). The `native` module, and so
`bootstrap`, is private.

Don't trust online docs: the readme's "Getting started" link points at windows-rs master, which
has been rewritten since 0.100.0. Its docs describe API that this release doesn't have
(`TMR/doc/todo/7-gui.md`).

---

## 1. Startup

### 1.1 `App` (`src/app.rs:723-899`)

```rust
pub struct App;
impl App {
    pub fn run(root: View) -> windows_core::Result<()>;                                     // app.rs:732
    pub fn run_windows<I>(roots: I) -> windows_core::Result<()> where I: IntoIterator<Item = View>; // :749, Err(E_INVALIDARG) if empty
    pub fn run_component<C: Component>(input: C::Input) -> windows_core::Result<()>;        // :779 = run(View::component::<C>(input))
}
```

`run_with` (`app.rs:783-898`) does the following, in order, on the **calling thread**, and blocks:

1. `is_packaged_process()` (`app.rs:1187`). Unpackaged, it calls `bootstrap_runtime()` (§1.2).
2. `initialize_ui_thread()` (`src/native/winui/mod.rs:4296-4307`) calls
   `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` (result ignored) and then
   `CoInitializeEx(null, COINIT_APARTMENTTHREADED)`. **If the thread is already MTA this returns
   `Err(RPC_E_CHANGED_MODE, "WinUI requires an STA thread")`.** `S_FALSE` (already STA) is fine.
3. `Application::Start`. In `OnLaunched` it merges `XamlControlsResources`, creates the pumps,
   mounts the first window and queues the others (`app.rs:795-879`).
4. It returns when every window has closed (`finalize_closed_window` → `PostQuitMessage`,
   `app.rs:1144-1162`). On return it shuts the pumps down and gives back the first error among
   Start, launch, host fault and scheduler fault (`app.rs:881-897`).

The error is `windows_core::Error` from windows-core **0.100**. Convert it with `.to_string()`
(TMR: `TMR/crates/tmr-gui/src/lib.rs:153,161`) or `.code().0`. Don't `?` it into
`windows::core::Error` 0.62.

Calling `App::run` twice in one process is not covered by the source: `bootstrap` is idempotent
(a static flag, `bootstrap.rs:7,14-22`), but whether WinUI's `Application::Start` can run twice
isn't something Reactor shows.

### 1.2 Bootstrap: the Windows App Runtime check and dialog

`bootstrap_runtime()` (`mod.rs:4245-4258`):
- **Self-contained:** if the exe's `RT_MANIFEST` resource #1 contains the bytes
  `windows-reactor-self-contained` (a raw byte search, `mod.rs:4268-4293`), it requires
  `Microsoft.WindowsAppRuntime.dll` beside the exe (`mod.rs:4260-4265`), else
  `Err(0x8007007E, "self-contained Windows App Runtime files are missing")`.
- **Otherwise** `bootstrap::bootstrap()` (`native/winui/bootstrap.rs:14-89`) runs. It calls
  `TryCreatePackageDependency(null, "Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe", min version,
  arch | Neutral, PROCESS lifetime)` and then `AddPackageDependency`. The minimum version is
  **2.4.0.0** (`WINDOWSAPPSDK_RUNTIME_VERSION_UINT64 = 562967133290496`, `bindings.rs:29211-29215`).
- **If the runtime is missing** (`STATEREPOSITORY_E_DEPENDENCY_NOT_RESOLVED`) it shows a modal
  `MessageBoxW` (MB_YESNO | MB_ICONERROR), captioned with the exe's file name: "You must install
  Windows App Runtime (2.4.0.0, x64) to run this application. Do you want to download it now?".
  Yes runs `ShellExecuteW("open", "https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads")`.
  `App::run` then returns `Err(hr, "Microsoft.WindowsAppRuntime.2 framework package is not
  installed.")` (`bootstrap.rs:43-49, 77-83, 132-162`). There's no public way to check first
  or to skip the dialog.
- `TryCreatePackageDependency` and `AddPackageDependency` are imported from
  `api-ms-win-appmodel-runtime-l1-1-5.dll` through `windows_core::link!`, which is raw-dylib
  (`bindings.rs:1,11`; `windows-link-0.100.0/src/lib.rs`). That DLL only exists from build 22000,
  which is why TMR delay-loads it (§2). Reactor's other direct imports are kernel32, user32, ole32
  and shell32.

### 1.3 Window title, size, icon, backdrop, theme

None of these is a builder on `App`. A component declares them in `view` through `ViewContext`
(`src/core/component.rs:905-926`):

```rust
fn on_color_scheme(&mut self, callback: impl IntoPayloadCallback<ColorScheme>);   // :907
fn on_window_size(&mut self, callback: impl IntoPayloadCallback<WindowSize>);     // :913
fn window_title(&mut self, title: impl Into<String>);                             // :919
fn window_visuals(&mut self, visuals: WindowVisuals);                             // :924
```

Each may be declared **at most once per publication and by one component per window**. A second
declaration is a `ComponentDeclarationError`/`PumpError::Duplicate*` (`component.rs:316-329`,
`core/pump/mod.rs:25-58`, `core/pump/turn.rs:258-268`), which faults the window. Declare them in
the root component on every `view`, as TMR does (`TMR/crates/tmr-gui/src/window.rs:211-223`).

`WindowVisuals` (`src/element.rs:1735-1795`, `Copy`, `Default`):

```rust
WindowVisuals::new()
    .backdrop(WindowBackdrop::Mica)          // None (default) | Mica | MicaAlt | Acrylic   element.rs:1681
    .theme(WindowTheme::System)              // System (default) | Light | Dark              element.rs:1580
    .client_size(560.0, 600.0)               // DIPs; panics unless finite and > 0           element.rs:1760
    .constraints(WindowConstraints {         // pub Option<f64> min/max width/height; panics if <= 0,
        min_width: Some(440.0), min_height: Some(480.0), ..Default::default() }) // non-finite or min > max  :1699, :1785
    .icon(path)                              // path: &'static str to an .ico file; panics if empty  :1774
```

How they're applied (`apply_window_visuals`, `mod.rs:752-900`; diff in `window_visual_changes`,
`mod.rs:506-517`). Each field is re-applied only when its declared value **changes**:
- The backdrop goes on through `SetSystemBackdrop` (MicaBackdrop Base or BaseAlt, or
  DesktopAcrylicBackdrop). The theme is applied as the title bar's `PreferredTheme` plus the host
  Grid's `RequestedTheme`.
- The icon is `AppWindow.SetIcon(path)`. It takes a file path, so TMR writes its embedded .ico to
  `%TEMP%` once and keeps the path in a `OnceLock<Option<String>>` to get a `&'static str`
  (`TMR/crates/tmr-gui/src/window.rs:31-41`).
- `client_size` is `AppWindow.ResizeClient` in DPI-scaled pixels. The doc says "initial", but the
  code **resizes again whenever the declared size changes**. Keep it constant, and don't feed
  `on_window_size` back into it.
- Constraints become the presenter's preferred min and max sizes, with the non-client size added.

`on_color_scheme` posts the current scheme once at subscription, then on each
`ActualThemeChanged` (`mod.rs:1178-1213`). `on_window_size` reports the host Grid's
`SizeChanged` as `WindowSize { width, height }` in DIPs (`mod.rs:1150-1176`). TMR uses both to
pick its palette and to size its chart (`window.rs:206-207, 222-223`). Only 8 theme brushes
exist (`ThemeBrush`, `element.rs:161-170`), so any other colour is a `Color` that you pick per
scheme.

The window is created and activated (shown) on mount (`mod.rs:1322-1349`; `core/pump/mod.rs:181`).
There's no hidden start.

---

## 2. GUI and CLI in one exe (how TMR does it)

Sources: `TMR/crates/tmr-app/build.rs`, `tmr.manifest`, `src/main.rs`, `src/launch.rs`;
`TMR/crates/tmr-gui/src/lib.rs:11-17`; `TMR/doc/todo/7-gui.md`.

- **Console subsystem.** There's no `#![windows_subsystem = "windows"]` (`main.rs` has none), so
  a shell's console is inherited and the CLI prints normally.
- **Manifest** (`tmr.manifest`, embedded only with the `gui` feature):
  `<consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>`
  under `asmv3:application/windowsSettings`. From build 26100, a launch with no console to
  inherit (a double-click) gets **no console**. Builds 22000-26099 ignore it and open one, which
  `GetConsoleProcessList` counts as 1, and TMR `FreeConsole`s it before the window opens: a brief
  flash (`launch.rs:6-10, 194-204`). The manifest holds nothing else. Reactor scans this resource
  for its self-contained marker (§1.2), so never put the literal marker text into it, comments
  included.
- **Link args** (`build.rs:28-40`, `cargo::rustc-link-arg-bins`, gui feature only):
  ```
  /DELAYLOAD:api-ms-win-appmodel-runtime-l1-1-5.dll
  delayimp.lib
  /MANIFEST:EMBED
  /MANIFESTINPUT:<OUT_DIR>/tmr.manifest      (forward slashes; copied into OUT_DIR first)
  ```
  The delay-load lets the exe (the CLI) start below build 22000, where the DLL doesn't exist; the
  DLL loads only when the window bootstraps. `7-gui.md` says it hasn't been tested on 20348 yet.
  The owner has made us Windows 11 only (`doc/todo/15-gui-tray.md`), so for us it's optional. It
  only matters for running CLI subcommands on older Windows. The icon is a hand-built `.res`
  passed as a link arg (`build.rs:22-26, 43-88`).
- **Choosing the frontend** (`launch.rs:122-149`, `decide` is a pure function, tested):
  1. `--gui` or `--cli` (the last one given) decides, and is removed from args (`take_mode`, :28-42).
  2. Any other argument means the CLI.
  3. Stdout that is a file or pipe (`GetFileType` DISK or PIPE) means the CLI (WinRM, `> out.txt`).
  4. No console window, or a console used by this process alone, means the GUI (a double-click).
     A shared console means the CLI (a shell).
  5. The GUI only opens when built with the feature and `RtlGetVersion` reports a build of at
     least 22000 (`FIRST_GUI_BUILD`). Otherwise `--gui` is refused with exit 2, and a double-click
     falls back to the CLI with a note.
  6. `free_console` when the GUI runs in a console Windows opened just for it; `alloc_console`
     when the CLI was launched with no console (`launch.rs:142-148`).
- `main` (`main.rs:154-235`) installs the panic hook and the VEH first, then decides, gates the
  CPU, and runs `tmr_gui::main(args, include_bytes!("../tmr.ico"))` or `tmr_cli::main(args)`. In GUI
  mode fatal text also goes to a message box (`fatal_box.rs`), because there's no console to read.
  When the window closes, TMR stops the session and gives it 30 s to finish
  (`TMR/crates/tmr-gui/src/lib.rs:124-137, 151-167`).

---

## 3. The component model

### 3.1 `Component` (`src/core/component.rs:978-997`)

```rust
pub trait Component: Sized + 'static {
    type Input: Clone + PartialEq + 'static;   // parent-supplied; compared with == on every parent render
    type Message: 'static;                     // must also be Send to use spawn_background
    fn create(input: &Self::Input, context: &ComponentContext<Self>) -> Self;
    fn input_changed(&mut self, _input: &Self::Input, _context: &ComponentContext<Self>) {}
    fn update(&mut self, _message: Self::Message, _context: &ComponentContext<Self>) {}
    fn view(&self, input: &Self::Input, context: &mut ViewContext<Self>) -> View;
}
```

- `update` gets **no input**, so keep what it needs (an `Arc` handle, say) in `self`. Fill it in
  `create` and refresh it in `input_changed`.
- `view` takes `&self`. It can't change state or spawn work. Spawn from `create`, `update` or
  `input_changed`.
- A panic in `view` is caught and re-raised (`component.rs:1538-1546, 1419`). Our release
  profile has `panic = "abort"`, so it kills the process.

### 3.2 `ComponentContext<C>` (`component.rs:813-862`), for `create`, `input_changed` and `update`

```rust
#[must_use] pub fn open_window(&self, root: View) -> bool;          // :825, new window with its own Pump
pub fn sender(&self) -> LocalSender<C::Message>;                    // :830
pub fn spawn_background<F>(&self, work: F) -> ComponentTask         // :838
    where C::Message: Send, F: FnOnce(CancellationToken) -> C::Message + Send + 'static;
pub fn spawn_background_with_rejection<F>(&self, work: F, rejected: C::Message) -> ComponentTask // :850
    where C::Message: Send, F: FnOnce(CancellationToken) -> C::Message + Send + 'static;
pub fn window(&self) -> WindowRef;                                  // :859
```

`WindowRef::request_close(&self) -> bool` (`#[must_use]`, `src/reference.rs:156`) closes the
window after the current turn publishes. It's accepted only during create, input_changed or update.

### 3.3 `ViewContext<C>` (`component.rs:890-976`), for `view`

Besides the window declarations in §1.3:

```rust
pub fn sender(&self) -> LocalSender<C::Message>;                                 // :929
pub fn callback<T>(&self, map: impl Fn(T) -> C::Message + 'static) -> Callback<T>; // :934
pub fn forward(&self) -> Callback<C::Message>;                                   // :939 (identity map)
pub fn message(&self, message: C::Message) -> Callback<()> where C::Message: Clone; // :944
pub fn use_context<T: Clone + 'static>(&mut self, context: &Context<T>) -> T;   // :952
pub fn use_effect<D: PartialEq + 'static>(&mut self, key: impl Into<EffectKey>, dependency: D,
    setup: impl FnOnce() -> Option<Box<dyn FnOnce()>> + 'static);               // :966
```

Typical wiring: `cx.callback(Msg::Mic)` for a payload event, and `cx.callback(|()| Msg::Measure)`
or `cx.message(Msg::Measure)` (needs `Msg: Clone`) for a click. A **captureless** mapper is
zero-sized, so the callback keeps its identity across renders (`component.rs:789-802`). A
capturing closure gets a new identity each render. That's harmless, because event subscription
only changes when a handler appears or disappears (`core/pump/planner/element.rs:452-482`), but
captureless is cheaper.

### 3.4 `LocalSender<M>` and `Callback<T>`

```rust
impl<M: 'static> LocalSender<M> {                                    // component.rs:756-811
    pub fn send(&self, message: M) -> bool;   // queued, never inline; false if retired, closed or queue full (4096)
    pub fn callback<T, F>(&self, map: F) -> Callback<T> where F: Fn(T) -> M + 'static;
    pub fn message(&self, message: M) -> Callback<()> where M: Clone;
}
impl<T> Callback<T> {                                                // element.rs:1994-2036
    pub fn new(callback: impl Fn(T) + 'static) -> Self;
    #[must_use] pub fn call(&self, value: T) -> bool;
}
```

`LocalSender` holds `Rc<RefCell<..>>` (`component.rs:740-744`) and `Callback` holds `Rc<dyn Fn>`
(`element.rs:1994-1997`). **Neither is `Send`, so neither can be used from an audio or engine
thread.** The same goes for `View` (it contains `Rc`, `element.rs:434-481`), `ComponentContext`,
`ViewContext`, `WindowRef` and `ElementRef`. Nothing in the crate adds `unsafe impl Send/Sync`.
`ComponentTask` holds only `Arc`s and plain tokens (`component.rs:531-535`). It's probably
`Send + Sync` through the auto traits, but no test checks that.

### 3.5 Background work (`component.rs:568-727`)

- `spawn_background` runs `work` on the **Windows default thread pool** through
  `windows_threading::submit` → `TrySubmitThreadpoolCallback` (`component.rs:617`,
  `windows-threading-0.100.0/src/lib.rs:15-26`). The result is boxed **on the pool thread**
  (`component.rs:659-665`), queued, and delivered to `update` on the UI thread. A
  `DispatcherQueue.TryEnqueue` from the pool thread wakes it (`mod.rs:4127-4153`).
- Limits (`component.rs:19-21`): **64** tasks running at once per window
  (`BACKGROUND_TASK_CAPACITY`), and 4096 queued background results. 4096 queued local messages.
  Past the task limit a spawn is **rejected synchronously** (`spawn_inner`, :597-600).
- `ComponentTask` (`component.rs:526-556`) has `cancel()`, `is_cancelled()` and
  `status() -> ComponentTaskStatus { Running, Queued, Delivered, Cancelled, Rejected }` (:441).
  Dropping the handle does **not** cancel. Removing the component, shutting the pump down (the
  window closing), or `cancel()` does. Cancellation is cooperative: `work` sees it through
  `CancellationToken::is_cancelled()` (:433), and a cancelled result is just dropped.
- `rejected` is delivered only when bounded delivery rejects the result (task limit, full queue,
  failed wake). Cancellation and retirement don't deliver it (`component.rs:846-849`).
- A panic in `work` is fatal (`component.rs:836-837`).
- COM: pool threads are shared and reused. Do WASAPI work on engine-owned threads that set up
  their own MTA, and have `work` only wait for the result (our rule; Reactor doesn't say).

### 3.6 How a turn renders (`core/pump/turn.rs:51-83`, `app.rs:429-433`)

One dispatch runs native events (budget 64, `native_work.rs:3`), then up to **64 component
messages**, then composes each dirty component **once**. Only the components that got messages are
re-run. A child component whose `Input` is unchanged (`==`) is **not** re-run when its parent
re-renders (`core/pump/planner/view.rs:135-164`). The diff sets only properties whose value
changed (`planner/element.rs:35-71`).

**Controlled controls.** When the user changes a control, Reactor records the native value, marks
the owning component dirty, and calls your callback (`core/pump/native_work.rs:326-381`). If your
`update` doesn't store the new value, the next `view` still declares the old one, and the diff
**writes it back**: the control snaps back. Programmatic sets suppress their own echo event
(`mod.rs:1896-1956`; per-property `feedback` in `generated.rs`, e.g. ComboBox at :16991-17046).

### 3.7 TMR's re-armed waiter (bringing data in from other threads)

Reactor 0.100.0 has no timer and no `Send` sender. TMR's pattern (`TMR/crates/tmr-gui/src/window.rs:93-115, 149-172`):
1. `start` spawns one waiter with `context.spawn_background(move |_| { ... })`.
2. The waiter blocks on the pool thread for up to `TICK` (500 ms): `recv_timeout` on an mpsc
   `Receiver`, then `try_iter()` to drain. It also reads shared progress (`Arc<ProgressTracker>`)
   and the log buffer, and returns **one** `Message::Feed(Box<Batch>)`. A wait that brings nothing
   still returns, and that return is the window's clock.
3. The `Receiver` travels **inside** the message (`Batch.feeds`), and `update` re-arms by calling
   `listen(feeds, ...)` again. So there's exactly one waiter, and the `Receiver` is never shared.
4. It stops re-arming when the producer has gone (`Disconnected`) or the session has ended.

What follows from the source:
- A rejected or cancelled result **ends the chain silently**, since no message arrives to re-arm.
  One waiter plus an occasional job stays far below the limits.
- Don't re-arm from the `rejected` message of `spawn_background_with_rejection`. A full task
  limiter rejects synchronously, so that would spin through the dispatcher.
- For a fixed rate, use `std::thread::sleep`. On Windows it's a high-resolution waitable timer
  (`std/sys/thread/windows.rs:117-121`), so 50 ms isn't rounded up to the 15.6 ms tick.

---

## 4. Builders

Every builder takes `self` and returns `Self`, until a method that returns `View`: `.content()`,
`.children()`, `.keyed_children()`, `.slot()`, `.slots()`, `.collection_slot()`, `.tooltip()`,
`.menu()`. **Call layout and placement methods (`grid_row`, `margin`, `width`, ...) before those**,
because `View` has none (`element.rs:2484-2507`; TMR `window.rs:305`). `Option`-taking setters
accept `None` to leave the property inherited. Every enum below is `#[non_exhaustive]`, so a
`match` needs `_ =>`. All lengths are DIPs.

### 4.1 Shared traits and value types

| Item | Signature / notes | Source |
|---|---|---|
| `LayoutControl` | `width`, `height`, `min_width`, `max_width`, `min_height`, `max_height`, `opacity`: `(self, impl Into<Option<f64>>) -> Self`, **assert finite and >= 0**. `horizontal_alignment(impl Into<Option<HorizontalAlignment>>)`, `vertical_alignment(..)`, `margin(impl Into<Thickness>)` (asserts finite), `margin_optional`, `exit_transition(ExitTransition)` | element.rs:2217-2404 |
| `GridChildExt` | `grid_row(i32)`, `grid_column(i32)` (assert >= 0), `grid_row_span(i32)`, `grid_column_span(i32)` (assert > 0) | element.rs:2508-2550 |
| `CanvasChildExt` | `canvas_left(f64)`, `canvas_top(f64)` (assert finite) | element.rs:2612-2634 |
| `ContentControl` | `content(self, impl Into<View>) -> View` | element.rs:2414-2418 |
| `ChildrenControl` | `children(self, impl IntoViews) -> View` (tuples up to 16, or `[T; N]`, positional); `keyed_children<T: Into<KeyedView>>(self, impl IntoIterator<Item = T>) -> View` (for `Vec`s; `(key, view)` tuples work) | element.rs:2425-2442, 1328-1350 |
| `SlotsControl` | `slot(self, Self::Slot, impl Into<View>) -> View`, `slots(self, impl IntoIterator<Item = SlotView<Self::Slot>>) -> View`, `collection_slot(self, slot, impl IntoIterator<Item: Into<KeyedView>>) -> View` | element.rs:2446-2482 |
| `TooltipExt` | `tooltip(self, impl Into<String>) -> View`, `tooltip_with(self, Tooltip) -> View` | element.rs:548-561 |
| `MenuExt` | `menu(self, Menu) -> View`; `Menu::new(impl IntoIterator<Item = MenuItem>, impl IntoPayloadCallback<String>)` (gets the **label**) | element.rs:691-719 |
| `View` | `View::empty()`, `View::component::<C>(input)`, `View::fragment(impl IntoViews)`, `View::keyed_fragment(..)`, `View::provide(&Context<T>, T, child)`. `From<&str>`/`From<String>` make a TextBlock. There's no `From<Option<_>>`: use `View::empty()` | element.rs:975-1105 |
| `Key`/`KeyedView` | `Key: From<u64/u32/usize/String/&str>`; `KeyedView::new(key, view)`, `From<(K, V)>` | element.rs:345-389, 1109-1151 |
| `Thickness` | `uniform(f64)`, `xy(h, v)`, `new(l, t, r, b)`, `From<f64>` | element.rs:1376-1435 |
| `CornerRadius` | `uniform(f64)`, `new(tl, tr, br, bl)`, `From<f64>` | element.rs:1445-1500 |
| `Color` | `pub a, r, g, b: u8`; `const fn argb(a, r, g, b)`, `const fn rgb(r, g, b)`, `transparent()` | element.rs:242-264 |
| `Brush` | `enum { Theme(ThemeBrush), Solid(Color) }`, `From<Color>`, `From<ThemeBrush>` | element.rs:268-312 |
| `ThemeBrush` | `Accent, AccentText, PrimaryText, SolidBackground, CardBackground, CardStroke, SystemCritical, SystemCriticalBackground` | element.rs:161-170, 224-237 |
| `FontWeight` | consts `THIN..EXTRA_BLACK` (`NORMAL`, `SEMI_BOLD`, `BOLD`, ...); `const fn new(u16) -> Option<Self>` (1-999) | element.rs:174-208 |
| `GridLength` | `Auto`, `Pixel(f64)`, `Star(f64)`, `const STAR`; must be finite and >= 0 (asserted by `Grid::rows/columns`) | element.rs:1837-1856 |
| `HorizontalAlignment`/`VerticalAlignment` | `Left/Top, Center, Right/Bottom, Stretch` | element.rs:1872-1886 |
| `Orientation` | `Vertical, Horizontal` | generated.rs:115 |

### 4.2 Controls (all in `src/generated.rs`; every one is `LayoutControl` unless stated)

**TextBlock** (struct :399, impl :410): `text(impl Into<String>)` :414, `text_optional`;
`text_wrapping(impl Into<Option<TextWrapping>>)` :425 (`NoWrap|Wrap|WrapWholeWords`, :386);
`font_size(impl Into<Option<f64>>)` :430 (**asserts finite and > 0**);
`font_weight(impl Into<Option<FontWeight>>)` :441; `is_text_selection_enabled(..bool)` :446;
`max_lines(..i32)` :451 (>= 0); `text_trimming(..TextTrimming)` :460
(`None|CharacterEllipsis|WordEllipsis|Clip`); `foreground(impl Into<Brush>)` :465,
`foreground_optional`. **No font family, no text alignment, no background** (the struct fields at
:399-409). TMR right-aligns text with a fixed-width Grid instead (`TMR/crates/tmr-gui/src/chart.rs:78-86`).

**Button** (:490, impl :502): `is_enabled(..bool)` :510; `horizontal_content_alignment`,
`vertical_content_alignment`; `resource_overrides(..ResourceOverrides)` :531;
`style(impl Into<Option<ButtonStyle>>)` :536 (`Default|Accent|Subtle|TextLink`, :75);
`key_accelerators` :541; `on_click(impl IntoUnitCallback)` :546; `element_ref`. `ContentControl`
(:566), so `.content("Start")`. Also `HyperlinkButton` (:576), `RepeatButton` (:647) and
`DropDownButton` (:4145).

**Border** (:701, impl :716): `padding(impl Into<Thickness>)` :720 and
`border_thickness(..)` :743 (both assert finite and >= 0); `corner_radius(impl Into<CornerRadius>)`
:766; `background(impl Into<Brush>)` :789, `background_optional`; `border_brush` :800;
`opacity_transition(..Duration)` :811; `scale(..f64)` :816; `scale_transition`; pointer and drag
events (:842-937). `ContentControl` (:959). This is the only container with padding: wrap a
StackPanel or Grid in it for padding or a background.

**StackPanel** (:1015, impl :1021): `orientation(..Orientation)` :1025, `spacing(..f64)` :1030.
`ChildrenControl` (:1048). It has no padding and no background.

**Grid** (:1125, impl :1136): `row_spacing(..f64)` :1144, `column_spacing` :1149,
`background(impl Into<Brush>)` :1159, `rows(impl IntoIterator<Item = GridLength>)` :1170,
`columns(..)` :1193 (both assert valid lengths), `*_optional`, `key_accelerators`, `element_ref`.
`ChildrenControl` (:1231). Changing `rows`/`columns` **clears and rebuilds** the native
Row/ColumnDefinitions (`native/winui/grid.rs:124-165`). Also `Canvas` (:5174, children with
`canvas_left/top`), `RelativePanel` (:5140) and `VariableSizedWrapGrid` (:1061).

**ComboBox** (:3537, impl :3547):
```rust
pub fn items_source<I, S>(self, value: I) -> Self where I: IntoIterator<Item = S>, S: Into<String>; // :3555 (Rc<Vec<String>>)
pub fn items_source_optional<I, S>(self, value: Option<I>) -> Self;                               // :3565
pub fn selected_index(self, value: impl Into<Option<usize>>) -> Self;                            // :3575 (None = -1)
pub fn placeholder_text(self, value: impl Into<String>) -> Self;                                  // :3580
pub fn is_editable(self, value: impl Into<Option<bool>>) -> Self;                                 // :3591
pub fn is_enabled(self, value: impl Into<Option<bool>>) -> Self;                                  // :3596
pub fn on_selection_changed(self, callback: impl IntoPayloadCallback<Option<usize>>) -> Self;    // :3601
// header: .slot(ComboBoxSlot::Header, "Microphone")   (ComboBoxSlot::Header :3627; there is no .header())
```
Items are strings only. Each render builds a fresh `Vec<String>`, which is compared by value and
set again only when it differs. Setting items calls `ItemsControl.SetItemsSource` with a new
`IVector<IInspectable>` (`native/winui/generated.rs:2313-2328`). `ItemsSource` has no feedback
handling (`generated.rs:16992-17001`). If WinUI resets the selection when the source is replaced
(WinUI behaviour, which this source doesn't show), that `SelectionChanged(None)` reaches your
callback. To refresh a device list safely, key the ComboBox by a list revision (`keyed_children`),
so a new native control is created with items and then index. The index goes straight to
`ISelector::SetSelectedIndex` (`native/winui/generated.rs:2329-2339`). Keep it within the item
count, because a failed native set aborts (pitfall 2).

**CheckBox** (:2241, impl :2249): `is_checked(..bool)` :2257, `is_enabled` :2262,
`on_is_checked_changed(impl IntoPayloadCallback<bool>)` :2267. `ContentControl` (:2287), so
`.content("Echo cancellation")`. There's no three-state option.

**ToggleSwitch** (:2179, impl :2186): `is_on(..bool)` :2194, `is_enabled` :2199,
`on_toggled(impl IntoPayloadCallback<bool>)` :2204. Slots `ToggleSwitchSlot::{Header, OnContent, OffContent}` (:2227).

**Slider** (:1652, impl :1663): `minimum(..f64)` :1671, `maximum` :1676, `value(..f64)` :1681,
`is_enabled` :1686, `step_frequency(..f64)` :1691 (**asserts finite and > 0**; there's no `step`),
`orientation` :1702, `on_value_changed(impl IntoPayloadCallback<f64>)` :1707. Header:
`.slot(SliderSlot::Header, ..)` (:1730). It has no tick marks and no SmallChange/LargeChange.

**NumberBox** (:1580, impl :1589): `minimum` :1597, `maximum` :1602,
`value(impl Into<Option<f64>>)` :1607 (NaN becomes `None`, and `None` is NaN natively,
`mod.rs:3499-3501`), `is_enabled` :1612,
`on_value_changed(impl IntoPayloadCallback<Option<f64>>)` :1617 (`None` when cleared). Header:
`.slot(NumberBoxSlot::Header, ..)` (:1640). It has no step, spin-button or format settings.

**TextBox** (:1244): `text`, `placeholder_text` :1276, `is_enabled`, `accepts_return`,
`text_wrapping`, colours, `on_text_changed(impl IntoPayloadCallback<String>)` :1347,
`TextBoxSlot::Header`.

**ProgressBar** (:2116, impl :2126): `minimum` :2130, `maximum` :2135, `value(..f64)` :2140,
`is_indeterminate` :2145, `show_error` :2150, `show_paused` :2155, `is_enabled` :2160.
`LayoutControl` (:2177), so `.height()` **does exist**, even though `TMR/doc/todo/7-gui.md` and
`doc/research-2026-10.md` say it has "no height". Whether WinUI's template draws a thicker bar
isn't in this source, and there's no corner radius. TMR builds its own bar from Borders and Star
columns (`TMR/crates/tmr-gui/src/window.rs:591-626`). Also `ProgressRing` (:2858).

**Meter bars from Borders.** Use a fixed-width track `Border` holding a left-aligned fill
`Border` whose `width` is `fraction * track`. Each tick then sets a single `Width`. TMR's
Star-column version (`Grid.columns([Star(f), Star(1 - f)])`) rebuilds two ColumnDefinitions on
every change (`grid.rs:148-162`), so keep that for slow progress, not 20 Hz meters. `Rectangle`
(:2964: `fill`, `stroke`, `radius_x`, `radius_y`) works too.

**InfoBar** (:2568, impl :2577): `title` :2581, `message` :2592,
`severity(impl Into<Option<InfoBarSeverity>>)` :2603 (`Informational|Success|Warning|Error`, :83),
`is_open(..bool)` :2608, `is_closable` :2613, `on_closed(impl IntoUnitCallback)` :2618. It has no
content, no action button and no slots. `IsOpen` has **no feedback** (`generated.rs:16389-16397`):
when the user closes it, handle `on_closed` by setting your `open = false`, so the next
`is_open(true)` is a change Reactor applies.

**ScrollViewer** (:2681, impl :2687): `horizontal_scroll_bar_visibility(..ScrollBarVisibility)`
:2691, `vertical_scroll_bar_visibility` :2699 (`Disabled|Auto|Hidden|Visible`). `ContentControl`
(:2721). It has no scroll offset and no ScrollIntoView (TMR lists newest-first for this reason,
`window.rs:461-462`). `ScrollView` (:2730) also exists.

**Expander** (:3490, impl :3495): `is_expanded(..bool)` :3499,
`on_is_expanded_changed(impl IntoPayloadCallback<bool>)` :3504. It isn't a ContentControl. Use
`.slots([SlotView::new(ExpanderSlot::Header, "Advanced"), SlotView::new(ExpanderSlot::Content, body)])` (:3524).

**Tabs.**
- `TabView` (:3888): `selected_index(..usize)`, `can_reorder_tabs`, `is_add_tab_button_visible`,
  `on_selection_changed(Option<usize>)`, `on_close_requested(String)`, `on_add_tab_button_click`,
  `on_reordered(Vec<String>)`, and the collection slot `TabViewSlot::TabItems`.
  - `TabViewItem` (:3977): `header(impl Into<String>)` :3988, `is_closable`, `tag`; it's a
    `ContentControl`.
  - Usage: `.collection_slot(TabViewSlot::TabItems, [("main", TabViewItem::new().header("Main").is_closable(false).content(a)), ("lab", ...)])`.
- `SelectorBar` (:3792): `on_selected_text_changed(Option<String>)`, with items
  `SelectorBarItem::new().text(..).is_selected(..)` in `SelectorBarSlot::Items`.
- `Pivot` (:3639) with `PivotItem::header` (:3709), and `NavigationView` (:1842), also exist.

**RadioButtons** (:2421): `items_source(..)` :2432, `selected_index` :2452, `max_columns` :2457,
`on_selection_changed(Option<usize>)` :2462, Header slot.

**ContentDialog** (:4712, impl :4723). It is **not** a `LayoutControl` (no `grid_row` or
`margin`). Put it anywhere among the children, as TMR does (`window.rs:236-244, 572-581`).
```rust
pub fn is_open(self, value: bool) -> Self;                                       // :4727
pub fn title(self, value: impl Into<String>) -> Self;                            // :4731
pub fn primary_button_text(..) / secondary_button_text(..) / close_button_text(..); // :4742 :4753 :4764
pub fn is_primary_button_enabled(..bool) / is_secondary_button_enabled(..bool);  // :4775 :4780
pub fn on_closed(self, callback: impl IntoPayloadCallback<ContentDialogResult>) -> Self; // :4785
// ContentControl (:4805): .content(view), or From<ContentDialog> for View with no content (:6176)
// ContentDialogResult { None (default), Primary, Secondary } (:67); Esc and the close button give None
```
Lifecycle (`src/native/winui/content_dialog.rs`):
- Only a **change** of `is_open` acts (:80-82). `true` calls `ShowAsync`.
- When the user closes it, `on_closed(result)` fires but Reactor still holds `desired_open =
  true`. Set your state to false in `update`, or the dialog can't be reopened.
- Setting it false while it shows hides it and **suppresses** `on_closed` (:102-108, 341-343).
- Only one dialog shows per XamlRoot. Others queue (:97-101, 346-388).

---

## 5. What Reactor 0.100.0 lacks (checked against the source)

- **Timer:** there's no public timer API. `DispatcherQueueTimer` is used internally only for exit
  transitions (`mod.rs:3912-3918`) and in the `test` feature (`app.rs:1165-1185`). Use the
  re-armed waiter (§3.7).
- **Close-cancel:** it only subscribes `Window.Closed` (`mod.rs:1333-1338`). There's no
  `AppWindow.Closing` and no veto. The only programmatic close is `WindowRef::request_close()`.
- **HWND:** it's fetched internally through `IWindowNative::WindowHandle` for DPI maths
  (`mod.rs:764-778`) and never exposed. A workaround (ours, untested): `EnumThreadWindows(GetCurrentThreadId())`
  from `update` on the UI thread, after the first publish.
- **Minimise and restore events:** none. `on_window_size` is the host Grid's `SizeChanged`, and
  whether a minimise fires it isn't in this source.
- **Start hidden or tray:** the window is always activated on mount (`mod.rs:1344-1349`). There's
  no tray API.
- **Text:** no font family, no text alignment (§4.2 TextBlock).
- **Scrolling:** no ScrollIntoView and no scroll offset.
- **Allocation-free view building:** every `view` allocates (`Rc`, `Vec`, `String` in each builder).

`features = ["test"]` exports `test::RecordingRuntime` and `Pump` (`src/test/mod.rs`) for driving
components without WinUI. It's test-oriented and we haven't tried it.

---

## 6. Pitfalls

1. **Nothing in Reactor crosses threads.** `View`, `Callback`, `LocalSender`, both contexts,
   `WindowRef` and `ElementRef` all hold `Rc` (§3.4). The only way in from another thread is
   the return value of `spawn_background`, so the **whole `Message` enum must be `Send`**
   (`component.rs:840`). Audio threads publish to atomics or a ring, and the waiter reads them.
2. **Panics and aborts take the process down, audio included if it's in-process.**
   - Builders `assert!` on NaN, infinite or negative widths, heights, opacity, margins and padding,
     on `font_size <= 0`, `step_frequency <= 0`, bad `GridLength`s, and bad `client_size` or
     constraints.
   - A failed native property set is `PumpError::NativeApplyFailed`, which calls
     `std::process::abort()` (`app.rs:1197-1205`).
   - Failing to finalise a closed window also aborts (`app.rs:1158-1161`).
   - Clamp every computed value (meter widths) and keep indices in range. This is one more reason
     for the engine and settings-process split in `doc/todo/15-gui-tray.md`.
3. **COM apartment.** `App::run` turns the calling thread into an STA and fails with
   `RPC_E_CHANGED_MODE` if it's already MTA (`mod.rs:4296-4307`). Never call
   `CoInitializeEx(MTA)` or wasapi's `initialize_mta()` on the thread that will run the window. Keep
   WASAPI on engine-owned MTA threads, not on the UI thread or the thread pool (§3.5).
4. **Allocation and cross-thread frees.**
   - The UI thread allocates on every render.
   - Each background result is `Box::new`'d on a **pool thread** and freed on the UI thread
     (`component.rs:659-665, 1378`). The waker also allocates a `DispatcherQueueHandler` on the
     pool thread (`mod.rs:4138`).
   - If our TLSF router picks a heap by **thread** at `dealloc`, these frees go to the wrong heap.
     It has to decide by **address range**.
   - Keep Reactor off the audio thread entirely.
5. **Controlled values.** `update` must store each control's new value, or the control snaps back
   (§3.6). `ContentDialog` and `InfoBar` need their open flag cleared on close (§4.2).
6. **Re-render cost.**
   - Each message re-runs the component that received it (one compose per batch of up to 64).
   - Put the 20 Hz meter in its **own child component**, with an `Input` that compares by
     `Arc::ptr_eq`. Then only the meter's small view is rebuilt and diffed (`planner/view.rs:147-163`).
   - The root's `ComboBox::items_source` re-allocates its `Vec<String>` on every root render.
7. **The waiter chain fails silently.** If a task is rejected or cancelled, no message arrives to
   re-arm the waiter. Don't re-arm from the rejection message, because that spins (§3.7).
8. **Lifecycle.**
   - `App::run` returns while a pool thread may still be sleeping in a waiter that holds an
     `Arc<Engine>`. The last `Arc` may then drop on that pool thread.
   - So stop the engine explicitly after `App::run` returns, and don't rely on its `Drop`.
   - There's no close-cancel, which is why TMR stops its session after the window has closed
     (`TMR/crates/tmr-gui/src/lib.rs:124-137`).
9. **Placement order.** Call `grid_row`, `margin` and `width` before `.content()`, `.children()`,
   `.slot()` or `.tooltip()`. A child component can't be placed in a Grid directly; wrap it in a
   `Border` (`element.rs:2484-2507`).
10. **Two windows-core versions.** Reactor's `windows_core::Error` 0.100 isn't the `windows` 0.62
    error type. Convert at the boundary.
11. **Missing runtime.** On a machine without the Windows App Runtime 2.4+, `App::run` shows
    Reactor's own Yes/No download box and then returns `Err`. If you also show your own error box,
    the user sees two.

---

## 7. Skeleton: a settings window with a 20 Hz meter and a background job

Our code, and a proposal. `Engine` stands in for nf-engine's handle. It must be `Send + Sync`,
because it moves into `spawn_background` closures.

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use windows_reactor::*;

// ---- shared with the engine (lives in nf-engine) ------------------------------------------

/// Peak since the GUI's last read, as f32 bits. Audio thread: `publish` (no alloc, no lock).
#[derive(Default)]
pub struct Meters { pub mic: AtomicU32, pub out: AtomicU32 }

impl Meters {
    #[inline]
    pub fn publish(slot: &AtomicU32, peak: f32) {
        // Positive f32s order like their bits; NaN, negatives and -0.0 become 0.
        slot.fetch_max(if peak > 0.0 { peak.to_bits() } else { 0 }, Relaxed);
    }
    pub fn take(slot: &AtomicU32) -> f32 { f32::from_bits(slot.swap(0, Relaxed)) }
}

pub struct Engine { pub meters: Meters, pub mics: Vec<String>, pub cables: Vec<String> }
impl Engine {
    /// Blocking. Does its WASAPI work on the engine's own MTA threads and waits for them.
    pub fn measure_latency(&self) -> Result<f64, String> { unimplemented!() }
    /// Hands the settings to the audio thread without blocking it.
    pub fn apply(&self, _settings: &Settings) {}
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings { pub mic: Option<usize>, pub cable: Option<usize>, pub aec: bool, pub denoise: bool, pub floor_db: f64 }

/// Inputs must be Clone + PartialEq (component.rs:985): compare the handle by pointer.
#[derive(Clone)]
pub struct Shared(pub Arc<Engine>);
impl PartialEq for Shared { fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) } }

/// Blocks until the window closes. Call it on a thread with no COM apartment (or an STA).
pub fn run(engine: Arc<Engine>, settings: Settings) -> Result<(), String> {
    App::run_component::<SettingsWindow>((Shared(engine), settings)).map_err(|e| e.to_string())
}

// ---- the window ---------------------------------------------------------------------------

pub enum Msg {                       // Send: spawn_background needs it (component.rs:840)
    Mic(Option<usize>),
    Cable(Option<usize>),
    Aec(bool),
    Denoise(bool),
    Floor(f64),
    Measure,
    Measured(Result<f64, String>),
    CloseError,
}

pub struct SettingsWindow {
    engine: Arc<Engine>,             // update() gets no Input
    s: Settings,
    measuring: bool,
    latency_ms: Option<f64>,
    error: Option<String>,
}

impl Component for SettingsWindow {
    type Input = (Shared, Settings);
    type Message = Msg;

    fn create(input: &(Shared, Settings), _cx: &ComponentContext<Self>) -> Self {
        Self { engine: Arc::clone(&input.0.0), s: input.1.clone(), measuring: false, latency_ms: None, error: None }
    }

    fn update(&mut self, msg: Msg, cx: &ComponentContext<Self>) {
        // Store every control's value, or Reactor writes the old one back (native_work.rs:336-365)
        let changed = match msg {
            Msg::Mic(i) => { self.s.mic = i; true }
            Msg::Cable(i) => { self.s.cable = i; true }
            Msg::Aec(on) => { self.s.aec = on; true }
            Msg::Denoise(on) => { self.s.denoise = on; true }
            Msg::Floor(db) => { self.s.floor_db = db; true }
            Msg::Measure => {
                if !self.measuring {
                    self.measuring = true;
                    self.error = None;
                    let engine = Arc::clone(&self.engine);
                    // One-shot job: the rejection message just reports failure (it doesn't re-arm anything)
                    cx.spawn_background_with_rejection(
                        move |_cancel| Msg::Measured(engine.measure_latency()),
                        Msg::Measured(Err("the measurement couldn't start".to_string())),
                    );
                }
                false
            }
            Msg::Measured(result) => {
                self.measuring = false;
                match result { Ok(ms) => self.latency_ms = Some(ms), Err(e) => self.error = Some(e) }
                false
            }
            Msg::CloseError => { self.error = None; false }   // InfoBar.IsOpen has no feedback
        };
        if changed { self.engine.apply(&self.s); }
    }

    fn view(&self, _input: &(Shared, Settings), cx: &mut ViewContext<Self>) -> View {
        cx.window_title("Noise Filter");
        cx.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(WindowTheme::System)
                .client_size(560.0, 600.0)  // constant: a changed value resizes the window again
                .constraints(WindowConstraints { min_width: Some(440.0), min_height: Some(480.0), ..Default::default() }),
        );
        let e = &self.engine;
        let latency = match (self.measuring, self.latency_ms) {
            (true, _) => "Measuring...".to_string(),
            (false, Some(ms)) => format!("End to end: {ms:.1} ms"),
            (false, None) => "Not measured yet".to_string(),
        };
        StackPanel::new().margin(Thickness::xy(24.0, 20.0)).spacing(14.0).children((
            ComboBox::new()
                .items_source(e.mics.iter().map(String::as_str))
                .selected_index(self.s.mic)
                .placeholder_text("Choose a microphone")
                .on_selection_changed(cx.callback(Msg::Mic))
                .min_width(360.0)                                   // layout before .slot()
                .slot(ComboBoxSlot::Header, "Microphone"),
            ComboBox::new()
                .items_source(e.cables.iter().map(String::as_str))
                .selected_index(self.s.cable)
                .placeholder_text("Choose the virtual cable")
                .on_selection_changed(cx.callback(Msg::Cable))
                .min_width(360.0)
                .slot(ComboBoxSlot::Header, "Output (virtual cable)"),
            CheckBox::new()
                .is_checked(self.s.aec)
                .on_is_checked_changed(cx.callback(Msg::Aec))
                .content("Echo cancellation (I use speakers)"),
            CheckBox::new()
                .is_checked(self.s.denoise)
                .on_is_checked_changed(cx.callback(Msg::Denoise))
                .content("Noise filter (RNNoise)"),
            Slider::new()
                .minimum(-60.0)
                .maximum(0.0)
                .step_frequency(1.0)
                .value(self.s.floor_db)
                .on_value_changed(cx.callback(Msg::Floor))
                .slot(SliderSlot::Header, format!("Attenuation floor: {:.0} dB", self.s.floor_db)),
            // Child component: its 20 Hz ticks re-render only the meter (planner/view.rs:147-163)
            View::component::<Meter>(Shared(Arc::clone(e))),
            StackPanel::new().orientation(Orientation::Horizontal).spacing(12.0).children((
                Button::new()
                    .style(ButtonStyle::Accent)
                    .is_enabled(!self.measuring)
                    .on_click(cx.callback(|()| Msg::Measure))
                    .content("Measure latency"),
                TextBlock::new().text(latency).vertical_alignment(VerticalAlignment::Center),
            )),
            InfoBar::new()
                .severity(InfoBarSeverity::Error)
                .title("Measurement failed")
                .message(self.error.clone().unwrap_or_default())
                .is_open(self.error.is_some())
                .is_closable(true)
                .on_closed(cx.callback(|()| Msg::CloseError)),
        ))
    }
}

// ---- the meter: its own component, fed by a re-armed waiter --------------------------------

const TICK: Duration = Duration::from_millis(50); // 20 Hz; std's sleep uses a high-resolution timer
const TRACK: f64 = 260.0;

pub enum MeterMsg { Tick { mic: f32, out: f32 } }

pub struct Meter { engine: Arc<Engine>, mic: f32, out: f32 }

/// One waiter at a time: armed in create, re-armed by each delivery (tmr-gui's `listen`)
fn arm(engine: &Arc<Engine>, cx: &ComponentContext<Meter>) {
    let engine = Arc::clone(engine);
    cx.spawn_background(move |_cancel| {
        std::thread::sleep(TICK);
        let m = &engine.meters;
        MeterMsg::Tick { mic: Meters::take(&m.mic), out: Meters::take(&m.out) }
    });
}

impl Component for Meter {
    type Input = Shared;
    type Message = MeterMsg;

    fn create(input: &Shared, cx: &ComponentContext<Self>) -> Self {
        arm(&input.0, cx);
        Self { engine: Arc::clone(&input.0), mic: 0.0, out: 0.0 }
    }

    fn update(&mut self, msg: MeterMsg, cx: &ComponentContext<Self>) {
        let MeterMsg::Tick { mic, out } = msg;
        (self.mic, self.out) = (mic, out);
        arm(&self.engine, cx);
    }

    fn view(&self, _input: &Shared, _cx: &mut ViewContext<Self>) -> View {
        StackPanel::new().spacing(6.0).children((bar("Mic", self.mic), bar("Out", self.out)))
    }
}

/// A fixed track and a left-aligned fill: one Width set per tick
fn bar(label: &'static str, peak: f32) -> View {
    // max/min drop NaN; the result must be finite and >= 0, or LayoutControl::width panics (element.rs:2218-2233)
    let db = 20.0 * f64::from(peak.max(1e-6).min(1.0)).log10();
    let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);          // -60..0 dBFS -> 0..1
    StackPanel::new().orientation(Orientation::Horizontal).spacing(8.0).children((
        TextBlock::new().text(label).width(32.0).vertical_alignment(VerticalAlignment::Center),
        Border::new()
            .width(TRACK)
            .height(10.0)
            .vertical_alignment(VerticalAlignment::Center)
            .corner_radius(5.0)
            .background(Color::rgb(0x33, 0x33, 0x33))
            .content(
                Border::new()
                    .width(fill * TRACK)
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .corner_radius(5.0)
                    .background(Color::rgb(0x3C, 0xB3, 0x71)),
            ),
    ))
}
```

In nf-app, the GUI branch: start the engine first (its threads set up MTA COM themselves), then
call `nf_gui::run(Arc::clone(&engine), settings)` on the main thread. After it returns, call
`engine.stop()` explicitly (pitfall 8). To bring discrete events in as well (device changes, AEC
stats), give a second waiter an `mpsc::Receiver` and have it `recv_timeout(TICK)` then `try_iter()`.
The receiver rides inside the message, as in TMR's `Batch.feeds`.
