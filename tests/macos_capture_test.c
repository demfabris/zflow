#include <ApplicationServices/ApplicationServices.h>
#include <assert.h>
#include <dlfcn.h>
#include <float.h>
#include <sched.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void *fake_dlsym(void *, const char *);
static void *fake_dlopen(const char *, int);
static int fake_dlclose(void *);
static CGError fake_associate(boolean_t);
static CGError fake_hide(CGDirectDisplayID);
static CGError fake_show(CGDirectDisplayID);
static CGError fake_warp(CGPoint);
static CGPoint fake_location(CGEventRef);
static CGError fake_display_list(uint32_t, CGDirectDisplayID *, uint32_t *);
static CGRect fake_display_bounds(CGDirectDisplayID);
static bool fake_key_state(CGEventSourceStateID, CGKeyCode);
static bool fake_button_state(CGEventSourceStateID, CGMouseButton);
static CGEventFlags fake_flags_state(CGEventSourceStateID);
static Boolean fake_trusted(void);
static Boolean fake_secure_input(void);
static OSType fake_layout_type(SInt16);
static Boolean fake_trusted_options(CFDictionaryRef);
static CFMachPortRef fake_tap(CGEventTapLocation, CGEventTapPlacement,
                             CGEventTapOptions, CGEventMask,
                             CGEventTapCallBack, void *);
static void fake_tap_enable(CFMachPortRef, bool);
static CFRunLoopRunResult fake_run_loop(CFRunLoopMode, CFTimeInterval, Boolean);

// Replace cursor mutations and tap creation before including the bridge.
// These tests do not capture, hide, disconnect, or post input on the host.
// The capture thread runs its real run loop on a plain Mach port.
#define dlsym fake_dlsym
#define dlopen fake_dlopen
#define dlclose fake_dlclose
#define CGEventTapEnable fake_tap_enable
#define CFRunLoopRunInMode fake_run_loop
#define CGAssociateMouseAndMouseCursorPosition fake_associate
#define CGDisplayHideCursor fake_hide
#define CGDisplayShowCursor fake_show
#define CGEventTapCreate fake_tap
#define CGWarpMouseCursorPosition fake_warp
#define CGEventGetLocation fake_location
#define CGGetActiveDisplayList fake_display_list
#define CGDisplayBounds fake_display_bounds
#define CGEventSourceKeyState fake_key_state
#define CGEventSourceButtonState fake_button_state
#define CGEventSourceFlagsState fake_flags_state
#define AXIsProcessTrusted fake_trusted
#define AXIsProcessTrustedWithOptions fake_trusted_options
#define IsSecureEventInputEnabled fake_secure_input
#define KBGetLayoutType fake_layout_type
#include "../src/macos/capture_bridge.c"
#undef CFRunLoopRunInMode

static char calls[64];
static size_t call_count;
static char fail_call;
static bool missing_symbol;
static bool connected;
static bool background;
static int hide_count;
static int tap_calls;
static int motion_taps;
static bool tap_available;
static int tap_enables;
// The window server's answer to the Accessibility probe's throwaway tap.
static bool probe_allowed;
static int probe_calls;
static bool lose_stop;
static _Atomic bool invalidate_tap;
static _Atomic int loop_runs;
static _Atomic int wakes;
static const ZFlowMacRect entry = {-1200, -100, 9, 200};
static CGPoint warped_position;
static CGPoint cursor_position = {-1200, -50};
static int held_key = -1;
static int held_button = -1;
static CGEventFlags held_flags;
static int permission_prompts;
static bool expect_return_warp;
static CGError record(char call);

static CGError fake_warp(CGPoint position) {
  if (expect_return_warp) assert(!connected && hide_count == 1 && background);
  CGError error = record('W');
  if (error == kCGErrorSuccess) warped_position = position;
  return error;
}

static CGPoint fake_location(CGEventRef event) {
  assert(event);
  return cursor_position;
}

static CGError fake_display_list(uint32_t capacity, CGDirectDisplayID *ids,
                                 uint32_t *count) {
  assert(capacity >= 2);
  ids[0] = 1; ids[1] = 2; *count = 2;
  return kCGErrorSuccess;
}

static CGRect fake_display_bounds(CGDirectDisplayID display) {
  return display == 1 ? CGRectMake(0, 0, 1920, 1080)
                      : CGRectMake(-1920, -200, 1920, 1080);
}

static bool fake_key_state(CGEventSourceStateID state, CGKeyCode key) {
  assert(state == kCGEventSourceStateHIDSystemState);
  return key == held_key;
}

static bool fake_button_state(CGEventSourceStateID state, CGMouseButton button) {
  assert(state == kCGEventSourceStateHIDSystemState);
  return (int)button == held_button;
}

static CGEventFlags fake_flags_state(CGEventSourceStateID state) {
  assert(state == kCGEventSourceStateHIDSystemState);
  return held_flags;
}

static Boolean fake_trusted(void) { return false; }

static bool secure_input;
static Boolean fake_secure_input(void) { return secure_input; }

// Keyboard types with the layouts HIToolbox reports for them.
enum { ANSI_KEYBOARD = 40, ISO_KEYBOARD = 41, JIS_KEYBOARD = 42 };

static OSType fake_layout_type(SInt16 keyboard) {
  switch (keyboard) {
    case ANSI_KEYBOARD: return 0x414E5349; // 'ANSI'
    case ISO_KEYBOARD: return ZFLOW_KEYBOARD_ISO;
    case JIS_KEYBOARD: return 0x4A495320; // 'JIS '
    default: return 0x3F3F3F3F; // '????'
  }
}

