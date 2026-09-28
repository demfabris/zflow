#include <ApplicationServices/ApplicationServices.h>
#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/hidsystem/IOHIDLib.h>
#include <IOKit/hidsystem/IOHIDParameter.h>
#include <IOKit/hidsystem/IOHIDShared.h>
#include <IOKit/pwr_mgt/IOPMLib.h>
#include <assert.h>
#include <mach-o/dyld.h>
#include <math.h>
#include <signal.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void fake_post(CGEventTapLocation, CGEventRef);
static CGEventRef fake_create(CGEventSourceRef);
static CGEventFlags fake_flags_state(CGEventSourceStateID);
static void fake_suppression_interval(CGEventSourceRef, CFTimeInterval);
static void fake_suppression_filter(CGEventSourceRef, CGEventFilterMask,
                                    CGEventSuppressionState);
static io_service_t fake_matching_service(mach_port_t, CFDictionaryRef);
static kern_return_t fake_service_open(io_service_t, task_port_t, uint32_t, io_connect_t *);
static kern_return_t fake_service_close(io_connect_t);
static kern_return_t fake_object_release(io_object_t);
static kern_return_t fake_get_lock(io_connect_t, int, bool *);
static kern_return_t fake_set_lock(io_connect_t, int, bool);
static CFTypeRef fake_registry_property(io_registry_entry_t, CFStringRef, CFAllocatorRef,
                                        IOOptionBits);
static CFPropertyListRef fake_preference(CFStringRef, CFStringRef);
static CFDictionaryRef fake_session(void);
static OSType fake_layout_type(SInt16);
static UInt8 fake_keyboard_type(void);
static bool fake_post_access(void);
static IOReturn fake_user_activity(CFStringRef, IOPMUserActiveType, IOPMAssertionID *);
static sig_t fake_signal(int, sig_t);

// Every call that posts, touches IOKit or reads the session is faked. Events
// are still built by CoreGraphics, so the tests read back real fields. These
// tests never post input on the host.
#define CGEventPost fake_post
#define CGEventCreate fake_create
#define CGEventSourceFlagsState fake_flags_state
#define CGEventSourceSetLocalEventsSuppressionInterval fake_suppression_interval
#define CGEventSourceSetLocalEventsFilterDuringSuppressionState fake_suppression_filter
#define IOServiceGetMatchingService fake_matching_service
#define IOServiceOpen fake_service_open
#define IOServiceClose fake_service_close
#define IOObjectRelease fake_object_release
#define IOHIDGetModifierLockState fake_get_lock
#define IOHIDSetModifierLockState fake_set_lock
#define IORegistryEntryCreateCFProperty fake_registry_property
#define CFPreferencesCopyAppValue fake_preference
#define CGSessionCopyCurrentDictionary fake_session
#define KBGetLayoutType fake_layout_type
#define LMGetKbdType fake_keyboard_type
#define CGPreflightPostEventAccess fake_post_access
#define IOPMAssertionDeclareUserActivity fake_user_activity
#define signal(number, handler) fake_signal(number, handler)
#include "../src/macos/inject.c"
#undef signal

#define FAKE_SERVICE 0x51
#define FAKE_CONNECTION 0x52
#define PERMIT_LOCAL (kCGEventFilterMaskPermitLocalMouseEvents | \
                      kCGEventFilterMaskPermitLocalKeyboardEvents | \
                      kCGEventFilterMaskPermitSystemDefinedEvents)

static CGEventRef posted[64];
static size_t posted_count;
// The spawned child prints what it posts, so the parent can check the
// signal handler's releases.
static bool print_posts;
static atomic_bool handler_posted;
static int interval_calls;
static double last_interval = -1;
static int filter_calls;
static bool filters[2];
static bool service_missing;
static bool open_fails;
static int opens;
static int closes;
static bool caps_state;
static int lock_calls;
static int32_t repeat_parameters[2] = {250000000, 33333333};
static bool registry_missing;
static int32_t repeat_preferences[2] = {15, 2};
static bool preferences_missing;
static CFDictionaryRef session;
static SInt16 keyboard_type = 40;
static bool post_access = true;
static int activity_calls;
static CGPoint cursor = {640, 400};
static CGEventFlags live_flags;

