#include <ApplicationServices/ApplicationServices.h>
#include <assert.h>
#include <dlfcn.h>
#include <float.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

static void *fake_dlsym(void *, const char *);
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
static Boolean fake_trusted_options(CFDictionaryRef);
static CFMachPortRef fake_tap(CGEventTapLocation, CGEventTapPlacement,
                             CGEventTapOptions, CGEventMask,
                             CGEventTapCallBack, void *);

// Replace cursor mutations and tap creation before including the bridge.
// These tests do not capture, hide, disconnect, or post input on the host.
#define dlsym fake_dlsym
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
#include "../src/macos/capture_bridge.c"

static char calls[64];
static size_t call_count;
static char fail_call;
static bool missing_symbol;
static bool connected;
static bool background;
static int hide_count;
static int tap_calls;
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
  assert(zflow_mac_warp_cursor(position) == 0);
  assert(warped_position.x == -1200 && warped_position.y == -50);
  assert(zflow_mac_warp_cursor((ZFlowMacPosition){NAN, 0}) == -1);
  assert(zflow_mac_input_is_neutral());
  held_key = 55; assert(!zflow_mac_input_is_neutral()); held_key = -1;
  held_button = 0; assert(!zflow_mac_input_is_neutral()); held_button = -1;
  held_flags = kCGEventFlagMaskCommand; assert(!zflow_mac_input_is_neutral());
  held_flags = kCGEventFlagMaskAlphaShift; assert(zflow_mac_input_is_neutral());
  held_flags = 0;
  g_check_entry = true;
  g_entry_region = (ZFlowMacRect){-1200, -100, 9, 200};
  assert(capture_entry_allowed());
  held_key = 10; assert(!capture_entry_allowed()); held_key = -1;
  held_button = 1; assert(!capture_entry_allowed()); held_button = -1;
  cursor_position.y -= 11; assert(capture_entry_allowed());
  cursor_position.y = 50; assert(capture_entry_allowed());
  cursor_position.x = -1191; assert(!capture_entry_allowed());
  assert(strstr(g_error, "left the configured crossing edge"));
  cursor_position.x = -1200;
  cursor_position.y = 100; assert(!capture_entry_allowed());
  cursor_position.y = -100; assert(capture_entry_allowed());
  cursor_position.y = -100.1; assert(!capture_entry_allowed());

  g_entry_region = (ZFlowMacRect){-1250, -50, 100, 9};
  cursor_position = CGPointMake(-1200, -50); assert(capture_entry_allowed());
  cursor_position.x += 11; assert(capture_entry_allowed());
  cursor_position.x = -1150; assert(!capture_entry_allowed());
  cursor_position = CGPointMake(-1200, -41); assert(!capture_entry_allowed());

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
    assert(!capture_entry_allowed());
    assert(strstr(g_error, "configured crossing edge is invalid"));
  }
  g_entry_region = (ZFlowMacRect){-1200, -100, 9, 200};
  cursor_position.x = NAN; assert(!capture_entry_allowed());
  cursor_position = CGPointMake(-1200, -50);
  g_check_entry = false;
  assert(!zflow_mac_accessibility_authorized(0));
  assert(permission_prompts == 0);
  assert(!zflow_mac_accessibility_authorized(1));
  assert(permission_prompts == 1);
}

static void startup_release_tests(void) {
  g_check_entry = true;
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
  g_check_entry = false;
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

static void *fake_dlsym(void *handle, const char *symbol) {
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

static CFMachPortRef fake_tap(CGEventTapLocation location,
                             CGEventTapPlacement placement,
                             CGEventTapOptions options, CGEventMask mask,
                             CGEventTapCallBack callback, void *context) {
  assert(location == kCGHIDEventTap);
  assert(placement == kCGHeadInsertEventTap);
  assert(options == kCGEventTapOptionDefault);
  assert(mask & CGEventMaskBit(kCGEventMouseMoved));
  assert(callback == event_callback && context == NULL);
  tap_calls++;
  return NULL;
}

static void reset(void) {
  assert(!g_cursor_hidden && !g_cursor_disconnected && !g_cursor_background);
  call_count = 0;
  calls[0] = '\0';
  fail_call = 0;
  missing_symbol = false;
  connected = true;
  background = false;
  hide_count = 0;
  atomic_store(&g_stop, false);
  atomic_store(&g_raw_contact_active, false);
  g_capture_status = 0;
  g_request_raw_touch = false;
  expect_return_warp = false;
  g_return_pending = false;
  g_queue_head = g_queue_tail = 0;
}

static void idle_test_source(void *context) { (void)context; }

static void *fake_capture_cleanup(void *context) {
  (void)context;
  CFRunLoopSourceContext source_context = {0};
  source_context.perform = idle_test_source;
  CFRunLoopSourceRef source = CFRunLoopSourceCreate(NULL, 0, &source_context);
  assert(source);
  CFRunLoopRef loop = CFRunLoopGetCurrent();
  CFRunLoopAddSource(loop, source, kCFRunLoopDefaultMode);
  pthread_mutex_lock(&g_run_loop_lock);
  g_run_loop = loop;
  pthread_mutex_unlock(&g_run_loop_lock);
  signal_started(0);
  if (!atomic_load(&g_stop)) CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1, false);
  ZFlowMacPosition position;
  bool returning = take_return_position(&position);
  if (!release_cursor_at(returning ? &position : NULL)) g_capture_status = -1;
  CFRunLoopRemoveSource(loop, source, kCFRunLoopDefaultMode);
  CFRelease(source);
  atomic_store(&g_stop, true);
  return NULL;
}

static void start_fake_capture_cleanup(void) {
  assert(capture_cursor());
  g_start_ready = false;
  assert(pthread_create(&g_thread, NULL, fake_capture_cleanup, NULL) == 0);
  g_thread_valid = true;
  pthread_mutex_lock(&g_start_lock);
  while (!g_start_ready) pthread_cond_wait(&g_start_condition, &g_start_lock);
  pthread_mutex_unlock(&g_start_lock);
}

static void return_cursor_tests(void) {
  const ZFlowMacPosition target = {-1200, -50};
  reset();
  expect_return_warp = true;
  start_fake_capture_cleanup();
  assert(zflow_mac_capture_stop_at(&target) == 0);
  assert(strcmp(calls, "BHDWCSb") == 0);
  assert(warped_position.x == target.x && warped_position.y == target.y);
  assert(!g_return_pending && !g_thread_valid);
  assert(zflow_mac_capture_stop() == 0);
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDWCSb") == 0);

  reset();
  start_fake_capture_cleanup();
  atomic_store(&g_stop, true);
  stop_capture_run_loop();
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDCSb") == 0);
  assert(connected && hide_count == 0 && !background);

  const ZFlowMacPosition invalid[] = {{NAN, 0}, {0, INFINITY},
                                    {1920, 0}, {-1200, -201}};
  for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
    reset();
    start_fake_capture_cleanup();
    assert(zflow_mac_capture_stop_at(&invalid[i]) == -1);
    assert(strcmp(calls, "BHDCSb") == 0);
    assert(connected && hide_count == 0 && !background);
  }

  reset();
  expect_return_warp = true;
  start_fake_capture_cleanup();
  fail_call = 'W';
  assert(zflow_mac_capture_stop_at(&target) == -1);
  assert(strcmp(calls, "BHDWCSb") == 0);
  assert(connected && hide_count == 0 && !background);

  reset();
  start_fake_capture_cleanup();
  assert(zflow_mac_capture_stop() == 0);
  assert(strcmp(calls, "BHDCSb") == 0);
}