static Boolean fake_trusted_options(CFDictionaryRef options) {
  assert(CFDictionaryGetValue(options, kAXTrustedCheckOptionPrompt) == kCFBooleanTrue);
  permission_prompts++;
  return false;
}

static void desktop_and_permission_tests(void) {
  ZFlowMacPosition position;
  assert(zflow_mac_cursor_position(&position) == 0);
  assert(position.x == -1200 && position.y == -50);
  assert(zflow_mac_cursor_position(NULL) == -1);
  ZFlowMacRect rectangles[64];
  assert(zflow_mac_desktop_rectangles(rectangles, 64) == 2);
  assert(rectangles[1].x == -1920 && rectangles[1].y == -200);
  assert(zflow_mac_desktop_rectangles(rectangles, 1) == -1);
  assert(zflow_mac_input_is_neutral());
  // A key alone never blocks entry; macOS can report one long after release.
  held_key = 0; assert(zflow_mac_input_is_neutral()); held_key = -1;
  held_button = 0; assert(!zflow_mac_input_is_neutral()); held_button = -1;
  held_flags = kCGEventFlagMaskCommand; assert(!zflow_mac_input_is_neutral());
  held_flags = kCGEventFlagMaskAlphaShift; assert(zflow_mac_input_is_neutral());
  held_flags = 0;
  g_entry_region = (ZFlowMacRect){-1200, -100, 9, 200};
  assert(capture_entry_allowed() == 0);
  held_key = 10; assert(capture_entry_allowed() == 0); held_key = -1;
  held_button = 1; assert(capture_entry_allowed() == -2); held_button = -1;
  cursor_position.y -= 11; assert(capture_entry_allowed() == 0);
  cursor_position.y = 50; assert(capture_entry_allowed() == 0);
  cursor_position.x = -1191; assert(capture_entry_allowed() == -2);
  assert(strstr(g_error, "left the configured crossing edge"));
  cursor_position.x = -1200;
  cursor_position.y = 100; assert(capture_entry_allowed() == -2);
  cursor_position.y = -100; assert(capture_entry_allowed() == 0);
  cursor_position.y = -100.1; assert(capture_entry_allowed() == -2);

  g_entry_region = (ZFlowMacRect){-1250, -50, 100, 9};
  cursor_position = CGPointMake(-1200, -50); assert(capture_entry_allowed() == 0);
  cursor_position.x += 11; assert(capture_entry_allowed() == 0);
  cursor_position.x = -1150; assert(capture_entry_allowed() == -2);
  cursor_position = CGPointMake(-1200, -41); assert(capture_entry_allowed() == -2);

  ZFlowMacRect invalid_regions[] = {
    {NAN, -100, 9, 200}, {-1200, INFINITY, 9, 200},
    {-1200, -100, NAN, 200}, {-1200, -100, 9, INFINITY},
    {-1200, -100, 0, 200}, {-1200, -100, 9, 0},
    {-1200, -100, -9, 200}, {-1200, -100, 9, -200},
    {DBL_MAX, -100, DBL_MAX, 200}, {-1200, DBL_MAX, 9, DBL_MAX},
  };
  cursor_position = CGPointMake(-1200, -50);
  for (size_t i = 0; i < sizeof(invalid_regions) / sizeof(invalid_regions[0]); i++) {
    g_entry_region = invalid_regions[i];
    assert(capture_entry_allowed() == -1);
    assert(strstr(g_error, "configured crossing edge is invalid"));
  }
  g_entry_region = (ZFlowMacRect){-1200, -100, 9, 200};
  cursor_position.x = NAN; assert(capture_entry_allowed() == -1);
  cursor_position = CGPointMake(-1200, -50);
  assert(!zflow_mac_accessibility_authorized(0));
  assert(permission_prompts == 0);
  assert(!zflow_mac_accessibility_authorized(1));
  assert(permission_prompts == 1);
}

static void startup_release_tests(void) {
  memset(g_forwarded_keys, 0, sizeof(g_forwarded_keys));
  memset(g_forwarded_buttons, 0, sizeof(g_forwarded_buttons));
  atomic_store(&g_stop, false);
  g_queue_head = g_queue_tail = 0;
  CGEventRef key = CGEventCreateKeyboardEvent(NULL, 0, false);
  assert(key);
  assert(event_callback(NULL, kCGEventKeyUp, key, NULL) == key);
  assert(event_callback(NULL, kCGEventKeyDown, key, NULL) == NULL);
  assert(event_callback(NULL, kCGEventKeyUp, key, NULL) == NULL);
  CFRelease(key);
  // A key held since before capture: repeats are dropped, the release stays local.
  CGEventRef repeat = CGEventCreateKeyboardEvent(NULL, 4, true);
  assert(repeat);
  CGEventSetIntegerValueField(repeat, kCGKeyboardEventAutorepeat, 1);
  assert(event_callback(NULL, kCGEventKeyDown, repeat, NULL) == NULL);
  assert(!g_forwarded_keys[4]);
  assert(event_callback(NULL, kCGEventKeyUp, repeat, NULL) == repeat);
  CFRelease(repeat);
  CGEventRef button = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseUp,
                                             CGPointMake(0, 0), kCGMouseButtonLeft);
  assert(button);
  assert(event_callback(NULL, kCGEventLeftMouseUp, button, NULL) == button);
  assert(event_callback(NULL, kCGEventLeftMouseDown, button, NULL) == NULL);
  assert(event_callback(NULL, kCGEventLeftMouseUp, button, NULL) == NULL);
  CFRelease(button);
  ZFlowMacEvent captured;
  int count = 0;
  while (zflow_mac_capture_poll(&captured)) count++;
  assert(count == 4);
  ZFlowMacEvent pre_capture = {.kind = ZFLOW_EVENT_TOUCH, .contact_count = 1};
  assert(enqueue(&pre_capture));
  clear_capture_queue();
  assert(!zflow_mac_capture_poll(&captured));
  assert(enqueue(&pre_capture));
  assert(zflow_mac_capture_poll(&captured));
  assert(captured.kind == ZFLOW_EVENT_TOUCH && captured.contact_count == 1);

  // Each Caps Lock toggle is one flagsChanged with no release report.
  CGEventRef caps = CGEventCreateKeyboardEvent(NULL, ZFLOW_CAPS_LOCK, true);
  assert(caps);
  CGEventSetIntegerValueField(caps, kCGKeyboardEventKeycode, ZFLOW_CAPS_LOCK);
  for (int toggle = 0; toggle < 2; toggle++) {
    CGEventSetFlags(caps, toggle ? 0 : kCGEventFlagMaskAlphaShift);
    assert(event_callback(NULL, kCGEventFlagsChanged, caps, NULL) == NULL);
    assert(zflow_mac_capture_poll(&captured) && captured.pressed);
    assert(captured.kind == ZFLOW_EVENT_KEY && captured.code == ZFLOW_CAPS_LOCK);
    assert(zflow_mac_capture_poll(&captured) && !captured.pressed);
    assert(captured.kind == ZFLOW_EVENT_KEY && captured.code == ZFLOW_CAPS_LOCK);
  }
  assert(!zflow_mac_capture_poll(&captured));
  CFRelease(caps);
}

