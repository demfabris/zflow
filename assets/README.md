# App artwork

`logo.svg` is the monochrome Cursor Flow mark.

| Platform | Source | Build usage |
| --- | --- | --- |
| macOS | `zflow.icon/` | The Mac build compiles the Icon Composer document into `Assets.car` and `zflow.icns`, including light, dark and tinted appearances. |
| GNOME | `linux/io.zflow.zflow.svg` | 128px app icon, installed under `hicolor/scalable/apps`. |
| GNOME symbolic | `linux/io.zflow.zflow-symbolic.svg` | 16px monochrome icon, installed under `hicolor/symbolic/apps`. |
| Windows | `windows/zflow.svg` | Editable 48px master; `zflow.ico` supplies executable, window and tray icons, and `zflow.png` supplies the title bar. |

The GNOME version uses the GNOME palette and a 4px lower profile. The Windows version uses a blue gradient and a subtle face highlight. Both keep transparent backgrounds and orange motion strokes from the Mac icon's palette.

After editing the Windows master, export a transparent 256px PNG and regenerate the ICO with 16, 20, 24, 32, 40, 48, 64, 96, 128 and 256px images. The checked-in PNG and ICO are used directly by the Windows build.

Design references: [GNOME app icons](https://developer.gnome.org/hig/guidelines/app-icons.html), [Windows app icons](https://learn.microsoft.com/en-us/windows/apps/design/iconography/app-icon-design), [Icon Composer](https://developer.apple.com/documentation/xcode/creating-your-app-icon-using-icon-composer).