static void cursor_lifecycle_tests(void) {
  for (int cycle = 0; cycle < 3; cycle++) {
    reset();
    assert(capture_cursor());
    assert(!connected && hide_count == 1 && background);
    assert(strcmp(calls, "BHD") == 0);
    assert(release_cursor());
    assert(connected && hide_count == 0 && !background);
    assert(strcmp(calls, "BHDCSb") == 0);
    assert(release_cursor());
    assert(strcmp(calls, "BHDCSb") == 0);
  }

  const char failures[] = {'B', 'H', 'D'};
  const char *expected[] = {"B", "BHb", "BHDCSb"};
  for (size_t i = 0; i < sizeof(failures); i++) {
    reset();
    fail_call = failures[i];
    assert(!capture_cursor());
    assert(strstr(g_error, "CGError"));
    assert(release_cursor());
    assert(connected && hide_count == 0 && !background);
    assert(strcmp(calls, expected[i]) == 0);
  }

  reset();
  missing_symbol = true;
  assert(!capture_cursor());
  assert(release_cursor());
  assert(call_count == 0);

  const char release_failures[] = {'C', 'S', 'b'};
  for (size_t i = 0; i < sizeof(release_failures); i++) {
    reset();
    assert(capture_cursor());
    fail_call = release_failures[i];
    assert(!release_cursor());
    assert(strcmp(calls, "BHDCSb") == 0);
    fail_call = 0;
    assert(release_cursor());
    assert(connected && hide_count == 0 && !background);
  }
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
  g_request_raw_touch = true;
  atomic_store(&g_raw_contact_active, true);
  assert(event_callback(NULL, kCGEventMouseMoved, event, NULL) == NULL);
  ZFlowMacEvent captured;
  assert(zflow_mac_capture_poll(&captured) == 0);

  const CGEventType disabled[] = {kCGEventTapDisabledByTimeout,
                                  kCGEventTapDisabledByUserInput};
  for (size_t i = 0; i < sizeof(disabled) / sizeof(disabled[0]); i++) {
    reset();
    assert(capture_cursor());
    assert(event_callback(NULL, disabled[i], event, NULL) == event);
    assert(zflow_mac_capture_stop_requested() == 1);
    assert(g_capture_status == -1);
    assert(event_callback(NULL, kCGEventMouseMoved, event, NULL) == event);
    assert(zflow_mac_capture_poll(&captured) == 0);
    assert(release_cursor());
  }

  reset();
  CFRelease(event);
  event = CGEventCreateKeyboardEvent(NULL, 51, true);
  assert(event);
  CGEventSetIntegerValueField(event, kCGKeyboardEventKeycode, 51);
  CGEventSetFlags(event, kCGEventFlagMaskControl | kCGEventFlagMaskCommand);
  event_callback(NULL, kCGEventKeyDown, event, NULL);
  assert(zflow_mac_capture_stop_requested() == 1);
  assert(zflow_mac_capture_poll(&captured) == 1);
  assert(captured.kind == ZFLOW_EVENT_ESCAPE);
  CFRelease(event);
}

int main(void) {
  desktop_and_permission_tests();
  startup_release_tests();
  cursor_lifecycle_tests();
  return_cursor_tests();
  event_tests();
  reset();
  assert(zflow_mac_capture_start(0, NULL) == -1);
  assert(tap_calls == 1 && call_count == 0);
  assert(!g_thread_valid);
  puts("macOS cursor lifecycle and event-filter tests passed (fake cursor APIs)");
  return 0;
}