static CGError record(char call) {
  assert(call_count + 1 < sizeof(calls));
  calls[call_count++] = call;
  calls[call_count] = '\0';
  return call == fail_call ? kCGErrorFailure : kCGErrorSuccess;
}

static int fake_connection(void) { return 42; }

static CGError fake_property(int source, int target, CFStringRef key,
                             CFTypeRef value) {
  assert(source == 42 && target == 42);
  assert(CFEqual(key, CFSTR("SetsCursorInBackground")));
  bool enabled = value == kCFBooleanTrue;
  CGError error = record(enabled ? 'B' : 'b');
  if (error == kCGErrorSuccess) background = enabled;
  return error;
}

// A fake MultitouchSupport with one built-in (1) and one external (2) device.
static int fake_multitouch;
static int mt_registered, mt_unregistered, mt_started, mt_stopped;

static CFMutableArrayRef fake_mt_list(void) {
  CFMutableArrayRef list = CFArrayCreateMutable(NULL, 0, NULL);
  CFArrayAppendValue(list, (const void *)1);
  CFArrayAppendValue(list, (const void *)2);
  return list;
}

static bool fake_mt_builtin(MTDeviceRef device) { return device == (MTDeviceRef)1; }

static void fake_mt_register(MTDeviceRef device, MTContactCallback callback) {
  assert(device == (MTDeviceRef)2 && callback == contact_callback);
  mt_registered++;
}

static void fake_mt_unregister(MTDeviceRef device, MTContactCallback callback) {
  assert(device == (MTDeviceRef)2 && callback == contact_callback);
  mt_unregistered++;
}

static int fake_mt_start(MTDeviceRef device, int mode) {
  assert(device == (MTDeviceRef)2 && mode == 0);
  mt_started++;
  return 0;
}

static int fake_mt_stop(MTDeviceRef device) {
  assert(device == (MTDeviceRef)2);
  mt_stopped++;
  return 0;
}

// The size lookup is optional: it can be missing or fail.
static bool mt_size_missing;
static int mt_size_status;

static int fake_mt_size(MTDeviceRef device, int32_t *width, int32_t *height) {
  assert(device == (MTDeviceRef)2);
  *width = 16030;
  *height = 11490;
  return mt_size_status;
}

static void *fake_dlopen(const char *path, int mode) {
  assert(strstr(path, "MultitouchSupport") && mode == RTLD_NOW);
  return &fake_multitouch;
}

static int fake_dlclose(void *handle) {
  assert(handle == &fake_multitouch);
  return 0;
}

static void *fake_dlsym(void *handle, const char *symbol) {
  if (handle == &fake_multitouch) {
    if (strcmp(symbol, "MTDeviceCreateList") == 0) return (void *)fake_mt_list;
    if (strcmp(symbol, "MTDeviceIsBuiltIn") == 0) return (void *)fake_mt_builtin;
    if (strcmp(symbol, "MTRegisterContactFrameCallback") == 0) return (void *)fake_mt_register;
    if (strcmp(symbol, "MTUnregisterContactFrameCallback") == 0) return (void *)fake_mt_unregister;
    if (strcmp(symbol, "MTDeviceStart") == 0) return (void *)fake_mt_start;
    if (strcmp(symbol, "MTDeviceGetSensorSurfaceDimensions") == 0)
      return mt_size_missing ? NULL : (void *)fake_mt_size;
    assert(strcmp(symbol, "MTDeviceStop") == 0);
    return (void *)fake_mt_stop;
  }
  assert(handle == RTLD_DEFAULT);
  if (missing_symbol) return NULL;
  if (strcmp(symbol, "_CGSDefaultConnection") == 0)
    return (void *)fake_connection;
  assert(strcmp(symbol, "CGSSetConnectionProperty") == 0);
  return (void *)fake_property;
}

static CGError fake_associate(boolean_t enabled) {
  CGError error = record(enabled ? 'C' : 'D');
  if (error == kCGErrorSuccess) connected = enabled;
  return error;
}

