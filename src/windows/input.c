// Windows desktop adapter. Hooks do no I/O, allocation or waiting. A separate
// Raw Input window supplies device motion even while the local cursor is held.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <wtsapi32.h>
#include <stdint.h>
#include <wchar.h>
#include <stdlib.h>
#include <string.h>

typedef int (*event_fn)(int kind, int code, int value, int extra);
static event_fn emit;
static volatile LONG remote_input;
static volatile LONG64 heartbeat;
static BOOL keys[256];
static HHOOK keyboard_hook, mouse_hook;
static HWND clipboard_window;
#define ZFLOW_TAG ((ULONG_PTR)0x5a464c4f)
int zflow_input_clean(void);

// Configured cross-computer edges only; other local monitor seams stay native.
// The hook never waits for the engine: publishing briefly holds the write lock,
// while a contended reader simply lets this motion through.
typedef struct { int left, top, right, bottom, edge, start, end, returning; } zflow_boundary;
static SRWLOCK boundary_lock = SRWLOCK_INIT;
static zflow_boundary boundaries[128];
static int boundary_count;
static volatile LONG boundaries_enabled;
static BOOL warping;

void zflow_input_boundaries(const zflow_boundary *items, int count) {
    AcquireSRWLockExclusive(&boundary_lock);
    boundary_count = count >= 0 && count <= 128 ? count : 0;
    if (boundary_count) memcpy(boundaries, items, boundary_count * sizeof(*items));
    InterlockedExchange(&boundaries_enabled, boundary_count != 0);
    ReleaseSRWLockExclusive(&boundary_lock);
}

// Pure segment intersection, also exercised by Rust tests against this C code.
// Stop at the last source pixel, including large moves that skip over a seam.
int zflow_boundary_hit(const zflow_boundary *b, int x, int y, int nx, int ny, int *hx, int *hy) {
    if (x < b->left || x >= b->right || y < b->top || y >= b->bottom) return 0;
    int vertical = b->edge < 2;
    int from = vertical ? x : y, to = vertical ? nx : ny;
    int plane = b->edge == 0 ? b->left : b->edge == 1 ? b->right - 1 :
        b->edge == 2 ? b->top : b->bottom - 1;
    int positive = b->edge == 1 || b->edge == 3;
    if (positive ? (to <= from || to < plane) : (to >= from || to > plane)) return 0;
    double t = from == plane ? 1.0 : (double)(plane - from) / (to - from);
    double along = vertical ? y + t * (ny - y) : x + t * (nx - x);
    if (along < b->start || along >= b->end) return 0;
    // Round to the closest pixel, including negative desktop coordinates.
    int pixel = (int)(along >= 0 ? along + 0.5 : along - 0.5);
    if (pixel < b->start) pixel = b->start;
    if (pixel >= b->end) pixel = b->end - 1;
    *hx = vertical ? plane : pixel;
    *hy = vertical ? pixel : plane;
    return 1;
}

static BOOL stop_at_boundary(const MSLLHOOKSTRUCT *m) {
    if (warping || !InterlockedCompareExchange(&boundaries_enabled, 0, 0) ||
        GetTickCount64() - (ULONGLONG)InterlockedCompareExchange64(&heartbeat, 0, 0) > 1000)
        return FALSE;
    POINT from, hit;
    if (!GetCursorPos(&from) || !TryAcquireSRWLockShared(&boundary_lock)) return FALSE;
    int edge = -1, returning = 0;
    for (int i = 0; i < boundary_count; ++i) {
        const zflow_boundary *b = &boundaries[i];
        // Remote return guards see our injected motion too. Outbound guards
        // only see the user's mouse, never accessibility tools or our warps.
        if ((m->flags & LLMHF_INJECTED) && (!b->returning || m->dwExtraInfo != ZFLOW_TAG)) continue;
        int x, y;
        if (zflow_boundary_hit(b, from.x, from.y, m->pt.x, m->pt.y, &x, &y)) {
            hit.x = x; hit.y = y; edge = b->edge; returning = b->returning; break;
        }
    }
    ReleaseSRWLockShared(&boundary_lock);
    if (edge < 0 || !zflow_input_clean()) return FALSE;
    for (int i = 0; i < 256; ++i) if (keys[i]) return FALSE;
    warping = TRUE;
    BOOL placed = SetCursorPos(hit.x, hit.y);
    warping = FALSE;
    if (!placed) return FALSE;
    if (!returning && emit && !emit(7, hit.x, hit.y, edge)) {
        InterlockedExchange(&boundaries_enabled, 0);
        return FALSE;
    }
    return TRUE;
}

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
    if (w == WM_MOUSEMOVE && !InterlockedCompareExchange(&remote_input, 0, 0) && stop_at_boundary(m)) return 1;
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
    zflow_input_boundaries(NULL, 0);
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

