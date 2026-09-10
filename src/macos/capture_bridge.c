#include <ApplicationServices/ApplicationServices.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define ZFLOW_QUEUE_CAPACITY 1024
#define ZFLOW_MAX_CONTACTS 5

typedef struct { float x, y; } MTPoint;
typedef struct { MTPoint pos, vel; } MTReadout;
typedef struct {
  int frame;
  double timestamp;
  int identifier;
  int state;
  int finger_id;
  int hand_id;
  MTReadout normalized;
  float size;
  int zero1;
  float angle;
  float major_axis;
  float minor_axis;
  MTReadout absolute_vector;
  int zero2[2];
  float z_density;
} MTTouch;

typedef void *MTDeviceRef;
typedef int (*MTContactCallback)(MTDeviceRef, MTTouch *, int, double, int);

typedef struct {
  int32_t id;
  float x;
  float y;
} ZFlowMacContact;

typedef struct {
  uint32_t kind;
  int64_t dx;
  int64_t dy;
  int64_t scroll_x;
  int64_t scroll_y;
  uint16_t code;
  uint16_t button;
  uint8_t pressed;
  uint8_t contact_count;
  uint8_t padding[2];
  ZFlowMacContact contacts[ZFLOW_MAX_CONTACTS];
} ZFlowMacEvent;

enum {
  ZFLOW_EVENT_MOTION = 1,
  ZFLOW_EVENT_KEY = 2,
  ZFLOW_EVENT_BUTTON = 3,
  ZFLOW_EVENT_TOUCH = 4,
  ZFLOW_EVENT_ESCAPE = 5,
};

static ZFlowMacEvent g_queue[ZFLOW_QUEUE_CAPACITY];
static size_t g_queue_head;
static size_t g_queue_tail;
static pthread_mutex_t g_queue_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t g_run_loop_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t g_start_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_start_condition = PTHREAD_COND_INITIALIZER;
static pthread_t g_thread;
static bool g_thread_valid;
static bool g_start_ready;
static int g_start_status;
static bool g_request_raw_touch;
static _Atomic bool g_stop;
static _Atomic bool g_raw_contact_active;
static int g_capture_status;
static char g_error[256] = "no diagnostic";
static CFRunLoopRef g_run_loop;
static CFMachPortRef g_event_tap;
static bool g_cursor_hidden;
static bool g_cursor_disconnected;
static bool g_cursor_background;
static int g_cursor_connection;
static CGError (*g_set_connection_property)(int, int, CFStringRef, CFTypeRef);

static void *g_multitouch;
static CFMutableArrayRef g_device_list;
static MTDeviceRef g_raw_devices[8];
static size_t g_raw_device_count;
static CFMutableArrayRef (*MTDeviceCreateList)(void);
static void (*MTRegisterContactFrameCallback)(MTDeviceRef, MTContactCallback);
static void (*MTUnregisterContactFrameCallback)(MTDeviceRef, MTContactCallback);
static int (*MTDeviceStart)(MTDeviceRef, int);
static int (*MTDeviceStop)(MTDeviceRef);
static bool (*MTDeviceIsBuiltIn)(MTDeviceRef);

static void set_error(const char *message) {
  snprintf(g_error, sizeof(g_error), "%s", message ? message : "unknown error");
}

static bool cursor_result(CGError error, const char *operation) {
  if (error == kCGErrorSuccess) return true;
  snprintf(g_error, sizeof(g_error), "%s (CGError %d)", operation, error);
  return false;
}

static bool release_cursor(void) {
  bool ok = true;
  if (g_cursor_disconnected) {
    if (cursor_result(CGAssociateMouseAndMouseCursorPosition(true),
                      "could not reconnect the Mac cursor"))
      g_cursor_disconnected = false;
    else
      ok = false;
  }
  if (g_cursor_hidden) {
    if (cursor_result(CGDisplayShowCursor(kCGNullDirectDisplay),
                      "could not show the Mac cursor"))
      g_cursor_hidden = false;
    else
      ok = false;
  }
  if (g_cursor_background) {
    if (cursor_result(g_set_connection_property(
            g_cursor_connection, g_cursor_connection,
            CFSTR("SetsCursorInBackground"), kCFBooleanFalse),
            "could not release background cursor control"))
      g_cursor_background = false;
    else
      ok = false;
  }
  return ok;
}