static CGError fake_hide(CGDirectDisplayID display) {
  assert(display == kCGNullDirectDisplay);
  assert(background);
  CGError error = record('H');
  if (error == kCGErrorSuccess) hide_count++;
  return error;
}

static CGError fake_show(CGDirectDisplayID display) {
  assert(display == kCGNullDirectDisplay);
  assert(hide_count == 1);
  CGError error = record('S');
  if (error == kCGErrorSuccess) hide_count--;
  return error;
}

static void idle_port(CFMachPortRef port, void *message, CFIndex size, void *info) {
  (void)port; (void)message; (void)size; (void)info;
}

static CFMachPortRef fake_tap(CGEventTapLocation location,
                             CGEventTapPlacement placement,
                             CGEventTapOptions options, CGEventMask mask,
                             CGEventTapCallBack callback, void *context) {
  if (callback == motion_callback) {
    assert(location == kCGSessionEventTap && placement == kCGTailAppendEventTap);
    assert(options == kCGEventTapOptionListenOnly && context == NULL);
    assert(mask == CGEventMaskBit(kCGEventMouseMoved));
    motion_taps++;
    return tap_available ? CFMachPortCreate(NULL, idle_port, NULL, NULL) : NULL;
  }
  assert(location == kCGHIDEventTap);
  if (callback == pass_event) {
    assert(placement == kCGTailAppendEventTap && options == kCGEventTapOptionDefault);
    assert(mask == CGEventMaskBit(kCGEventOtherMouseDown) && context == NULL);
    probe_calls++;
    return probe_allowed ? CFMachPortCreate(NULL, idle_port, NULL, NULL) : NULL;
  }
  assert(placement == kCGHeadInsertEventTap);
  assert(options == kCGEventTapOptionDefault);
  assert(mask & CGEventMaskBit(kCGEventMouseMoved));
  assert((mask & ZFLOW_SWALLOWED_EVENTS) == ZFLOW_SWALLOWED_EVENTS);
  assert(callback == event_callback && context == NULL);
  tap_calls++;
  return tap_available ? CFMachPortCreate(NULL, idle_port, NULL, NULL) : NULL;
}

static void fake_tap_enable(CFMachPortRef tap, bool enable) {
  assert(tap);
  if (enable) tap_enables++;
}

static CFRunLoopRunResult fake_run_loop(CFRunLoopMode mode, CFTimeInterval seconds,
                                        Boolean once) {
  atomic_fetch_add(&loop_runs, 1);
  // With its tap invalidated, the mode has no sources left to run.
  if (atomic_load(&invalidate_tap)) return kCFRunLoopRunFinished;
  if (lose_stop) {
    // A stop that lands just before the loop runs is dropped by CFRunLoopStop.
    lose_stop = false;
    atomic_store(&g_stop, true);
    stop_capture_run_loop();
  }
  return CFRunLoopRunInMode(mode, seconds, once);
}

static void reset(void) {
  assert(!g_cursor_hidden && !g_cursor_disconnected);
  // A fresh process: the background cursor property is not set yet.
  g_cursor_background = false;
  call_count = 0;
  calls[0] = '\0';
  fail_call = 0;
  missing_symbol = false;
  connected = true;
  background = false;
  hide_count = 0;
  atomic_store(&g_stop, false);
  atomic_store(&g_pause_requested, false);
  atomic_store(&g_raw_contact_active, false);
  g_capture_status = 0;
  g_request_raw_touch = false;
  expect_return_warp = false;
  g_return_pending = false;
  g_queue_head = g_queue_tail = 0;
  memset(g_forwarded_keys, 0, sizeof(g_forwarded_keys));
  memset(g_forwarded_codes, 0, sizeof(g_forwarded_codes));
  memset(g_forwarded_buttons, 0, sizeof(g_forwarded_buttons));
  held_key = held_button = -1;
  tap_available = true;
  probe_allowed = true;
}

static void count_wake(void) { atomic_fetch_add(&wakes, 1); }

static void start_capture(void) {
  assert(zflow_mac_capture_start(0, &entry, count_wake) == 0);
  assert(g_thread_valid && strcmp(calls, "BHD") == 0);
}

static void secure_input_tests(void) {
  // With Secure Event Input the tap sees no keys, so capture must not start.
  reset();
  secure_input = true;
  assert(zflow_mac_secure_input_enabled() == 1);
  assert(zflow_mac_capture_start(0, &entry, NULL) == -2);
  assert(strstr(g_error, "secure keyboard entry"));
  assert(call_count == 0 && !g_thread_valid);
  assert(capture_entry_allowed() == -2);
  secure_input = false;
  assert(capture_entry_allowed() == 0);
  assert(zflow_mac_secure_input_enabled() == 0);
}

static void admission_tests(void) {
  // Capture always needs a crossing edge to check once its tap is installed.
  reset();
  int taps = tap_calls;
  assert(zflow_mac_capture_start(0, NULL, NULL) == -1);
  assert(tap_calls == taps && !g_thread_valid);
  reset();
  held_button = 0;
  assert(zflow_mac_capture_start(0, &entry, NULL) == -2);
  assert(strstr(g_error, "release held buttons"));
  assert(tap_calls == taps + 1 && call_count == 0 && !g_thread_valid);
  held_button = -1;
  cursor_position.x = -1150;
  assert(zflow_mac_capture_start(0, &entry, NULL) == -2);
  assert(strstr(g_error, "left the configured crossing edge"));
  cursor_position.x = -1200;
}

