// Windows desktop adapter. Hooks do no I/O, allocation or waiting. A separate
// Raw Input window supplies device motion even while the local cursor is held.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <wtsapi32.h>
#include <stdint.h>
#include <wchar.h>

typedef int (*event_fn)(int kind, int code, int value, int extra);
static event_fn emit;
static volatile LONG remote_input;
static volatile LONG64 heartbeat;
static BOOL keys[256];
static HHOOK keyboard_hook, mouse_hook;
static HWND clipboard_window;
#define ZFLOW_TAG ((ULONG_PTR)0x5a464c4f)
int zflow_input_clean(void);

static void event(int kind, int code, int value, int extra) {
    if (emit && !emit(kind, code, value, extra)) InterlockedExchange(&remote_input, 0);
}

static LRESULT CALLBACK keyboard(int n, WPARAM w, LPARAM l) {
    if (n < 0) return CallNextHookEx(NULL, n, w, l);
    const KBDLLHOOKSTRUCT *k = (const KBDLLHOOKSTRUCT *)l;
    if (k->flags & LLKHF_INJECTED) return CallNextHookEx(NULL, n, w, l);
    BOOL down = !(k->flags & LLKHF_UP);
    BOOL repeat = keys[k->vkCode & 255];
    keys[k->vkCode & 255] = down;
    BOOL ctrl = keys[VK_LCONTROL] || keys[VK_RCONTROL];
    BOOL win = keys[VK_LWIN] || keys[VK_RWIN];
    if (down && ctrl && win && (k->vkCode == VK_BACK || k->vkCode == VK_F12)) {
        if (k->vkCode == VK_BACK) InterlockedExchange(&remote_input, 0);
        if (!repeat) event(k->vkCode == VK_BACK ? 5 : 6, 0, 0, 0);
        return 1;
    }
    if (InterlockedCompareExchange(&remote_input, 0, 0)) {
        if (!down || !repeat) event(1, (int)k->scanCode, down, (int)k->flags | ((int)k->vkCode << 8));
        return 1;
    }
    return CallNextHookEx(NULL, n, w, l);
}

static LRESULT CALLBACK mouse(int n, WPARAM w, LPARAM l) {
    if (n < 0) return CallNextHookEx(NULL, n, w, l);
    const MSLLHOOKSTRUCT *m = (const MSLLHOOKSTRUCT *)l;
    if (m->flags & LLMHF_INJECTED) return CallNextHookEx(NULL, n, w, l);
    if (!InterlockedCompareExchange(&remote_input, 0, 0)) return CallNextHookEx(NULL, n, w, l);
    switch (w) {
        case WM_LBUTTONDOWN: case WM_LBUTTONUP: event(2, 1, w == WM_LBUTTONDOWN, 0); break;
        case WM_RBUTTONDOWN: case WM_RBUTTONUP: event(2, 2, w == WM_RBUTTONDOWN, 0); break;
        case WM_MBUTTONDOWN: case WM_MBUTTONUP: event(2, 3, w == WM_MBUTTONDOWN, 0); break;
        case WM_XBUTTONDOWN: case WM_XBUTTONUP: event(2, HIWORD(m->mouseData) == XBUTTON1 ? 4 : 5, w == WM_XBUTTONDOWN, 0); break;
        case WM_MOUSEWHEEL: event(4, 0, (SHORT)HIWORD(m->mouseData), 0); break;
        case WM_MOUSEHWHEEL: event(4, (SHORT)HIWORD(m->mouseData), 0, 0); break;
    }
    return 1;
}