static bool capture_cursor(void) {
  if (g_cursor_hidden || g_cursor_disconnected || g_cursor_background) {
    set_error("previous capture did not release cursor control");
    return false;
  }
  // The CLI needs the same background visibility property used by Deskflow.
  // Resolve the private API at runtime and refuse capture if it is unavailable.
  int (*default_connection)(void) = dlsym(RTLD_DEFAULT, "_CGSDefaultConnection");
  g_set_connection_property = dlsym(RTLD_DEFAULT, "CGSSetConnectionProperty");
  if (!default_connection || !g_set_connection_property) {
    set_error("macOS background cursor control is unavailable");
    return false;
  }
  g_cursor_connection = default_connection();
  if (!cursor_result(g_set_connection_property(
          g_cursor_connection, g_cursor_connection,
          CFSTR("SetsCursorInBackground"), kCFBooleanTrue),
          "could not enable background cursor control")) return false;
  g_cursor_background = true;
  if (!cursor_result(CGDisplayHideCursor(kCGNullDirectDisplay),
                     "could not hide the Mac cursor")) return false;
  g_cursor_hidden = true;
  // Keep relative deltas while freezing the local cursor. Do not recenter it.
  g_cursor_disconnected = true;
  return cursor_result(CGAssociateMouseAndMouseCursorPosition(false),
                       "could not disconnect the Mac cursor");
}

static bool enqueue(const ZFlowMacEvent *event) {
  pthread_mutex_lock(&g_queue_lock);
  size_t next = (g_queue_head + 1) % ZFLOW_QUEUE_CAPACITY;
  if (next == g_queue_tail) {
    pthread_mutex_unlock(&g_queue_lock);
    return false;
  }
  g_queue[g_queue_head] = *event;
  g_queue_head = next;
  pthread_mutex_unlock(&g_queue_lock);
  return true;
}

static int contact_callback(MTDeviceRef device, MTTouch *touches, int count,
                            double timestamp, int frame) {
  (void)device;
  (void)timestamp;
  (void)frame;
  ZFlowMacEvent event = {0};
  event.kind = ZFLOW_EVENT_TOUCH;
  for (int i = 0; i < count && event.contact_count < ZFLOW_MAX_CONTACTS; i++) {
    if (touches[i].state != 3 && touches[i].state != 4) continue;

    atomic_store(&g_raw_contact_active, true);

    float x = touches[i].normalized.pos.x;
    float y = touches[i].normalized.pos.y;
    if (!isfinite(x) || !isfinite(y) || x < 0.0f || x > 1.0f || y < 0.0f ||
        y > 1.0f) {
      return 0;
    }

    ZFlowMacContact *contact = &event.contacts[event.contact_count++];
    contact->id = touches[i].identifier;
    contact->x = x;
    contact->y = y;
  }
  atomic_store(&g_raw_contact_active, event.contact_count > 0);
  enqueue(&event);
  return 0;
}

static uint8_t side_modifier_pressed(CGEventFlags flags,
                                     CGEventFlags aggregate_mask,
                                     CGEventFlags side_mask,
                                     CGEventFlags left_mask,
                                     CGEventFlags right_mask) {
  CGEventFlags device_mask = left_mask | right_mask;
  if ((flags & device_mask) != 0) return (flags & side_mask) != 0;
  return (flags & aggregate_mask) != 0;
}

uint8_t zflow_mac_modifier_pressed(uint16_t keycode, uint64_t raw_flags) {
  CGEventFlags flags = (CGEventFlags)raw_flags;
  switch (keycode) {
    case 54: return side_modifier_pressed(
        flags, kCGEventFlagMaskCommand, NX_DEVICERCMDKEYMASK,
        NX_DEVICELCMDKEYMASK, NX_DEVICERCMDKEYMASK);
    case 55: return side_modifier_pressed(
        flags, kCGEventFlagMaskCommand, NX_DEVICELCMDKEYMASK,
        NX_DEVICELCMDKEYMASK, NX_DEVICERCMDKEYMASK);
    case 56: return side_modifier_pressed(
        flags, kCGEventFlagMaskShift, NX_DEVICELSHIFTKEYMASK,
        NX_DEVICELSHIFTKEYMASK, NX_DEVICERSHIFTKEYMASK);
    case 60: return side_modifier_pressed(
        flags, kCGEventFlagMaskShift, NX_DEVICERSHIFTKEYMASK,
        NX_DEVICELSHIFTKEYMASK, NX_DEVICERSHIFTKEYMASK);
    case 58: return side_modifier_pressed(
        flags, kCGEventFlagMaskAlternate, NX_DEVICELALTKEYMASK,
        NX_DEVICELALTKEYMASK, NX_DEVICERALTKEYMASK);
    case 61: return side_modifier_pressed(
        flags, kCGEventFlagMaskAlternate, NX_DEVICERALTKEYMASK,
        NX_DEVICELALTKEYMASK, NX_DEVICERALTKEYMASK);
    case 59: return side_modifier_pressed(
        flags, kCGEventFlagMaskControl, NX_DEVICELCTLKEYMASK,
        NX_DEVICELCTLKEYMASK, NX_DEVICERCTLKEYMASK);
    case 62: return side_modifier_pressed(
        flags, kCGEventFlagMaskControl, NX_DEVICERCTLKEYMASK,
        NX_DEVICELCTLKEYMASK, NX_DEVICERCTLKEYMASK);
    case 57: return (flags & kCGEventFlagMaskAlphaShift) != 0;
    default: return 0;
  }
}