static void wake_tests(void) {
  // Each queued event and the end of capture wake the consumer.
  reset();
  atomic_store(&wakes, 0);
  start_capture();
  int started = atomic_load(&wakes);
  ZFlowMacEvent motion = {.kind = ZFLOW_EVENT_MOTION};
  assert(enqueue(&motion));
  assert(atomic_load(&wakes) == started + 1);
  assert(zflow_mac_capture_stop() == 0);
  assert(atomic_load(&wakes) == started + 2);
  g_wake = NULL;
}

static void invalidated_tap_tests(void) {
  // macOS invalidating the tap ends the crossing as a failure, never a pause.
  reset();
  start_capture();
  atomic_store(&invalidate_tap, true);
  while (!zflow_mac_capture_stop_requested()) sched_yield();
  atomic_store(&invalidate_tap, false);
  assert(zflow_mac_capture_stop() == -1);
  assert(strstr(g_error, "macOS stopped input capture"));
  assert(zflow_mac_capture_pause_requested() == 0);
  assert(strcmp(calls, "BHDSC") == 0);
  g_wake = NULL;
}

static void lost_stop_tests(void) {
  reset();
  lose_stop = true;
  atomic_store(&loop_runs, 0);
  start_capture();
  while (atomic_load(&loop_runs) == 0) sched_yield();
  // The join returns only if the loop rechecks the flag it missed.
  assert(zflow_mac_capture_stop() == 0);
  assert(!g_thread_valid && !lose_stop);
  assert(strcmp(calls, "BHDSC") == 0);
}

static void return_cursor_tests(void) {
  const ZFlowMacPosition target = {-1200, -50};
  reset();
  expect_return_warp = true;
  start_capture();
  assert(zflow_mac_capture_stop_at(&target) == 0);
  assert(strcmp(calls, "BHDWSC") == 0);
  assert(warped_position.x == target.x && warped_position.y == target.y);
  assert(!g_return_pending && !g_thread_valid);
  assert(zflow_mac_capture_stop() == 0);
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDWSC") == 0);

  reset();
  start_capture();
  atomic_store(&g_stop, true);
  stop_capture_run_loop();
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDSC") == 0);
  assert(connected && hide_count == 0 && background);

  const ZFlowMacPosition invalid[] = {{NAN, 0}, {0, INFINITY},
                                    {1920, 0}, {-1200, -201}};
  for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
    reset();
    start_capture();
    assert(zflow_mac_capture_stop_at(&invalid[i]) == -1);
    assert(strcmp(calls, "BHDSC") == 0);
    assert(connected && hide_count == 0 && background);
  }

  reset();
  expect_return_warp = true;
  start_capture();
  fail_call = 'W';
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDWSC") == 0);
  assert(connected && hide_count == 0 && background);

  reset();
  start_capture();
  assert(zflow_mac_capture_stop() == 0);
  assert(strcmp(calls, "BHDSC") == 0);
}

static void cursor_lifecycle_tests(void) {
  for (int cycle = 0; cycle < 3; cycle++) {
    reset();
    assert(capture_cursor());
    assert(!connected && hide_count == 1 && background);
    assert(strcmp(calls, "BHD") == 0);
    assert(release_cursor());
    assert(connected && hide_count == 0 && background);
    assert(strcmp(calls, "BHDSC") == 0);
    assert(release_cursor());
    assert(strcmp(calls, "BHDSC") == 0);
    // The property stays on, so the next capture only hides and disconnects.
    assert(capture_cursor());
    assert(strcmp(calls, "BHDSCHD") == 0);
    assert(release_cursor());
  }

  const char failures[] = {'B', 'H', 'D'};
  const char *expected[] = {"B", "BH", "BHDSC"};
  for (size_t i = 0; i < sizeof(failures); i++) {
    reset();
    fail_call = failures[i];
    assert(!capture_cursor());
    assert(strstr(g_error, "CGError"));
    assert(release_cursor());
    assert(connected && hide_count == 0 && background == (failures[i] != 'B'));
    assert(strcmp(calls, expected[i]) == 0);
  }

  reset();
  missing_symbol = true;
  assert(!capture_cursor());
  assert(release_cursor());
  assert(call_count == 0);

  const char release_failures[] = {'S', 'C'};
  for (size_t i = 0; i < sizeof(release_failures); i++) {
    reset();
    assert(capture_cursor());
    fail_call = release_failures[i];
    assert(!release_cursor());
    assert(strcmp(calls, "BHDSC") == 0);
    fail_call = 0;
    assert(release_cursor());
    assert(connected && hide_count == 0 && background);
  }
}

static void multitouch_tests(void) {
  reset();
  assert(zflow_mac_capture_start(1, &entry, NULL) == 1);
  assert(mt_registered == 1 && mt_started == 1);
  assert(g_raw_devices[0] == (MTDeviceRef)2);
  assert(g_raw_surfaces[0][0] == 16030 && g_raw_surfaces[0][1] == 11490);
  assert(zflow_mac_capture_stop() == 0);
  assert(mt_stopped == 1 && mt_unregistered == 1);

  // Without a size, raw touch still starts and reports the size as unknown.
  mt_size_status = -1;
  reset();
  assert(zflow_mac_capture_start(1, &entry, NULL) == 1);
  assert(g_raw_surfaces[0][0] == 0 && g_raw_surfaces[0][1] == 0);
  assert(zflow_mac_capture_stop() == 0);
  mt_size_status = 0;
  mt_size_missing = true;
  reset();
  assert(zflow_mac_capture_start(1, &entry, NULL) == 1);
  assert(g_raw_surfaces[0][0] == 0 && g_raw_surfaces[0][1] == 0);
  assert(zflow_mac_capture_stop() == 0);
  mt_size_missing = false;
  assert(mt_started == 3 && mt_stopped == 3);
}