static void fake_post(CGEventTapLocation tap, CGEventRef event) {
  assert(tap == kCGHIDEventTap);
  if (print_posts) {
    atomic_store(&handler_posted, true);
    CGEventType type = CGEventGetType(event);
    bool mouse = type == kCGEventLeftMouseUp || type == kCGEventRightMouseUp ||
                 type == kCGEventOtherMouseUp;
    printf("%d %lld\n", (int)type, CGEventGetIntegerValueField(
        event, mouse ? kCGMouseEventButtonNumber : kCGKeyboardEventKeycode));
    fflush(stdout);
  }
  assert(posted_count < sizeof(posted) / sizeof(posted[0]));
  posted[posted_count++] = (CGEventRef)CFRetain(event);
}

static CGEventRef fake_create(CGEventSourceRef source) {
  assert(!source);
  return CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, cursor, kCGMouseButtonLeft);
}

static CGEventFlags fake_flags_state(CGEventSourceStateID state) {
  assert(state == kCGEventSourceStateHIDSystemState);
  return live_flags;
}

static void fake_suppression_interval(CGEventSourceRef source, CFTimeInterval seconds) {
  assert(source);
  interval_calls++;
  last_interval = seconds;
}

static void fake_suppression_filter(CGEventSourceRef source, CGEventFilterMask filter,
                                    CGEventSuppressionState state) {
  assert(source && filter == PERMIT_LOCAL);
  assert(state == kCGEventSuppressionStateSuppressionInterval ||
         state == kCGEventSuppressionStateRemoteMouseDrag);
  filters[state == kCGEventSuppressionStateRemoteMouseDrag] = true;
  filter_calls++;
}

static io_service_t fake_matching_service(mach_port_t port, CFDictionaryRef matching) {
  assert(port == kIOMainPortDefault && matching);
  CFStringRef class_name = CFDictionaryGetValue(matching, CFSTR(kIOProviderClassKey));
  assert(class_name && CFEqual(class_name, CFSTR(kIOHIDSystemClass)));
  CFRelease(matching);
  return service_missing ? IO_OBJECT_NULL : FAKE_SERVICE;
}

static kern_return_t fake_service_open(io_service_t service, task_port_t task,
                                       uint32_t type, io_connect_t *connect) {
  assert(service == FAKE_SERVICE && task == mach_task_self());
  assert(type == kIOHIDParamConnectType);
  opens++;
  if (open_fails) return kIOReturnNotPermitted;
  *connect = FAKE_CONNECTION;
  return KERN_SUCCESS;
}

static kern_return_t fake_service_close(io_connect_t connect) {
  assert(connect == FAKE_CONNECTION);
  closes++;
  return KERN_SUCCESS;
}

static kern_return_t fake_object_release(io_object_t object) {
  assert(object == FAKE_SERVICE);
  return KERN_SUCCESS;
}

static kern_return_t fake_get_lock(io_connect_t connect, int selector, bool *state) {
  assert(connect == FAKE_CONNECTION && selector == kIOHIDCapsLockState);
  lock_calls++;
  *state = caps_state;
  return KERN_SUCCESS;
}

static kern_return_t fake_set_lock(io_connect_t connect, int selector, bool state) {
  assert(connect == FAKE_CONNECTION && selector == kIOHIDCapsLockState);
  lock_calls++;
  caps_state = state;
  return KERN_SUCCESS;
}

static CFNumberRef number(int32_t value) {
  return CFNumberCreate(NULL, kCFNumberSInt32Type, &value);
}