static void stop_capture_run_loop(void) {
  pthread_mutex_lock(&g_run_loop_lock);
  if (g_run_loop) CFRunLoopStop(g_run_loop);
  pthread_mutex_unlock(&g_run_loop_lock);
}

uint8_t zflow_mac_should_forward_scroll(uint8_t raw_touch,
                                        uint8_t raw_contact_active,
                                        int64_t scroll_phase,
                                        int64_t momentum_phase) {
  // macOS can keep sending gesture scroll after the raw contacts lift.
  return !raw_touch ||
      (!raw_contact_active && scroll_phase == 0 && momentum_phase == 0);
}

static CGEventRef event_callback(CGEventTapProxy proxy, CGEventType type,
                                 CGEventRef event, void *context) {
  (void)proxy;
  (void)context;
  if (type == kCGEventTapDisabledByTimeout ||
      type == kCGEventTapDisabledByUserInput) {
    set_error("macOS disabled input capture; ending remote control");
    g_capture_status = -1;
    atomic_store(&g_stop, true);
    stop_capture_run_loop();
    return event;
  }
  if (atomic_load(&g_stop)) return event;

  ZFlowMacEvent captured = {0};
  switch (type) {
    case kCGEventKeyDown:
    case kCGEventKeyUp:
    case kCGEventFlagsChanged: {
      uint16_t keycode = (uint16_t)CGEventGetIntegerValueField(
          event, kCGKeyboardEventKeycode);
      CGEventFlags flags = CGEventGetFlags(event);
      if (type == kCGEventKeyDown && keycode == 51 &&
          (flags & kCGEventFlagMaskControl) &&
          (flags & kCGEventFlagMaskCommand)) {
        captured.kind = ZFLOW_EVENT_ESCAPE;
        enqueue(&captured);
        atomic_store(&g_stop, true);
        stop_capture_run_loop();
        return event;
      }
      captured.kind = ZFLOW_EVENT_KEY;
      captured.code = keycode;
      captured.pressed = type == kCGEventFlagsChanged
          ? zflow_mac_modifier_pressed(keycode, flags)
          : type == kCGEventKeyDown;
      enqueue(&captured);
      return NULL;
    }
    case kCGEventLeftMouseDown:
    case kCGEventRightMouseDown:
    case kCGEventOtherMouseDown:
    case kCGEventLeftMouseUp:
    case kCGEventRightMouseUp:
    case kCGEventOtherMouseUp:
      captured.kind = ZFLOW_EVENT_BUTTON;
      captured.button = (uint16_t)CGEventGetIntegerValueField(
          event, kCGMouseEventButtonNumber) + 1;
      captured.pressed = type == kCGEventLeftMouseDown ||
                         type == kCGEventRightMouseDown ||
                         type == kCGEventOtherMouseDown;
      enqueue(&captured);
      return NULL;
    case kCGEventMouseMoved:
    case kCGEventLeftMouseDragged:
    case kCGEventRightMouseDragged:
    case kCGEventOtherMouseDragged:
      if (!g_request_raw_touch || !atomic_load(&g_raw_contact_active)) {
        captured.kind = ZFLOW_EVENT_MOTION;
        captured.dx = CGEventGetIntegerValueField(event, kCGMouseEventDeltaX);
        captured.dy = CGEventGetIntegerValueField(event, kCGMouseEventDeltaY);
        enqueue(&captured);
      }
      return NULL;
    case kCGEventScrollWheel:
      if (zflow_mac_should_forward_scroll(
              g_request_raw_touch, atomic_load(&g_raw_contact_active),
              CGEventGetIntegerValueField(event, kCGScrollWheelEventScrollPhase),
              CGEventGetIntegerValueField(event, kCGScrollWheelEventMomentumPhase))) {
        captured.kind = ZFLOW_EVENT_MOTION;
        captured.scroll_x = CGEventGetIntegerValueField(
            event, kCGScrollWheelEventPointDeltaAxis2);
        captured.scroll_y = CGEventGetIntegerValueField(
            event, kCGScrollWheelEventPointDeltaAxis1);
        enqueue(&captured);
      }
      return NULL;
    default:
      return event;
  }
}