static void touch_tests(void) {
  reset();
  g_raw_devices[0] = (MTDeviceRef)2;
  g_raw_surfaces[0][0] = 16030;
  g_raw_surfaces[0][1] = 11490;
  MTTouch touches[2] = {0};
  touches[0].state = 4;
  touches[0].identifier = 1;
  touches[0].normalized.pos = (MTPoint){1.02f, -0.01f};
  touches[1].state = 4;
  touches[1].identifier = 2;
  touches[1].normalized.pos = (MTPoint){0.5f, 0.25f};
  // A contact just past the pad edge keeps the frame, clamped.
  contact_callback((MTDeviceRef)2, touches, 2, 0, 0);
  ZFlowMacEvent captured;
  assert(zflow_mac_capture_poll(&captured) && captured.kind == ZFLOW_EVENT_TOUCH);
  assert(captured.contact_count == 2);
  assert(captured.surface_width == 16030 && captured.surface_height == 11490);
  assert(captured.contacts[0].x == 1.0f && captured.contacts[0].y == 0.0f);
  assert(captured.contacts[1].x == 0.5f && captured.contacts[1].y == 0.25f);
  // A device the bridge never sized reports an unknown size.
  contact_callback((MTDeviceRef)3, touches, 2, 0, 0);
  assert(zflow_mac_capture_poll(&captured));
  assert(captured.surface_width == 0 && captured.surface_height == 0);
  touches[0].normalized.pos.x = NAN;
  contact_callback((MTDeviceRef)2, touches, 2, 0, 0);
  assert(!zflow_mac_capture_poll(&captured));
}

static void event_tests(void) {
  reset();
  CGEventRef event = CGEventCreateMouseEvent(
      NULL, kCGEventMouseMoved, CGPointZero, kCGMouseButtonLeft);
  assert(event);
  CGEventSetIntegerValueField(event, kCGMouseEventDeltaX, 17);
  CGEventSetIntegerValueField(event, kCGMouseEventDeltaY, -9);
  const CGEventType motion[] = {kCGEventMouseMoved, kCGEventLeftMouseDragged,
                               kCGEventRightMouseDragged, kCGEventOtherMouseDragged};
  for (size_t i = 0; i < sizeof(motion) / sizeof(motion[0]); i++) {
    assert(event_callback(NULL, motion[i], event, NULL) == NULL);
    ZFlowMacEvent captured;
    assert(zflow_mac_capture_poll(&captured) == 1);
    assert(captured.kind == ZFLOW_EVENT_MOTION);
    assert(captured.dx == 17 && captured.dy == -9);
  }
  // Up is positive on both sides; CoreGraphics counts left and the wire right.
  CGEventRef scroll = CGEventCreateScrollWheelEvent2(
      NULL, kCGScrollEventUnitPixel, 2, -40, 7, 0);
  assert(scroll);
  assert(event_callback(NULL, kCGEventScrollWheel, scroll, NULL) == NULL);
  CFRelease(scroll);
  ZFlowMacEvent scrolled;
  assert(zflow_mac_capture_poll(&scrolled) == 1);
  assert(scrolled.kind == ZFLOW_EVENT_MOTION);
  assert(scrolled.scroll_x == -7 && scrolled.scroll_y == -40);
  g_request_raw_touch = true;
  atomic_store(&g_raw_contact_active, true);
  assert(event_callback(NULL, kCGEventMouseMoved, event, NULL) == NULL);
  ZFlowMacEvent captured;
  assert(zflow_mac_capture_poll(&captured) == 0);

  // Media keys and native gestures stay off the Mac while capturing.
  const CGEventType swallowed[] = {NX_SYSDEFINED, 18, 19, 20, 29, 30, 31, 32, 33, 34};
  for (size_t i = 0; i < sizeof(swallowed) / sizeof(swallowed[0]); i++) {
    assert(event_callback(NULL, swallowed[i], event, NULL) == NULL);
  }
  assert(event_callback(NULL, kCGEventTabletPointer, event, NULL) == event);
  assert(zflow_mac_capture_poll(&captured) == 0);

  reset();
  g_event_tap = CFMachPortCreate(NULL, idle_port, NULL, NULL);
  int enables = tap_enables;
  assert(event_callback(NULL, kCGEventTapDisabledByTimeout, event, NULL) == event);
  assert(tap_enables == enables + 1);
  assert(zflow_mac_capture_stop_requested() == 0 && g_capture_status == 0);
  assert(event_callback(NULL, kCGEventMouseMoved, event, NULL) == NULL);
  assert(zflow_mac_capture_poll(&captured) == 1);

  // Releases made while the tap was off reached the Mac. Re-enabling sends
  // them to the other computer for keys and buttons no longer down.
  reset();
  g_forwarded_keys[4] = g_forwarded_keys[55] = true;
  g_forwarded_codes[4] = 4;
  g_forwarded_codes[55] = 55;
  g_forwarded_buttons[1] = g_forwarded_buttons[2] = true;
  held_key = 55;
  held_button = 1;
  assert(event_callback(NULL, kCGEventTapDisabledByTimeout, event, NULL) == event);
  assert(zflow_mac_capture_poll(&captured) == 1);
  assert(captured.kind == ZFLOW_EVENT_KEY && captured.code == 4 && !captured.pressed);
  assert(zflow_mac_capture_poll(&captured) == 1);
  assert(captured.kind == ZFLOW_EVENT_BUTTON && captured.button == 1 && !captured.pressed);
  assert(zflow_mac_capture_poll(&captured) == 0);
  assert(!g_forwarded_keys[4] && g_forwarded_keys[55]);
  assert(!g_forwarded_buttons[1] && g_forwarded_buttons[2]);

  // With Accessibility removed, a re-enabled tap would hold the Mac's input,
  // so a timeout then ends remote control instead.
  reset();
  probe_allowed = false;
  enables = tap_enables;
  int probes = probe_calls;
  assert(event_callback(NULL, kCGEventTapDisabledByTimeout, event, NULL) == event);
  assert(probe_calls == probes + 1 && tap_enables == enables);
  assert(zflow_mac_capture_stop_requested() == 1 && g_capture_status == -1);
  CFRelease(g_event_tap);
  g_event_tap = NULL;

  reset();
  assert(capture_cursor());
  assert(event_callback(NULL, kCGEventTapDisabledByUserInput, event, NULL) == event);
  assert(zflow_mac_capture_stop_requested() == 1);
  assert(zflow_mac_capture_pause_requested() == 0);
  assert(g_capture_status == -1);
  assert(event_callback(NULL, kCGEventMouseMoved, event, NULL) == event);
  assert(zflow_mac_capture_poll(&captured) == 0);
  assert(release_cursor());

  reset();
  CFRelease(event);
  event = CGEventCreateKeyboardEvent(NULL, 51, true);
  assert(event);
  CGEventSetIntegerValueField(event, kCGKeyboardEventKeycode, 51);
  CGEventSetFlags(event, kCGEventFlagMaskControl | kCGEventFlagMaskCommand);
  // The chord must not reach the frontmost Mac app.
  assert(event_callback(NULL, kCGEventKeyDown, event, NULL) == NULL);
  assert(zflow_mac_capture_stop_requested() == 1);
  assert(zflow_mac_capture_pause_requested() == 1);
  assert(zflow_mac_capture_poll(&captured) == 1);
  assert(captured.kind == ZFLOW_EVENT_ESCAPE);

  reset();
  ZFlowMacEvent motion_event = {.kind = ZFLOW_EVENT_MOTION};
  for (size_t i = 0; i < ZFLOW_QUEUE_CAPACITY - 1; i++) {
    assert(enqueue(&motion_event));
  }
  assert(!enqueue(&motion_event));
  assert(event_callback(NULL, kCGEventKeyDown, event, NULL) == NULL);
  assert(zflow_mac_capture_stop_requested() == 1);
  assert(zflow_mac_capture_pause_requested() == 1);
  while (zflow_mac_capture_poll(&captured)) {
    assert(captured.kind == ZFLOW_EVENT_MOTION);
  }
  CFRelease(event);

  // A release that does not fit must end capture instead of vanishing.
  reset();
  for (size_t i = 0; i < ZFLOW_QUEUE_CAPACITY - 1; i++) {
    assert(enqueue(&motion_event));
  }
  event = CGEventCreateKeyboardEvent(NULL, 0, false);
  assert(event);
  g_forwarded_keys[0] = true;
  assert(event_callback(NULL, kCGEventKeyUp, event, NULL) == NULL);
  assert(zflow_mac_capture_stop_requested() == 1);
  assert(zflow_mac_capture_pause_requested() == 0);
  assert(g_capture_status == -1 && strstr(g_error, "overflowed"));
  CFRelease(event);
}

