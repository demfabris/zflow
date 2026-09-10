#include <ApplicationServices/ApplicationServices.h>
#include <assert.h>
#include <dlfcn.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

static void *fake_dlsym(void *, const char *);
static CGError fake_associate(boolean_t);
static CGError fake_hide(CGDirectDisplayID);
static CGError fake_show(CGDirectDisplayID);
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
#include "../src/macos/capture_bridge.c"

static char calls[64];
static size_t call_count;
static char fail_call;
static bool missing_symbol;
static bool connected;
static bool background;
static int hide_count;
static int tap_calls;

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
  g_queue_head = g_queue_tail = 0;
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
  cursor_lifecycle_tests();
  event_tests();
  reset();
  assert(zflow_mac_capture_start(0) == -1);
  assert(tap_calls == 1 && call_count == 0);
  assert(!g_thread_valid);
  puts("macOS cursor lifecycle and event-filter tests passed (fake cursor APIs)");
  return 0;
}