static CFTypeRef fake_registry_property(io_registry_entry_t entry, CFStringRef key,
                                        CFAllocatorRef allocator, IOOptionBits options) {
  assert(entry == FAKE_SERVICE && allocator == kCFAllocatorDefault && options == 0);
  assert(CFEqual(key, CFSTR(kIOHIDParametersKey)));
  if (registry_missing) return NULL;
  CFMutableDictionaryRef parameters = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
  CFStringRef keys[] = {CFSTR(kIOHIDInitialKeyRepeatKey), CFSTR(kIOHIDKeyRepeatKey)};
  for (int i = 0; i < 2; i++) {
    CFNumberRef value = number(repeat_parameters[i]);
    CFDictionarySetValue(parameters, keys[i], value);
    CFRelease(value);
  }
  return parameters;
}

static CFPropertyListRef fake_preference(CFStringRef key, CFStringRef application) {
  assert(CFEqual(application, kCFPreferencesAnyApplication));
  if (preferences_missing) return NULL;
  if (CFEqual(key, CFSTR("InitialKeyRepeat"))) return number(repeat_preferences[0]);
  assert(CFEqual(key, CFSTR("KeyRepeat")));
  return number(repeat_preferences[1]);
}

static CFDictionaryRef fake_session(void) {
  return session ? CFRetain(session) : NULL;
}

// Keyboard types with the layouts HIToolbox reports for them.
static OSType fake_layout_type(SInt16 keyboard) {
  switch (keyboard) {
    case 40: return 0x414E5349; // 'ANSI'
    case 41: return ZFLOW_KEYBOARD_ISO;
    default: return 0x4A495320; // 'JIS '
  }
}

static UInt8 fake_keyboard_type(void) { return (UInt8)keyboard_type; }

static bool fake_post_access(void) { return post_access; }

static IOReturn fake_user_activity(CFStringRef name, IOPMUserActiveType type,
                                   IOPMAssertionID *assertion) {
  assert(CFStringGetLength(name) > 0 && type == kIOPMUserActiveLocal && assertion);
  // The same assertion is reused on every call.
  assert(*assertion == (activity_calls ? 7 : kIOPMNullAssertionID));
  *assertion = 7;
  activity_calls++;
  return kIOReturnSuccess;
}

// The exit handler restores the default action just before it re-raises.
// A post from the input thread at that moment must not press anything.
static sig_t fake_signal(int number, sig_t handler) {
  if (print_posts && handler == SIG_DFL) {
    ZFlowMacPosted key = {.kind = ZFLOW_POST_KEY, .code = 41, .down = 1};
    printf("late post %d\n", zflow_mac_inject_post(&key));
    fflush(stdout);
  }
  return signal(number, handler);
}

static void clear_posted(void) {
  for (size_t i = 0; i < posted_count; i++) CFRelease(posted[i]);
  posted_count = 0;
}

static CGEventRef post_one(ZFlowMacPosted event) {
  size_t before = posted_count;
  assert(zflow_mac_inject_post(&event) == 0);
  assert(posted_count == before + 1);
  CGEventRef result = posted[before];
  assert(CGEventGetIntegerValueField(result, kCGEventSourceUserData) == ZFLOW_POSTED_MARK);
  return result;
}

static int64_t field(CGEventRef event, CGEventField name) {
  return CGEventGetIntegerValueField(event, name);
}

static void source_tests(void) {
  ZFlowMacPosted key = {.kind = ZFLOW_POST_KEY, .code = 0, .down = 1};
  assert(zflow_mac_inject_post(&key) == -1);
  assert(posted_count == 0);
  assert(zflow_mac_inject_open() == 0);
  assert(interval_calls == 1 && last_interval == 0);
  assert(filter_calls == 2 && filters[0] && filters[1]);
  assert(CGEventSourceGetUserData(g_source) == ZFLOW_POSTED_MARK);
  assert(CGEventSourceGetSourceStateID(g_source) == kCGEventSourceStateHIDSystemState);
  // A second open keeps the source.
  CGEventSourceRef source = g_source;
  assert(zflow_mac_inject_open() == 0);
  assert(g_source == source && interval_calls == 1);
}