// Sends a key from a keyboard of that type through the tap and returns the
// code the bridge forwarded for it.
static uint16_t tap_key(uint16_t keycode, bool down, int64_t keyboard) {
  CGEventRef event = CGEventCreateKeyboardEvent(NULL, keycode, down);
  assert(event);
  CGEventSetIntegerValueField(event, kCGKeyboardEventKeyboardType, keyboard);
  assert(event_callback(NULL, down ? kCGEventKeyDown : kCGEventKeyUp, event, NULL) == NULL);
  CFRelease(event);
  ZFlowMacEvent captured, extra;
  assert(zflow_mac_capture_poll(&captured));
  assert(captured.kind == ZFLOW_EVENT_KEY && captured.pressed == down);
  assert(!zflow_mac_capture_poll(&extra));
  return captured.code;
}

static void iso_key_tests(void) {
  // macOS swaps 10 and 50 on ISO keyboards only. Swapped back, each code
  // names one position on every layout.
  reset();
  const uint16_t swapped[] = {10, 50};
  const int64_t unswapped[] = {ANSI_KEYBOARD, JIS_KEYBOARD, 0};
  for (size_t i = 0; i < sizeof(unswapped) / sizeof(unswapped[0]); i++) {
    for (size_t k = 0; k < 2; k++) {
      assert(tap_key(swapped[k], true, unswapped[i]) == swapped[k]);
      assert(tap_key(swapped[k], false, unswapped[i]) == swapped[k]);
    }
  }
  for (size_t k = 0; k < 2; k++) {
    assert(tap_key(swapped[k], true, ISO_KEYBOARD) == 60 - swapped[k]);
    assert(tap_key(swapped[k], false, ISO_KEYBOARD) == 60 - swapped[k]);
  }
  assert(tap_key(42, true, ISO_KEYBOARD) == 42);
  assert(tap_key(42, false, ISO_KEYBOARD) == 42);

  // A repeat or release from after a keyboard type change keeps the code the
  // press chose.
  assert(tap_key(10, true, ISO_KEYBOARD) == 50);
  assert(tap_key(10, true, ANSI_KEYBOARD) == 50);
  assert(tap_key(10, false, ANSI_KEYBOARD) == 50);
  assert(tap_key(50, true, ANSI_KEYBOARD) == 50);
  assert(tap_key(50, false, ISO_KEYBOARD) == 50);

  // A release lost while the tap was off has no event to read the type from.
  // Sending the raw code would release a key the other computer never got.
  assert(tap_key(10, true, ISO_KEYBOARD) == 50);
  assert(tap_key(50, true, ISO_KEYBOARD) == 10);
  g_event_tap = CFMachPortCreate(NULL, idle_port, NULL, NULL);
  CGEventRef timeout = CGEventCreate(NULL);
  assert(timeout);
  assert(event_callback(NULL, kCGEventTapDisabledByTimeout, timeout, NULL) == timeout);
  ZFlowMacEvent captured;
  assert(zflow_mac_capture_poll(&captured));
  assert(captured.kind == ZFLOW_EVENT_KEY && captured.code == 50 && !captured.pressed);
  assert(zflow_mac_capture_poll(&captured));
  assert(captured.kind == ZFLOW_EVENT_KEY && captured.code == 10 && !captured.pressed);
  assert(!zflow_mac_capture_poll(&captured));
  assert(!g_forwarded_keys[10] && !g_forwarded_keys[50]);
  CFRelease(timeout);
  CFRelease(g_event_tap);
  g_event_tap = NULL;
}

