#include <ApplicationServices/ApplicationServices.h>
#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/hidsystem/IOHIDLib.h>
#include <IOKit/hidsystem/IOHIDParameter.h>
#include <IOKit/hidsystem/IOHIDShared.h>
#include <IOKit/pwr_mgt/IOPMLib.h>
#include <dispatch/dispatch.h>
#include <math.h>
#include <pthread.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

// "zflow" in ASCII. Every posted event carries it, so a tap can tell it from
// a person's input.
#define ZFLOW_POSTED_MARK 0x7A666C6F77LL
#define ZFLOW_HELD_KEYS 128
#define ZFLOW_HELD_BUTTONS 32
// HIToolbox's kKeyboardISO, the characters 'ISO '.
#define ZFLOW_KEYBOARD_ISO 0x49534F20
// The global preferences count key repeat in ticks of 15 ms.
#define ZFLOW_REPEAT_TICK_NS 15000000ULL

// From Carbon's HIToolbox; declared here instead of including all of Carbon.
extern OSType KBGetLayoutType(SInt16 keyboard_type);
extern UInt8 LMGetKbdType(void);

// One event for zflow_mac_inject_post. Rust decides every field; this file
// only builds the CGEvent. `code` is a keycode for keys and a CG button
// number (0 left, 1 right, 2 and up other) for buttons and drags.
typedef struct {
  uint32_t kind;
  uint16_t code;
  uint8_t down;
  uint8_t autorepeat;
  double x, y;
  int64_t dx, dy;
  int32_t wheel_x, wheel_y;
  uint8_t pixel;
  uint8_t drag;
  uint8_t padding[6];
  int64_t click_state;
  uint64_t flags;
} ZFlowMacPosted;

_Static_assert(sizeof(ZFlowMacPosted) == 72, "Rust's NativePosted has this layout");

enum {
  ZFLOW_POST_MOVE = 1,
  ZFLOW_POST_BUTTON = 2,
  ZFLOW_POST_KEY = 3,
  ZFLOW_POST_MODIFIER = 4,
  ZFLOW_POST_SCROLL = 5,
};

static pthread_mutex_t g_inject_lock = PTHREAD_MUTEX_INITIALIZER;
static CGEventSourceRef g_source;
static io_connect_t g_hid_system;
// What this process holds down, so everything can be released even when Rust
// cannot run: on close and from the signal handlers.
static bool g_held_keys[ZFLOW_HELD_KEYS];
static bool g_held_modifiers[ZFLOW_HELD_KEYS];
static bool g_held_buttons[ZFLOW_HELD_BUTTONS];

// The source takes its state from the HID system, like a real device. With
// no suppression interval and local events permitted, the Mac's own keyboard
// and trackpad keep working while events are posted.
int zflow_mac_inject_open(void) {
  pthread_mutex_lock(&g_inject_lock);
  if (!g_source) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    if (!source) {
      pthread_mutex_unlock(&g_inject_lock);
      return -1;
    }
    CGEventFilterMask permit = kCGEventFilterMaskPermitLocalMouseEvents |
        kCGEventFilterMaskPermitLocalKeyboardEvents |
        kCGEventFilterMaskPermitSystemDefinedEvents;
    CGEventSourceSetLocalEventsSuppressionInterval(source, 0);
    CGEventSourceSetLocalEventsFilterDuringSuppressionState(
        source, permit, kCGEventSuppressionStateSuppressionInterval);
    CGEventSourceSetLocalEventsFilterDuringSuppressionState(
        source, permit, kCGEventSuppressionStateRemoteMouseDrag);
    CGEventSourceSetUserData(source, ZFLOW_POSTED_MARK);
    g_source = source;
  }
  pthread_mutex_unlock(&g_inject_lock);
  return 0;
}

static CGEventType button_type(uint16_t button, bool down) {
  switch (button) {
    case kCGMouseButtonLeft: return down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp;
    case kCGMouseButtonRight: return down ? kCGEventRightMouseDown : kCGEventRightMouseUp;
    default: return down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp;
  }
}

static CGEventType drag_type(uint16_t button) {
  switch (button) {
    case kCGMouseButtonLeft: return kCGEventLeftMouseDragged;
    case kCGMouseButtonRight: return kCGEventRightMouseDragged;
    default: return kCGEventOtherMouseDragged;
  }
}