static void pointer_tests(void) {
  CGEventRef event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_MOVE, .x = 100.5, .y = 200, .dx = 3, .dy = -4,
      .flags = kCGEventFlagMaskCommand | NX_DEVICELCMDKEYMASK});
  assert(CGEventGetType(event) == kCGEventMouseMoved);
  CGPoint location = CGEventGetLocation(event);
  assert(location.x == 100.5 && location.y == 200);
  assert(field(event, kCGMouseEventDeltaX) == 3 && field(event, kCGMouseEventDeltaY) == -4);
  assert(CGEventGetFlags(event) == (kCGEventFlagMaskCommand | NX_DEVICELCMDKEYMASK));

  const CGEventType drags[] = {kCGEventLeftMouseDragged, kCGEventRightMouseDragged,
                               kCGEventOtherMouseDragged, kCGEventOtherMouseDragged};
  for (uint16_t button = 0; button < 4; button++) {
    event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_MOVE, .code = button, .drag = 1,
                                      .x = 1, .y = 2, .click_state = 2});
    assert(CGEventGetType(event) == drags[button]);
    assert(field(event, kCGMouseEventClickState) == 2);
    if (button > 1) assert(field(event, kCGMouseEventButtonNumber) == button);
  }

  const CGEventType downs[] = {kCGEventLeftMouseDown, kCGEventRightMouseDown,
                               kCGEventOtherMouseDown};
  const CGEventType ups[] = {kCGEventLeftMouseUp, kCGEventRightMouseUp,
                             kCGEventOtherMouseUp};
  for (uint16_t button = 0; button < 3; button++) {
    for (int down = 1; down >= 0; down--) {
      event = post_one((ZFlowMacPosted){
          .kind = ZFLOW_POST_BUTTON, .code = button, .down = (uint8_t)down,
          .x = 5, .y = 6, .click_state = 3, .flags = kCGEventFlagMaskShift});
      assert(CGEventGetType(event) == (down ? downs[button] : ups[button]));
      assert(field(event, kCGMouseEventClickState) == 3);
      assert(CGEventGetFlags(event) == kCGEventFlagMaskShift);
      assert(CGEventGetLocation(event).x == 5 && CGEventGetLocation(event).y == 6);
    }
  }
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_BUTTON, .code = 31, .down = 1});
  assert(field(event, kCGMouseEventButtonNumber) == 31);
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_BUTTON, .code = 31});
  assert(CGEventGetType(event) == kCGEventOtherMouseUp);
}

static void key_tests(void) {
  CGEventRef event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_KEY, .code = 0, .down = 1,
      .flags = kCGEventFlagMaskShift | NX_DEVICELSHIFTKEYMASK});
  assert(CGEventGetType(event) == kCGEventKeyDown);
  assert(field(event, kCGKeyboardEventKeycode) == 0);
  assert(field(event, kCGKeyboardEventAutorepeat) == 0);
  assert(CGEventGetFlags(event) == (kCGEventFlagMaskShift | NX_DEVICELSHIFTKEYMASK));
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 0, .down = 1,
                                    .autorepeat = 1});
  assert(CGEventGetType(event) == kCGEventKeyDown);
  assert(field(event, kCGKeyboardEventAutorepeat) == 1);
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 0});
  assert(CGEventGetType(event) == kCGEventKeyUp);

  // flagsChanged carries the aggregate flag and the side's device bit.
  event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_MODIFIER, .code = 54, .down = 1,
      .flags = kCGEventFlagMaskCommand | NX_DEVICERCMDKEYMASK});
  assert(CGEventGetType(event) == kCGEventFlagsChanged);
  assert(field(event, kCGKeyboardEventKeycode) == 54);
  assert(CGEventGetFlags(event) == (kCGEventFlagMaskCommand | NX_DEVICERCMDKEYMASK));
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_MODIFIER, .code = 54});
  assert(CGEventGetType(event) == kCGEventFlagsChanged && CGEventGetFlags(event) == 0);

  // Caps Lock follows the Mac's own lock, whatever the caller sent.
  live_flags = kCGEventFlagMaskAlphaShift | kCGEventFlagMaskControl;
  event = post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 0, .down = 1,
                                    .flags = kCGEventFlagMaskCommand});
  assert(CGEventGetFlags(event) == (kCGEventFlagMaskAlphaShift | kCGEventFlagMaskCommand));
  live_flags = 0;
  event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_KEY, .code = 0,
      .flags = kCGEventFlagMaskAlphaShift | kCGEventFlagMaskCommand});
  assert(CGEventGetFlags(event) == kCGEventFlagMaskCommand);
}

