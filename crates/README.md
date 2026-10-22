# crates

zflow's app and the UI stack it is built on.

| Crate | What it is |
|---|---|
| `zflow-app` | The app: one window with the computers and the settings, a menu bar or tray icon, and on a Mac the engine itself. The same code runs on Linux, macOS and Windows. |
| `zz-gpui`, `zz-gpui-macros`, `zz-gpui-platform` | zz's fork of Zed's GPUI, vendored. |
| `zz-gpui-kit` | zz's widget kit, vendored, with the additions listed below. |
| `zz-gpui-tray` | New: a tray or menu bar icon with a menu, for any zz-gpui app. |

The zz-gpui crates were copied from [demfabris/zz](https://github.com/demfabris/zz) at `32ee3ab37`. They follow zz's design language: colors come from the theme, surfaces from roles, and the app draws zz's Default interface theme in the system's light or dark mode.

## Backport to zz

Everything here that is not zflow's goes back to zz. Each item is self-contained and depends on nothing in zflow.

### zz-gpui-kit

The shared code in these modules matches zz-ui at `32ee3ab37`: same constants, role ids and style patches. What a backport has to reconcile:

- `roles`: the 12 chrome roles move here from zz-ui's `roles.rs`; zz-ui drops them and re-exports the kit's (zz uses them in 19 places).
  - `register` keeps the app's roles in a `OnceLock`: zz must register all of its roles in one call, before any theme is parsed, or Liquid Glass's 19 zz role ids are rejected.
  - Chrome roles now list before app roles, which reorders the Cards and Rules groups in the theme editor.
  - Descriptions were made generic ("zz draws", "session sidebar", "pane headers" dropped); zz may want its wording back.
  - `members()` was dropped (unused at `32ee3ab37`), and the test now checks only chrome roles; zz-ui's checks of kinds, labels and groups should move here.
- `interface_theme`: the Default theme is built in and `BUILTIN_THEMES` is `["default"]`, so `builtin_theme("liquid-glass")` is `None` until the kit can take extra built-in themes from zz-ui.
  - The kit has its own copies of `ThemeModeSetting` and the accent and tint constants instead of re-exporting `zz_client::appearance`; the backport should make zz-client use the kit's types, or the kit re-export zz-client's. `AccentSetting` is not re-exported.
  - Two tests swapped zz role ids for kit ones (`pane.chip` became `badge`, the composer suggestion became `preview`); zz-ui keeps the originals.
- `settings`: `SettingsStack`, `SettingEntry`, `SettingCopy`, `StackPosition`, `settings_page_content`, `settings_page_heading`, `settings_scroll_column`, `SettingsBottomInset`, `settings_card_ground`, `settings_preview`, `settings_provenance_badge`, `settings_reset_button`, `SettingsSelectItem`, and the `settings_list_*` group headers.
  - Navigation takes plain values: `settings_navigation_button(id, title, icon, selected, cx)` and `settings_navigation_group_label(title, cx)` replace the `SettingsSection` and `SettingsNavigationGroup` forms; `settings_navigation_back_row` is `settings_navigation_row` with its own 32 px height, no longer tied to `WORKSPACE_TREE_ROW_HEIGHT`.
  - Stay in zz-ui: `SettingsSection`, `SettingsNavigationGroup`, `settings_section_index`, `settings_page_description`, the virtual column, `panes_page` and the section submodules. They need these made `pub` in the kit: `settings_group_header`, `SETTINGS_PAGE_PADDING`, `SETTINGS_STACK_PADDING`, and `StackPosition`'s `rounds_top`, `rounds_bottom` and `rules_above`.
  - Two doc links point at `settings_virtual_column`, which stayed in zz-ui. The module's tests (scroll tests, stack positions, focused field, bottom inset) stayed behind and should come with it.
- `sidebar`: the `workspace_sidebar_*` functions are `sidebar_*`, `workspace_controls_leading_inset` is `controls_leading_inset`, and `WORKSPACE_TREE_CONTENT_INSET` is `SIDEBAR_CONTENT_INSET`; zz's tree rows use the same value, so keep one constant.
  - `chrome_button` is now `pub`; `STATUS_DOT_SIZE`, `SIDEBAR_EDGE_INSET` and `edge_inset` are private, but zz-ui reads them (`flat.rs`, `palette.rs`, `workspace_row_action_gap`), so they need `pub`.
  - `workspace_sidebar_surface` has no kit version yet.
- `shell`: `app_shell_surface` (its `workspace` argument is `content`), `chrome_background`, and `app_titlebar_strip`, which takes a new `leading` slot inside the drag area, so zz's callers change. `window_drag_handle` comes from `crates/zz/src/window/drag.rs` and is `pub`; its test `background_drag_yields_to_buttons_and_text_fields` should come along.
  - zz already has a different `theme::chrome_background`; one of the two needs another name.
  - Stay in zz-ui: `app_workspace_surface`, `app_connection_state`, `workspace_status_bar`, `empty_workspace*`.

- `title_bar`: `WindowControls` now defaults to `roles::WINDOW_CONTROLS`. Its old private default was `ICON_BUTTON.with(..)`, which replaces the icon button's bare look, so Windows caption buttons drew as filled, outlined boxes. zz sets the role itself in `app_shell.rs`, so it never showed there.

None of these modules mention zflow. zflow's own roles and sizes live in `crates/zflow-app`.

### zz-gpui and zz-gpui-platform

- `Window::activate_window_with_token` and `PlatformWindow::activate_with_token`: raise a window with an activation token another process handed over. Wayland uses it; other platforms fall back to `activate`. zflow uses it when a second launch hands its `XDG_ACTIVATION_TOKEN` to the running copy.
- `build.rs` writes empty Metal outputs when a non-Mac host checks a Mac build, so `cargo check --target aarch64-apple-darwin` works from Linux. Nothing built that way can run.

### zz-gpui-tray

A new crate from zz's `crates/zz/src/tray` backends (ksni, `NSStatusItem`, `Shell_NotifyIcon`), made generic: the app hands it a `Menu` of items with actions and check marks and reads `TrayEvent`s. zz's tray becomes its `facts.rs` menu plus this crate.

## Checking other platforms from Linux

`cargo check` and `cargo clippy` for Windows and macOS run on Linux with a stand-in C compiler, since nothing links:

- Windows: `--target x86_64-pc-windows-gnu`, `RC_x86_64_pc_windows_gnu=llvm-rc`, and a `CC_x86_64_pc_windows_gnu` that preprocesses with `clang` (`clang --driver-mode=cl` for the resource file) and writes empty objects otherwise.
- macOS: `--target aarch64-apple-darwin` and the same stand-in as `CC_aarch64_apple_darwin`.

Real builds still need each system: Xcode on a Mac, the Windows SDK's `fxc.exe` on Windows for release shaders.