static CGEventRef create_event(const ZFlowMacPosted *posted) {
  CGPoint point = CGPointMake(posted->x, posted->y);
  bool pointer = posted->kind == ZFLOW_POST_MOVE || posted->kind == ZFLOW_POST_BUTTON;
  if (pointer && (!isfinite(posted->x) || !isfinite(posted->y) ||
                  posted->code >= ZFLOW_HELD_BUTTONS)) return NULL;
  CGEventRef event = NULL;
  switch (posted->kind) {
    case ZFLOW_POST_MOVE:
      event = CGEventCreateMouseEvent(
          g_source, posted->drag ? drag_type(posted->code) : kCGEventMouseMoved,
          point, (CGMouseButton)posted->code);
      if (!event) return NULL;
      // Only the location moves the cursor. The deltas are for apps that
      // read them, such as games.
      CGEventSetIntegerValueField(event, kCGMouseEventDeltaX, posted->dx);
      CGEventSetIntegerValueField(event, kCGMouseEventDeltaY, posted->dy);
      if (posted->drag)
        CGEventSetIntegerValueField(event, kCGMouseEventClickState, posted->click_state);
      break;
    case ZFLOW_POST_BUTTON:
      event = CGEventCreateMouseEvent(g_source, button_type(posted->code, posted->down),
                                      point, (CGMouseButton)posted->code);
      if (!event) return NULL;
      // Without the click state a second click is not a double-click.
      CGEventSetIntegerValueField(event, kCGMouseEventClickState, posted->click_state);
      break;
    case ZFLOW_POST_KEY:
    case ZFLOW_POST_MODIFIER:
      if (posted->code >= ZFLOW_HELD_KEYS) return NULL;
      event = CGEventCreateKeyboardEvent(g_source, posted->code, posted->down);
      if (!event) return NULL;
      if (posted->kind == ZFLOW_POST_MODIFIER) {
        CGEventSetType(event, kCGEventFlagsChanged);
      } else if (posted->autorepeat) {
        CGEventSetIntegerValueField(event, kCGKeyboardEventAutorepeat, 1);
      }
      break;
    case ZFLOW_POST_SCROLL:
      event = CGEventCreateScrollWheelEvent2(
          g_source, posted->pixel ? kCGScrollEventUnitPixel : kCGScrollEventUnitLine,
          2, posted->wheel_y, posted->wheel_x, 0);
      if (!event) return NULL;
      CGEventSetIntegerValueField(event, kCGScrollWheelEventIsContinuous, posted->pixel ? 1 : 0);
      break;
    default:
      return NULL;
  }
  CGEventSetFlags(event, (CGEventFlags)posted->flags);
  CGEventSetIntegerValueField(event, kCGEventSourceUserData, ZFLOW_POSTED_MARK);
  return event;
}

static void track(const ZFlowMacPosted *posted) {
  switch (posted->kind) {
    case ZFLOW_POST_BUTTON: g_held_buttons[posted->code] = posted->down; break;
    case ZFLOW_POST_KEY: g_held_keys[posted->code] = posted->down; break;
    case ZFLOW_POST_MODIFIER: g_held_modifiers[posted->code] = posted->down; break;
    default: break;
  }
}

int zflow_mac_inject_post(const ZFlowMacPosted *posted) {
  if (!posted) return -1;
  pthread_mutex_lock(&g_inject_lock);
  CGEventRef event = g_source ? create_event(posted) : NULL;
  if (!event) {
    pthread_mutex_unlock(&g_inject_lock);
    return -1;
  }
  CGEventPost(kCGHIDEventTap, event);
  track(posted);
  CFRelease(event);
  pthread_mutex_unlock(&g_inject_lock);
  return 0;
}

static void post_release(CGEventRef event) {
  if (!event) return;
  CGEventSetFlags(event, 0);
  CGEventSetIntegerValueField(event, kCGEventSourceUserData, ZFLOW_POSTED_MARK);
  CGEventPost(kCGHIDEventTap, event);
  CFRelease(event);
}