static void scroll_tests(void) {
  CGEventRef event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_SCROLL, .wheel_x = -1, .wheel_y = 2});
  assert(CGEventGetType(event) == kCGEventScrollWheel);
  assert(field(event, kCGScrollWheelEventIsContinuous) == 0);
  assert(field(event, kCGScrollWheelEventDeltaAxis1) == 2);
  assert(field(event, kCGScrollWheelEventDeltaAxis2) == -1);
  event = post_one((ZFlowMacPosted){
      .kind = ZFLOW_POST_SCROLL, .wheel_x = 7, .wheel_y = -40, .pixel = 1,
      .flags = kCGEventFlagMaskAlternate});
  assert(field(event, kCGScrollWheelEventIsContinuous) == 1);
  assert(field(event, kCGScrollWheelEventPointDeltaAxis1) == -40);
  assert(field(event, kCGScrollWheelEventPointDeltaAxis2) == 7);
  assert(CGEventGetFlags(event) == kCGEventFlagMaskAlternate);
}

static void refusal_tests(void) {
  size_t before = posted_count;
  ZFlowMacPosted bad[] = {
    {.kind = ZFLOW_POST_KEY, .code = 128},
    {.kind = ZFLOW_POST_MODIFIER, .code = 200},
    {.kind = ZFLOW_POST_BUTTON, .code = 32},
    {.kind = ZFLOW_POST_MOVE, .x = NAN},
    {.kind = ZFLOW_POST_BUTTON, .y = INFINITY},
    {.kind = 0},
    {.kind = 99},
  };
  for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++)
    assert(zflow_mac_inject_post(&bad[i]) == -1);
  assert(zflow_mac_inject_post(NULL) == -1);
  assert(posted_count == before);
}

static void release_tests(void) {
  clear_posted();
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_BUTTON, .code = 0, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_BUTTON, .code = 3, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_MODIFIER, .code = 56, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_MODIFIER, .code = 55, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 40, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 0, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 40});
  clear_posted();

  // Keys, then modifiers, then buttons where the cursor is.
  zflow_mac_inject_release_all();
  assert(posted_count == 5);
  assert(CGEventGetType(posted[0]) == kCGEventKeyUp && field(posted[0], kCGKeyboardEventKeycode) == 0);
  assert(CGEventGetType(posted[1]) == kCGEventFlagsChanged && field(posted[1], kCGKeyboardEventKeycode) == 55);
  assert(CGEventGetType(posted[2]) == kCGEventFlagsChanged && field(posted[2], kCGKeyboardEventKeycode) == 56);
  assert(CGEventGetType(posted[3]) == kCGEventLeftMouseUp);
  assert(CGEventGetType(posted[4]) == kCGEventOtherMouseUp && field(posted[4], kCGMouseEventButtonNumber) == 3);
  for (size_t i = 0; i < posted_count; i++) {
    assert(CGEventGetFlags(posted[i]) == 0);
    assert(field(posted[i], kCGEventSourceUserData) == ZFLOW_POSTED_MARK);
  }
  assert(CGEventGetLocation(posted[3]).x == cursor.x && CGEventGetLocation(posted[4]).y == cursor.y);
  clear_posted();
  zflow_mac_inject_release_all();
  assert(posted_count == 0);

  // Closing releases too, and nothing posts until the source is back.
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 12, .down = 1});
  clear_posted();
  zflow_mac_inject_close();
  assert(posted_count == 1 && CGEventGetType(posted[0]) == kCGEventKeyUp);
  assert(!g_source);
  ZFlowMacPosted key = {.kind = ZFLOW_POST_KEY, .code = 12, .down = 1};
  assert(zflow_mac_inject_post(&key) == -1);
  assert(zflow_mac_inject_open() == 0 && interval_calls == 2);
  clear_posted();
}

