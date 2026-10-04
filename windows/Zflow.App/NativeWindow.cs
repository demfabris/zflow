using System.Runtime.InteropServices;
using Microsoft.UI.Xaml;

namespace Zflow;
// A Win32 notification-area icon complements the WinUI window. All callbacks run
// on the window's dispatcher; no engine work happens inside a window procedure.
internal sealed class NativeWindow : IDisposable
{
    private readonly Window window;
    private readonly nint hwnd;
    private readonly SubclassProc callback;
    private NotifyIconData icon;
    public event Action? QuitRequested;
    private const uint TrayMessage = 0x8001;
    private readonly uint taskbarCreated = RegisterWindowMessage("TaskbarCreated");
    public NativeWindow(Window window, string iconPath)
    {
        this.window = window; hwnd = WinRT.Interop.WindowNative.GetWindowHandle(window); callback = Procedure;
        // Keep the owned icon until disposal so Explorer restarts can reuse it.
        nint trayIcon = LoadImage(0, iconPath, 1, GetSystemMetrics(49), GetSystemMetrics(50), 0x10);
        if (trayIcon == 0) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "Could not load the zflow tray icon.");
        icon = new NotifyIconData { cbSize = (uint)Marshal.SizeOf<NotifyIconData>(), hWnd = hwnd, uID = 1, uFlags = 1 | 2 | 4, uCallbackMessage = TrayMessage, hIcon = trayIcon, szTip = "zflow · Open to manage input sharing", szInfo = "", szInfoTitle = "" };
        if (!SetWindowSubclass(hwnd, callback, 1, 0)) { DestroyIcon(trayIcon); throw new InvalidOperationException("Could not attach the zflow tray menu."); }
        Shell_NotifyIcon(0, ref icon);
    }
    private nint Procedure(nint h, uint message, nuint w, nint l, nuint id, nuint data)
    {
        if (message == taskbarCreated) Shell_NotifyIcon(0, ref icon);
        if (message == 0x8002) { window.DispatcherQueue.TryEnqueue(() => QuitRequested?.Invoke()); return 0; }
        if (message == TrayMessage)
        {
            if ((int)l is 0x202 or 0x203) { window.AppWindow.Show(); window.Activate(); SetForegroundWindow(hwnd); }
            if ((int)l == 0x205)
            {
                nint menu = CreatePopupMenu(); AppendMenu(menu, 0, 1, "Open zflow"); AppendMenu(menu, 0, 2, "Quit zflow");
                GetCursorPos(out var point); SetForegroundWindow(hwnd);
                uint chosen = TrackPopupMenu(menu, 0x100 | 0x2, point.X, point.Y, 0, hwnd, 0); DestroyMenu(menu);
                if (chosen == 1) { window.AppWindow.Show(); window.Activate(); }
                if (chosen == 2) window.DispatcherQueue.TryEnqueue(() => QuitRequested?.Invoke());
            }
            return 0;
        }
        return DefSubclassProc(h, message, w, l);
    }
    public void Dispose()
    {
        if (icon.hIcon == 0) return;
        Shell_NotifyIcon(2, ref icon); RemoveWindowSubclass(hwnd, callback, 1);
        DestroyIcon(icon.hIcon); icon.hIcon = 0;
    }
    public static void ShowExisting() { nint h = FindWindow(null, "zflow"); if (h != 0) { ShowWindow(h, 9); SetForegroundWindow(h); } }
    public static void QuitExisting() { nint h = FindWindow(null, "zflow"); if (h != 0) PostMessage(h, 0x8002, 0, 0); }
    private delegate nint SubclassProc(nint hwnd, uint message, nuint w, nint l, nuint id, nuint data);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] private struct NotifyIconData { public uint cbSize; public nint hWnd; public uint uID, uFlags, uCallbackMessage; public nint hIcon; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string szTip; public uint dwState, dwStateMask; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string szInfo; public uint uTimeout; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string szInfoTitle; public uint dwInfoFlags; public Guid guidItem; public nint hBalloonIcon; }
    [StructLayout(LayoutKind.Sequential)] private struct Point { public int X, Y; }
    [DllImport("shell32", CharSet = CharSet.Unicode)] [return: MarshalAs(UnmanagedType.Bool)] private static extern bool Shell_NotifyIcon(uint message, ref NotifyIconData data);
    [DllImport("comctl32")] [return: MarshalAs(UnmanagedType.Bool)] private static extern bool SetWindowSubclass(nint hwnd, SubclassProc callback, nuint id, nuint data);
    [DllImport("comctl32")] private static extern bool RemoveWindowSubclass(nint hwnd, SubclassProc callback, nuint id);
    [DllImport("comctl32")] private static extern nint DefSubclassProc(nint hwnd, uint message, nuint w, nint l);
    [DllImport("user32", CharSet = CharSet.Unicode, SetLastError = true)] private static extern nint LoadImage(nint instance, string name, uint type, int width, int height, uint flags);
    [DllImport("user32")] private static extern int GetSystemMetrics(int index);
    [DllImport("user32")] [return: MarshalAs(UnmanagedType.Bool)] private static extern bool DestroyIcon(nint icon);
    [DllImport("user32", CharSet = CharSet.Unicode)] private static extern uint RegisterWindowMessage(string name);
    [DllImport("user32", CharSet = CharSet.Unicode)] private static extern nint FindWindow(string? cls, string title);
    [DllImport("user32")] private static extern bool ShowWindow(nint hwnd, int command);
    [DllImport("user32")] private static extern bool PostMessage(nint hwnd, uint message, nuint w, nint l);
    [DllImport("user32")] private static extern bool SetForegroundWindow(nint hwnd);
    [DllImport("user32")] private static extern nint CreatePopupMenu();
    [DllImport("user32", CharSet = CharSet.Unicode)] private static extern bool AppendMenu(nint menu, uint flags, nuint id, string label);
    [DllImport("user32")] private static extern uint TrackPopupMenu(nint menu, uint flags, int x, int y, int reserved, nint owner, nint rectangle);
    [DllImport("user32")] private static extern bool DestroyMenu(nint menu);
    [DllImport("user32")] private static extern bool GetCursorPos(out Point point);
}