// Keys before modifiers, so a released chord never leaves its bare key down
// to repeat, then buttons where the cursor is now.
static void release_held(void) {
  for (uint16_t code = 0; code < ZFLOW_HELD_KEYS; code++) {
    if (!g_held_keys[code]) continue;
    g_held_keys[code] = false;
    post_release(CGEventCreateKeyboardEvent(g_source, code, false));
  }
  for (uint16_t code = 0; code < ZFLOW_HELD_KEYS; code++) {
    if (!g_held_modifiers[code]) continue;
    g_held_modifiers[code] = false;
    CGEventRef event = CGEventCreateKeyboardEvent(g_source, code, false);
    if (event) CGEventSetType(event, kCGEventFlagsChanged);
    post_release(event);
  }
  CGPoint cursor = CGPointZero;
  CGEventRef now = CGEventCreate(NULL);
  if (now) {
    cursor = CGEventGetLocation(now);
    CFRelease(now);
  }
  for (uint16_t button = 0; button < ZFLOW_HELD_BUTTONS; button++) {
    if (!g_held_buttons[button]) continue;
    g_held_buttons[button] = false;
    post_release(CGEventCreateMouseEvent(g_source, button_type(button, false), cursor,
                                         (CGMouseButton)button));
  }
}

// Releases whatever the table still holds. Rust releases in order first, so
// this normally finds nothing; it catches a post that failed halfway.
void zflow_mac_inject_release_all(void) {
  pthread_mutex_lock(&g_inject_lock);
  release_held();
  pthread_mutex_unlock(&g_inject_lock);
}

void zflow_mac_inject_close(void) {
  pthread_mutex_lock(&g_inject_lock);
  release_held();
  if (g_source) CFRelease(g_source);
  g_source = NULL;
  if (g_hid_system) IOServiceClose(g_hid_system);
  g_hid_system = IO_OBJECT_NULL;
  pthread_mutex_unlock(&g_inject_lock);
}

static bool open_hid_system(void) {
  if (g_hid_system) return true;
  io_service_t service = IOServiceGetMatchingService(
      kIOMainPortDefault, IOServiceMatching(kIOHIDSystemClass));
  if (!service) return false;
  kern_return_t result = IOServiceOpen(service, mach_task_self(), kIOHIDParamConnectType,
                                       &g_hid_system);
  IOObjectRelease(service);
  if (result != KERN_SUCCESS) g_hid_system = IO_OBJECT_NULL;
  return result == KERN_SUCCESS;
}

int zflow_mac_caps_lock(int *on) {
  if (!on) return -1;
  pthread_mutex_lock(&g_inject_lock);
  bool state = false;
  int status = open_hid_system() &&
      IOHIDGetModifierLockState(g_hid_system, kIOHIDCapsLockState, &state) == KERN_SUCCESS
      ? 0 : -1;
  pthread_mutex_unlock(&g_inject_lock);
  if (status == 0) *on = state ? 1 : 0;
  return status;
}

// A posted Caps Lock key only fakes the flag: letters change case while the
// real lock and its light stay as they were. IOHIDSystem holds the real lock.
int zflow_mac_set_caps_lock(int on) {
  pthread_mutex_lock(&g_inject_lock);
  int status = open_hid_system() &&
      IOHIDSetModifierLockState(g_hid_system, kIOHIDCapsLockState, on != 0) == KERN_SUCCESS
      ? 0 : -1;
  pthread_mutex_unlock(&g_inject_lock);
  return status;
}

// The layout of the keyboard last typed on. A posted event's keyboard type
// does not change the characters, so the caller swaps keycodes itself.
int zflow_mac_keyboard_is_iso(void) {
  return KBGetLayoutType((SInt16)LMGetKbdType()) == ZFLOW_KEYBOARD_ISO ? 1 : 0;
}

static bool positive_number(CFTypeRef value, uint64_t *out) {
  int64_t number = 0;
  if (!value || CFGetTypeID(value) != CFNumberGetTypeID() ||
      !CFNumberGetValue((CFNumberRef)value, kCFNumberSInt64Type, &number) || number <= 0)
    return false;
  *out = (uint64_t)number;
  return true;
}

static bool preference_ticks(CFStringRef key, uint64_t *out) {
  CFPropertyListRef value = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
  bool found = positive_number(value, out);
  if (value) CFRelease(value);
  if (found) *out *= ZFLOW_REPEAT_TICK_NS;
  return found;
}