static void caps_tests(void) {
  size_t before = posted_count;
  int on = -1;
  assert(zflow_mac_caps_lock(&on) == 0 && on == 0);
  assert(opens == 1);
  assert(zflow_mac_set_caps_lock(1) == 0 && caps_state);
  assert(zflow_mac_caps_lock(&on) == 0 && on == 1);
  assert(zflow_mac_set_caps_lock(0) == 0 && !caps_state);
  assert(opens == 1 && lock_calls == 4);
  // No key event: a posted Caps Lock only fakes the flag.
  assert(posted_count == before);
  assert(zflow_mac_caps_lock(NULL) == -1);

  zflow_mac_inject_close();
  assert(closes == 1);
  open_fails = true;
  assert(zflow_mac_caps_lock(&on) == -1 && zflow_mac_set_caps_lock(1) == -1);
  open_fails = false;
  service_missing = true;
  assert(zflow_mac_set_caps_lock(1) == -1 && !caps_state);
  service_missing = false;
  assert(zflow_mac_inject_open() == 0);
}

static void environment_tests(void) {
  keyboard_type = 40; assert(zflow_mac_keyboard_is_iso() == 0);
  keyboard_type = 41; assert(zflow_mac_keyboard_is_iso() == 1);
  keyboard_type = 42; assert(zflow_mac_keyboard_is_iso() == 0);

  uint64_t initial = 0, interval = 0;
  assert(zflow_mac_key_repeat_ns(&initial, &interval) == 0);
  assert(initial == 250000000 && interval == 33333333);
  registry_missing = true;
  assert(zflow_mac_key_repeat_ns(&initial, &interval) == 0);
  assert(initial == 225000000 && interval == 30000000);
  registry_missing = false;
  repeat_parameters[1] = 0;
  assert(zflow_mac_key_repeat_ns(&initial, &interval) == 0);
  assert(initial == 225000000 && interval == 30000000);
  service_missing = true;
  preferences_missing = true;
  assert(zflow_mac_key_repeat_ns(&initial, &interval) == -1);
  assert(zflow_mac_key_repeat_ns(NULL, &interval) == -1);
  service_missing = false;
  preferences_missing = false;
  repeat_parameters[1] = 33333333;

  assert(zflow_mac_post_allowed() == 1);
  post_access = false;
  assert(zflow_mac_post_allowed() == 0);
  assert(zflow_mac_declare_user_activity() == 0);
  assert(zflow_mac_declare_user_activity() == 0);
  assert(activity_calls == 2);
}

static CFDictionaryRef session_with(CFStringRef key, CFTypeRef value,
                                    CFStringRef key2, CFTypeRef value2) {
  CFMutableDictionaryRef dictionary = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
  if (key) CFDictionarySetValue(dictionary, key, value);
  if (key2) CFDictionarySetValue(dictionary, key2, value2);
  return dictionary;
}