static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM w, LPARAM l) {
    if (message == WM_APP + 1) {
        // Check and grab on the hook thread so a key cannot go down between
        // the neutral-state check and the suppression switch.
        if (!zflow_input_clean()) return 0;
        for (int i = 0; i < 256; ++i) if (keys[i]) return 0;
        InterlockedExchange64(&heartbeat, (LONG64)GetTickCount64());
        InterlockedExchange(&remote_input, 1);
        return 1;
    }
    if (message == WM_INPUT) {
        RAWINPUT data;
        UINT size = sizeof(data);
        if (GetRawInputData((HRAWINPUT)l, RID_INPUT, &data, &size, sizeof(RAWINPUTHEADER)) != (UINT)-1 &&
            data.header.dwType == RIM_TYPEMOUSE && !(data.data.mouse.usFlags & MOUSE_MOVE_ABSOLUTE) &&
            InterlockedCompareExchange(&remote_input, 0, 0)) {
            event(3, data.data.mouse.lLastX, data.data.mouse.lLastY, 0);
        }
    } else if (message == WM_TIMER) {
        if (InterlockedCompareExchange(&remote_input, 0, 0) &&
            GetTickCount64() - (ULONGLONG)InterlockedCompareExchange64(&heartbeat, 0, 0) > 1000) {
            InterlockedExchange(&remote_input, 0);
            event(5, 0, 0, 0);
        }
    }
    return DefWindowProcW(window, message, w, l);
}

int zflow_input_run(event_fn callback) {
    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    HINSTANCE instance = GetModuleHandleW(NULL);
    WNDCLASSW cls = {0};
    cls.lpfnWndProc = window_proc;
    cls.hInstance = instance;
    cls.lpszClassName = L"ZflowInput";
    if (!RegisterClassW(&cls) && GetLastError() != ERROR_CLASS_ALREADY_EXISTS) return 0;
    HWND window = CreateWindowW(cls.lpszClassName, L"", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, instance, NULL);
    if (!window) return 0;
    clipboard_window = window;
    RAWINPUTDEVICE device = {1, 2, RIDEV_INPUTSINK, window};
    if (!RegisterRawInputDevices(&device, 1, sizeof(device))) { DestroyWindow(window); return 0; }
    emit = callback;
    keyboard_hook = SetWindowsHookExW(WH_KEYBOARD_LL, keyboard, instance, 0);
    mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, mouse, instance, 0);
    int ok = keyboard_hook && mouse_hook && SetTimer(window, 1, 100, NULL);
    if (ok) {
        event(0, (int)GetCurrentThreadId(), 0, 0);
        MSG message;
        int result;
        while ((result = GetMessageW(&message, NULL, 0, 0)) > 0) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if (result < 0) ok = 0;
    }
    InterlockedExchange(&remote_input, 0);
    if (keyboard_hook) UnhookWindowsHookEx(keyboard_hook);
    if (mouse_hook) UnhookWindowsHookEx(mouse_hook);
    device.dwFlags = RIDEV_REMOVE;
    device.hwndTarget = NULL;
    RegisterRawInputDevices(&device, 1, sizeof(device));
    KillTimer(window, 1);
    DestroyWindow(window);
    clipboard_window = NULL;
    emit = NULL;
    return ok;
}

void zflow_input_remote(int enabled) {
    InterlockedExchange64(&heartbeat, (LONG64)GetTickCount64());
    InterlockedExchange(&remote_input, enabled);
}
int zflow_input_is_remote(void) { return (int)InterlockedCompareExchange(&remote_input, 0, 0); }
int zflow_input_grab(void) {
    DWORD_PTR result = 0;
    return clipboard_window && SendMessageTimeoutW(clipboard_window, WM_APP + 1, 0, 0,
        SMTO_ABORTIFHUNG | SMTO_BLOCK, 100, &result) && result == 1;
}
void zflow_input_pulse(void) { InterlockedExchange64(&heartbeat, (LONG64)GetTickCount64()); }
void zflow_input_stop(unsigned long thread) { PostThreadMessageW(thread, WM_QUIT, 0, 0); }
void *zflow_clipboard_window(void) { return clipboard_window; }