// IOHIDSystem's rate is what real keyboards repeat at. The global preferences
// are the fallback.
int zflow_mac_key_repeat_ns(uint64_t *initial, uint64_t *interval) {
  if (!initial || !interval) return -1;
  io_service_t service = IOServiceGetMatchingService(
      kIOMainPortDefault, IOServiceMatching(kIOHIDSystemClass));
  if (service) {
    CFTypeRef parameters = IORegistryEntryCreateCFProperty(
        service, CFSTR(kIOHIDParametersKey), kCFAllocatorDefault, 0);
    IOObjectRelease(service);
    bool found = parameters && CFGetTypeID(parameters) == CFDictionaryGetTypeID() &&
        positive_number(CFDictionaryGetValue(parameters, CFSTR(kIOHIDInitialKeyRepeatKey)),
                        initial) &&
        positive_number(CFDictionaryGetValue(parameters, CFSTR(kIOHIDKeyRepeatKey)), interval);
    if (parameters) CFRelease(parameters);
    if (found) return 0;
  }
  return preference_ticks(CFSTR("InitialKeyRepeat"), initial) &&
      preference_ticks(CFSTR("KeyRepeat"), interval) ? 0 : -1;
}

static bool dictionary_flag(CFDictionaryRef dictionary, CFStringRef key, bool missing) {
  CFTypeRef value = CFDictionaryGetValue(dictionary, key);
  if (value && CFGetTypeID(value) == CFBooleanGetTypeID())
    return CFBooleanGetValue((CFBooleanRef)value);
  int64_t number = 0;
  if (value && CFGetTypeID(value) == CFNumberGetTypeID() &&
      CFNumberGetValue((CFNumberRef)value, kCFNumberSInt64Type, &number))
    return number != 0;
  return missing;
}

// Locked when the screen lock is up or another user has the console. The
// lock key is not documented; it is absent while unlocked.
int zflow_mac_session_locked(void) {
  CFDictionaryRef session = CGSessionCopyCurrentDictionary();
  if (!session) return 1;
  bool locked = dictionary_flag(session, CFSTR("CGSSessionScreenIsLocked"), false) ||
      !dictionary_flag(session, kCGSessionOnConsoleKey, false);
  CFRelease(session);
  return locked ? 1 : 0;
}

int zflow_mac_post_allowed(void) {
  return CGPreflightPostEventAccess() ? 1 : 0;
}

// Wakes the display and holds off idle sleep, as a local event would.
int zflow_mac_declare_user_activity(void) {
  static IOPMAssertionID assertion = kIOPMNullAssertionID;
  pthread_mutex_lock(&g_inject_lock);
  IOReturn result = IOPMAssertionDeclareUserActivity(
      CFSTR("zflow remote input"), kIOPMUserActiveLocal, &assertion);
  pthread_mutex_unlock(&g_inject_lock);
  return result == kIOReturnSuccess ? 0 : -1;
}

static void release_and_exit(void *context) {
  int number = (int)(intptr_t)context;
  zflow_mac_inject_release_all();
  signal(number, SIG_DFL);
  raise(number);
}

static void install_exit_handlers(void) {
  static const int numbers[] = {SIGINT, SIGTERM, SIGHUP};
  dispatch_queue_t queue = dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0);
  for (size_t i = 0; i < sizeof(numbers) / sizeof(numbers[0]); i++) {
    dispatch_source_t source = dispatch_source_create(
        DISPATCH_SOURCE_TYPE_SIGNAL, (uintptr_t)numbers[i], 0, queue);
    if (!source) continue;
    // Dispatch still sees an ignored signal; the default action would kill
    // the process before the handler runs.
    signal(numbers[i], SIG_IGN);
    dispatch_set_context(source, (void *)(intptr_t)numbers[i]);
    dispatch_source_set_event_handler_f(source, release_and_exit);
    dispatch_resume(source);
  }
}

// Keys and buttons held for a peer would stay down after the app dies. On
// SIGINT, SIGTERM and SIGHUP release them, then die of the same signal.
// Dispatch runs the handler on a normal thread, where posting is safe.
// SIGKILL cannot be caught.
void zflow_mac_inject_install_exit_handlers(void) {
  static pthread_once_t once = PTHREAD_ONCE_INIT;
  pthread_once(&once, install_exit_handlers);
}