static void session_tests(void) {
  CFStringRef locked = CFSTR("CGSSessionScreenIsLocked");
  CFNumberRef one = number(1);
  CFNumberRef zero = number(0);
  struct { CFDictionaryRef session; int locked; } cases[] = {
    {NULL, 1},
    {session_with(kCGSessionOnConsoleKey, kCFBooleanTrue, NULL, NULL), 0},
    {session_with(kCGSessionOnConsoleKey, one, NULL, NULL), 0},
    {session_with(kCGSessionOnConsoleKey, kCFBooleanFalse, NULL, NULL), 1},
    {session_with(kCGSessionOnConsoleKey, zero, NULL, NULL), 1},
    {session_with(NULL, NULL, NULL, NULL), 1},
    {session_with(kCGSessionOnConsoleKey, kCFBooleanTrue, locked, kCFBooleanTrue), 1},
    {session_with(kCGSessionOnConsoleKey, kCFBooleanTrue, locked, kCFBooleanFalse), 0},
    {session_with(kCGSessionOnConsoleKey, kCFBooleanTrue, locked, CFSTR("yes")), 0},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
    session = cases[i].session;
    assert(zflow_mac_session_locked() == cases[i].locked);
    if (session) CFRelease(session);
  }
  session = NULL;
  CFRelease(one);
  CFRelease(zero);
}

// Runs in a spawned copy: hold input, then take SIGTERM. The handler must
// release everything, then die of the signal it raises itself.
static int signal_child(void) {
  alarm(10);
  assert(zflow_mac_inject_open() == 0);
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_KEY, .code = 40, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_MODIFIER, .code = 56, .down = 1});
  post_one((ZFlowMacPosted){.kind = ZFLOW_POST_BUTTON, .code = 1, .down = 1});
  zflow_mac_inject_install_exit_handlers();
  zflow_mac_inject_install_exit_handlers();
  print_posts = true;
  // Dispatch arms its sources asynchronously, and an ignored signal that
  // arrives first is lost, so keep sending until the handler posts. After
  // that, only its own raise may end the process before the alarm does.
  while (!atomic_load(&handler_posted)) {
    kill(getpid(), SIGTERM);
    usleep(20000);
  }
  for (;;) pause();
}

static void signal_tests(void) {
  char path[4096];
  uint32_t size = sizeof(path);
  assert(_NSGetExecutablePath(path, &size) == 0);
  int output[2];
  assert(pipe(output) == 0);
  posix_spawn_file_actions_t actions;
  posix_spawn_file_actions_init(&actions);
  posix_spawn_file_actions_adddup2(&actions, output[1], STDOUT_FILENO);
  posix_spawn_file_actions_addclose(&actions, output[0]);
  char *arguments[] = {path, "signal-child", NULL};
  extern char **environ;
  pid_t child;
  assert(posix_spawn(&child, path, &actions, NULL, arguments, environ) == 0);
  posix_spawn_file_actions_destroy(&actions);
  close(output[1]);
  char text[256] = {0};
  size_t length = 0;
  ssize_t count;
  while ((count = read(output[0], text + length, sizeof(text) - 1 - length)) > 0)
    length += (size_t)count;
  close(output[0]);
  int status = 0;
  assert(waitpid(child, &status, 0) == child);
  assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
  // Key up 40, flagsChanged 56, right button up, then nothing goes down.
  char expected[64];
  int released = snprintf(expected, sizeof(expected), "%d 40\n%d 56\n%d 1\n",
                          (int)kCGEventKeyUp, (int)kCGEventFlagsChanged,
                          (int)kCGEventRightMouseUp);
  assert(strncmp(text, expected, (size_t)released) == 0);
  // A SIGTERM sent before the handler restored the default action runs it
  // again. That run has nothing left to release, so only the late post repeats.
  const char *late = "late post -1\n";
  const char *rest = text + released;
  assert(*rest);
  for (; *rest; rest += strlen(late)) assert(strncmp(rest, late, strlen(late)) == 0);
}

int main(int argc, char **argv) {
  if (argc > 1 && strcmp(argv[1], "signal-child") == 0) return signal_child();
  alarm(30);
  source_tests();
  pointer_tests();
  key_tests();
  scroll_tests();
  refusal_tests();
  release_tests();
  caps_tests();
  environment_tests();
  session_tests();
  signal_tests();
  zflow_mac_inject_close();
  clear_posted();
  puts("macOS inject tests passed (fake post, IOKit and session APIs)");
  return 0;
}