static void unload_multitouch(void) {
  for (size_t i = 0; i < g_raw_device_count; i++) {
    if (MTUnregisterContactFrameCallback)
      MTUnregisterContactFrameCallback(g_raw_devices[i], contact_callback);
    if (MTDeviceStop) MTDeviceStop(g_raw_devices[i]);
  }
  g_raw_device_count = 0;
  if (g_device_list) CFRelease(g_device_list);
  g_device_list = NULL;
  if (g_multitouch) dlclose(g_multitouch);
  g_multitouch = NULL;
  atomic_store(&g_raw_contact_active, false);
}

static bool load_multitouch(bool start_devices) {
  g_multitouch = dlopen(
      "/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport",
      RTLD_NOW);
  if (!g_multitouch) {
    set_error(dlerror());
    return false;
  }
  MTDeviceCreateList = dlsym(g_multitouch, "MTDeviceCreateList");
  MTRegisterContactFrameCallback =
      dlsym(g_multitouch, "MTRegisterContactFrameCallback");
  MTUnregisterContactFrameCallback =
      dlsym(g_multitouch, "MTUnregisterContactFrameCallback");
  MTDeviceStart = dlsym(g_multitouch, "MTDeviceStart");
  MTDeviceStop = dlsym(g_multitouch, "MTDeviceStop");
  MTDeviceIsBuiltIn = dlsym(g_multitouch, "MTDeviceIsBuiltIn");
  if (!MTDeviceCreateList || !MTRegisterContactFrameCallback ||
      !MTDeviceStart || !MTDeviceIsBuiltIn) {
    set_error("MultitouchSupport is missing required symbols");
    unload_multitouch();
    return false;
  }

  g_device_list = MTDeviceCreateList();
  CFIndex count = g_device_list ? CFArrayGetCount(g_device_list) : 0;
  for (CFIndex i = 0; i < count && g_raw_device_count < 8; i++) {
    MTDeviceRef device = (MTDeviceRef)CFArrayGetValueAtIndex(g_device_list, i);
    if (MTDeviceIsBuiltIn(device)) continue;
    g_raw_devices[g_raw_device_count++] = device;
    if (start_devices) {
      MTRegisterContactFrameCallback(device, contact_callback);
      if (MTDeviceStart(device, 0) != 0) {
        set_error("Magic Trackpad failed to start");
        unload_multitouch();
        return false;
      }
    }
  }
  if (g_raw_device_count == 0) {
    set_error("no external Magic Trackpad was enumerated");
    unload_multitouch();
    return false;
  }
  return true;
}

static void signal_started(int status) {
  pthread_mutex_lock(&g_start_lock);
  g_start_status = status;
  g_start_ready = true;
  pthread_cond_signal(&g_start_condition);
  pthread_mutex_unlock(&g_start_lock);
}