typedef void (*monitor_fn)(int x, int y, int width, int height, const char *id,
    const char *name, unsigned int width_mm, unsigned int height_mm, void *context);
typedef struct { monitor_fn callback; void *context; } monitor_context;

// EDID physical dimensions follow the panel through resolution and DPI changes.
static void monitor_size(const WCHAR *device_path, UINT *width, UINT *height) {
    WCHAR key[512] = L"SYSTEM\\CurrentControlSet\\Enum\\";
    const WCHAR *start = device_path;
    if (wcsncmp(start, L"\\\\?\\", 4) != 0) return;
    start += 4;
    size_t at = wcslen(key);
    for (; *start && *start != L'{'; ++start) {
        if (at + 32 >= 512) return;
        key[at++] = *start == L'#' ? L'\\' : *start;
    }
    key[at] = 0;
    if (!*start) return;
    wcscat_s(key, 512, L"Device Parameters");
    BYTE edid[4096]; DWORD size = sizeof(edid);
    if (RegGetValueW(HKEY_LOCAL_MACHINE, key, L"EDID", RRF_RT_REG_BINARY, NULL, edid, &size) == ERROR_SUCCESS
        && size >= 128 && edid[0] == 0 && edid[1] == 255 && edid[21] && edid[22]) {
        *width = edid[21] * 10; *height = edid[22] * 10;
    }
}

static BOOL CALLBACK monitor(HMONITOR m, HDC dc, LPRECT r, LPARAM p) {
    (void)dc;
    monitor_context *ctx = (monitor_context *)p;
    MONITORINFOEXW info = {0}; info.cbSize = sizeof(info);
    if (!GetMonitorInfoW(m, (MONITORINFO *)&info)) return FALSE;
    WCHAR id[512], name[128];
    wcscpy_s(id, 512, info.szDevice); wcscpy_s(name, 128, info.szDevice);
    UINT width_mm = 0, height_mm = 0;
    UINT32 paths_count = 0, modes_count = 0;
    if (GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &paths_count, &modes_count) == ERROR_SUCCESS) {
        DISPLAYCONFIG_PATH_INFO *paths = calloc(paths_count, sizeof(*paths));
        DISPLAYCONFIG_MODE_INFO *modes = calloc(modes_count, sizeof(*modes));
        if (paths && modes && QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &paths_count, paths, &modes_count, modes, NULL) == ERROR_SUCCESS) {
            for (UINT32 i = 0; i < paths_count; ++i) {
                DISPLAYCONFIG_SOURCE_DEVICE_NAME source = {0};
                source.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
                source.header.size = sizeof(source); source.header.adapterId = paths[i].sourceInfo.adapterId; source.header.id = paths[i].sourceInfo.id;
                if (DisplayConfigGetDeviceInfo(&source.header) != ERROR_SUCCESS || wcscmp(source.viewGdiDeviceName, info.szDevice)) continue;
                DISPLAYCONFIG_TARGET_DEVICE_NAME target = {0};
                target.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
                target.header.size = sizeof(target); target.header.adapterId = paths[i].targetInfo.adapterId; target.header.id = paths[i].targetInfo.id;
                if (DisplayConfigGetDeviceInfo(&target.header) != ERROR_SUCCESS) continue;
                if (*target.monitorDevicePath) wcscpy_s(id, 512, target.monitorDevicePath);
                if (*target.monitorFriendlyDeviceName) wcscpy_s(name, 128, target.monitorFriendlyDeviceName);
                monitor_size(id, &width_mm, &height_mm);
                if (paths[i].targetInfo.rotation == DISPLAYCONFIG_ROTATION_ROTATE90 || paths[i].targetInfo.rotation == DISPLAYCONFIG_ROTATION_ROTATE270) {
                    UINT swap = width_mm; width_mm = height_mm; height_mm = swap;
                }
                break;
            }
        }
        free(paths); free(modes);
    }
    char utf8_id[1536] = {0}, utf8_name[384] = {0};
    WideCharToMultiByte(CP_UTF8, 0, id, -1, utf8_id, sizeof(utf8_id), NULL, NULL);
    WideCharToMultiByte(CP_UTF8, 0, name, -1, utf8_name, sizeof(utf8_name), NULL, NULL);
    ctx->callback(r->left, r->top, r->right-r->left, r->bottom-r->top, utf8_id, utf8_name, width_mm, height_mm, ctx->context);
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