int zflow_desktop_available(void) {
    DWORD session;
    if (!ProcessIdToSessionId(GetCurrentProcessId(), &session)) return 0;
    WTSINFOEXW *info = NULL;
    DWORD bytes = 0;
    if (!WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, session, WTSSessionInfoEx, (LPWSTR *)&info, &bytes)) return 0;
    BOOL unlocked = bytes >= sizeof(WTSINFOEXW) && info->Level == 1 &&
        info->Data.WTSInfoExLevel1.SessionState == WTSActive &&
        info->Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_UNLOCK;
    WTSFreeMemory(info);
    if (!unlocked) return 0;
    HDESK desktop = OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS);
    if (!desktop) return 0;
    WCHAR name[256];
    BOOL ok = GetUserObjectInformationW(desktop, UOI_NAME, name, sizeof(name), &bytes);
    CloseDesktop(desktop);
    return ok && wcscmp(name, L"Default") == 0;
}

int zflow_input_clean(void) {
    for (int i = 1; i < 256; ++i) if (GetAsyncKeyState(i) & 0x8000) return 0;
    return 1;
}

// kind: 1 scan key, 2 virtual key, 3 button, 4 relative motion, 5 wheel.
int zflow_input_post(int kind, int code, int value, int extra) {
    INPUT input = {0};
    if (kind <= 2) {
        input.type = INPUT_KEYBOARD;
        input.ki.dwExtraInfo = ZFLOW_TAG;
        input.ki.wScan = kind == 1 ? (WORD)code : 0;
        input.ki.wVk = kind == 2 ? (WORD)code : 0;
        input.ki.dwFlags = (kind == 1 ? KEYEVENTF_SCANCODE : 0) |
            (extra ? KEYEVENTF_EXTENDEDKEY : 0) | (value ? 0 : KEYEVENTF_KEYUP);
    } else {
        input.type = INPUT_MOUSE;
        input.mi.dwExtraInfo = ZFLOW_TAG;
        if (kind == 3) {
            switch (code) {
                case 1: input.mi.dwFlags = value ? MOUSEEVENTF_LEFTDOWN : MOUSEEVENTF_LEFTUP; break;
                case 2: input.mi.dwFlags = value ? MOUSEEVENTF_RIGHTDOWN : MOUSEEVENTF_RIGHTUP; break;
                case 3: input.mi.dwFlags = value ? MOUSEEVENTF_MIDDLEDOWN : MOUSEEVENTF_MIDDLEUP; break;
                case 4: case 5:
                    input.mi.dwFlags = value ? MOUSEEVENTF_XDOWN : MOUSEEVENTF_XUP;
                    input.mi.mouseData = code == 4 ? XBUTTON1 : XBUTTON2; break;
                default: return 0;
            }
        } else if (kind == 4) {
            input.mi.dwFlags = MOUSEEVENTF_MOVE;
            input.mi.dx = code; input.mi.dy = value;
        } else if (kind == 5) {
            input.mi.dwFlags = extra ? MOUSEEVENTF_HWHEEL : MOUSEEVENTF_WHEEL;
            input.mi.mouseData = (DWORD)value;
        } else return 0;
    }
    return SendInput(1, &input, sizeof(input)) == 1;
}

typedef void (*monitor_fn)(int x, int y, int width, int height, void *context);
typedef struct { monitor_fn callback; void *context; } monitor_context;
static BOOL CALLBACK monitor(HMONITOR m, HDC dc, LPRECT r, LPARAM p) {
    (void)m; (void)dc;
    monitor_context *ctx = (monitor_context *)p;
    ctx->callback(r->left, r->top, r->right-r->left, r->bottom-r->top, ctx->context);
    return TRUE;
}
int zflow_monitors(monitor_fn callback, void *context) {
    DPI_AWARENESS_CONTEXT previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    monitor_context ctx = {callback, context};
    int result = EnumDisplayMonitors(NULL, NULL, monitor, (LPARAM)&ctx);
    SetThreadDpiAwarenessContext(previous);
    return result;
}
int zflow_cursor(int *x, int *y, int move) {
    DPI_AWARENESS_CONTEXT previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    POINT point;
    int result = move ? SetCursorPos(*x, *y) : GetCursorPos(&point);
    if (!move && result) { *x = point.x; *y = point.y; }
    SetThreadDpiAwarenessContext(previous);
    return result;
}