static CGEventRef move_by(int64_t dx, int64_t dy) {
  CGEventRef move = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, CGPointMake(0, 500),
                                            kCGMouseButtonLeft);
  assert(move);
  CGEventSetIntegerValueField(move, kCGMouseEventDeltaX, dx);
  CGEventSetIntegerValueField(move, kCGMouseEventDeltaY, dy);
  // The HID system's own events carry no process.
  CGEventSetIntegerValueField(move, kCGEventSourceUnixProcessID, 0);
  return move;
}

static void wait_for_motion_watch_to_end(void) {
  CFMachPortInvalidate(g_motion_tap);
  while (atomic_load(&g_motion_watching)) sched_yield();
  assert(g_motion_tap == NULL);
}

static void motion_tests(void) {
  ZFlowMacMotion motion;
  assert(!zflow_mac_motion_take(&motion) && !zflow_mac_motion_take(NULL));
  // The Mac's own moves add up until the next take.
  CGEventRef move = move_by(-2, 1);
  assert(motion_callback(NULL, kCGEventMouseMoved, move, NULL) == move);
  assert(motion_callback(NULL, kCGEventMouseMoved, move, NULL) == move);
  // A peer's moves, which inject.c marks, are not the Mac's own.
  CGEventRef posted = move_by(-100, 0);
  CGEventSetIntegerValueField(posted, kCGEventSourceUserData, ZFLOW_POSTED_MARK);
  assert(motion_callback(NULL, kCGEventMouseMoved, posted, NULL) == posted);
  assert(motion_callback(NULL, kCGEventTapDisabledByTimeout, move, NULL) == move);
  assert(zflow_mac_motion_take(&motion) == 1);
  assert(motion.dx == -4.0 && motion.dy == 2.0 && motion.age_ns < 1000000000ULL);
  assert(!zflow_mac_motion_take(&motion));
  CGEventRef right = move_by(3, 0);
  assert(motion_callback(NULL, kCGEventMouseMoved, right, NULL) == right);
  assert(zflow_mac_motion_take(&motion) == 1 && motion.dx == 3.0 && motion.dy == 0.0);
  assert(motion_callback(NULL, kCGEventMouseMoved, posted, NULL) == posted);
  assert(!zflow_mac_motion_take(&motion));
  // One this process posted is a peer's even if it lost the mark.
  CGEventRef unmarked = move_by(50, 0);
  CGEventSetIntegerValueField(unmarked, kCGEventSourceUnixProcessID, getpid());
  assert(motion_callback(NULL, kCGEventMouseMoved, unmarked, NULL) == unmarked);
  assert(!zflow_mac_motion_take(&motion));
  CFRelease(move);
  CFRelease(posted);
  CFRelease(right);
  CFRelease(unmarked);

  // One listen-only tap on its own thread; a refusal starts nothing.
  tap_available = false;
  assert(zflow_mac_motion_watch() == -1 && !atomic_load(&g_motion_watching));
  assert(motion_taps == 1 && g_motion_tap == NULL);
  tap_available = true;
  assert(zflow_mac_motion_watch() == 0 && zflow_mac_motion_watch() == 0);
  assert(motion_taps == 2);
  // macOS invalidates the tap after sleep; the next watch starts over.
  wait_for_motion_watch_to_end();
  assert(zflow_mac_motion_watch() == 0 && motion_taps == 3);
  wait_for_motion_watch_to_end();
}

int main(void) {
  // A capture thread that misses its stop hangs the join. Fail instead.
  alarm(30);
  desktop_and_permission_tests();
  startup_release_tests();
  cursor_lifecycle_tests();
  secure_input_tests();
  admission_tests();
  lost_stop_tests();
  return_cursor_tests();
  wake_tests();
  invalidated_tap_tests();
  multitouch_tests();
  touch_tests();
  event_tests();
  iso_key_tests();
  reset();
  tap_available = false;
  int taps = tap_calls;
  atomic_store(&g_pause_requested, true);
  assert(zflow_mac_capture_start(0, &entry, NULL) == -1);
  assert(zflow_mac_capture_pause_requested() == 0);
  assert(tap_calls == taps + 1 && call_count == 0);
  assert(!g_thread_valid);
  // Last: its thread runs the faked run loop, which other tests count.
  motion_tests();
  puts("macOS cursor lifecycle and event-filter tests passed (fake cursor APIs)");
  return 0;
}