static void *capture_thread(void *context) {
  (void)context;
  bool raw_active = g_request_raw_touch && load_multitouch(true);
  if (!raw_active) g_request_raw_touch = false;

  CGEventMask mask = CGEventMaskBit(kCGEventKeyDown) |
      CGEventMaskBit(kCGEventKeyUp) |
      CGEventMaskBit(kCGEventFlagsChanged) |
      CGEventMaskBit(kCGEventLeftMouseDown) |
      CGEventMaskBit(kCGEventLeftMouseUp) |
      CGEventMaskBit(kCGEventRightMouseDown) |
      CGEventMaskBit(kCGEventRightMouseUp) |
      CGEventMaskBit(kCGEventOtherMouseDown) |
      CGEventMaskBit(kCGEventOtherMouseUp) |
      CGEventMaskBit(kCGEventMouseMoved) |
      CGEventMaskBit(kCGEventLeftMouseDragged) |
      CGEventMaskBit(kCGEventRightMouseDragged) |
      CGEventMaskBit(kCGEventOtherMouseDragged) |
      CGEventMaskBit(kCGEventScrollWheel);
  g_event_tap = CGEventTapCreate(
      kCGHIDEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault,
      mask, event_callback, NULL);
  if (!g_event_tap) {
    set_error("CGEventTapCreate failed; Accessibility permission is required");
    unload_multitouch();
    signal_started(-1);
    return NULL;
  }

  CFRunLoopSourceRef source = CFMachPortCreateRunLoopSource(NULL, g_event_tap, 0);
  if (!source) {
    set_error("could not create the macOS capture run-loop source");
    CFMachPortInvalidate(g_event_tap);
    CFRelease(g_event_tap);
    g_event_tap = NULL;
    unload_multitouch();
    signal_started(-1);
    return NULL;
  }
  CFRunLoopRef run_loop = CFRunLoopGetCurrent();
  pthread_mutex_lock(&g_run_loop_lock);
  g_run_loop = run_loop;
  pthread_mutex_unlock(&g_run_loop_lock);
  CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
  CGEventTapEnable(g_event_tap, true);
  bool cursor_active = capture_cursor();
  if (cursor_active) {
    signal_started(raw_active ? 1 : 0);
    if (!atomic_load(&g_stop)) CFRunLoopRun();
  }

  pthread_mutex_lock(&g_run_loop_lock);
  g_run_loop = NULL;
  pthread_mutex_unlock(&g_run_loop_lock);
  CGEventTapEnable(g_event_tap, false);
  CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
  CFMachPortInvalidate(g_event_tap);
  CFRelease(source);
  CFRelease(g_event_tap);
  g_event_tap = NULL;
  if (!release_cursor()) g_capture_status = -1;
  unload_multitouch();
  atomic_store(&g_stop, true);
  if (!cursor_active) signal_started(-1);
  return NULL;
}

int zflow_mac_raw_touch_available(void) {
  set_error("no diagnostic");
  bool available = load_multitouch(false);
  unload_multitouch();
  return available ? 1 : 0;
}

int zflow_mac_capture_start(int raw_touch) {
  if (g_thread_valid) {
    set_error("capture is already running");
    return -1;
  }
  pthread_mutex_lock(&g_queue_lock);
  g_queue_head = 0;
  g_queue_tail = 0;
  pthread_mutex_unlock(&g_queue_lock);
  atomic_store(&g_stop, false);
  atomic_store(&g_raw_contact_active, false);
  g_capture_status = 0;
  set_error("no diagnostic");
  pthread_mutex_lock(&g_run_loop_lock);
  g_run_loop = NULL;
  pthread_mutex_unlock(&g_run_loop_lock);
  g_request_raw_touch = raw_touch != 0;
  g_start_ready = false;
  g_start_status = -1;
  if (pthread_create(&g_thread, NULL, capture_thread, NULL) != 0) {
    set_error("could not create the macOS capture thread");
    return -1;
  }
  g_thread_valid = true;
  pthread_mutex_lock(&g_start_lock);
  while (!g_start_ready) pthread_cond_wait(&g_start_condition, &g_start_lock);
  int status = g_start_status;
  pthread_mutex_unlock(&g_start_lock);
  if (status < 0) {
    pthread_join(g_thread, NULL);
    g_thread_valid = false;
  }
  return status;
}

int zflow_mac_capture_poll(ZFlowMacEvent *event) {
  if (!event || pthread_mutex_trylock(&g_queue_lock) != 0) return 0;
  if (g_queue_tail == g_queue_head) {
    pthread_mutex_unlock(&g_queue_lock);
    return 0;
  }
  *event = g_queue[g_queue_tail];
  g_queue_tail = (g_queue_tail + 1) % ZFLOW_QUEUE_CAPACITY;
  pthread_mutex_unlock(&g_queue_lock);
  return 1;
}

int zflow_mac_capture_stop_requested(void) {
  return atomic_load(&g_stop) ? 1 : 0;
}

int zflow_mac_capture_stop(void) {
  if (!g_thread_valid) return g_capture_status;
  atomic_store(&g_stop, true);
  stop_capture_run_loop();
  pthread_join(g_thread, NULL);
  g_thread_valid = false;
  atomic_store(&g_raw_contact_active, false);
  return g_capture_status;
}

const char *zflow_mac_capture_last_error(void) {
  return g_error;
}
