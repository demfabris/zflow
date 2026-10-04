#import <AppKit/AppKit.h>
#include <ApplicationServices/ApplicationServices.h>
#include <assert.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>

static void fake_post(CGEventTapLocation, CGEventRef);

// Posting is faked, so these tests never press a media key on the host.
// AppKit still builds the events, so the tests read back real fields.
#define CGEventPost fake_post
#include "../src/macos/media.m"

#define MARK 0x7A666C6F77LL
#define NX_KEYTYPE_SOUND_UP 0
#define NX_KEYTYPE_PLAY 16

static CGEventRef posted[4];
static size_t posted_count;
static bool posted_on_main;

static void found_display(int32_t x, int32_t y, int32_t width, int32_t height,
    const char *id, const char *name, uint32_t width_mm, uint32_t height_mm, void *context) {
  (void)x; (void)y;
  assert(width > 0 && height > 0 && id && *id && name && *name);
  assert((width_mm == 0) == (height_mm == 0));
  ++*(unsigned int *)context;
  printf("Display: %s, %dx%d, %ux%u mm\n", name, width, height, width_mm, height_mm);
}

static void fake_post(CGEventTapLocation tap, CGEventRef event) {
  assert(tap == kCGHIDEventTap);
  assert(posted_count < sizeof(posted) / sizeof(posted[0]));
  posted_on_main |= pthread_main_np() != 0;
  posted[posted_count++] = (CGEventRef)CFRetain(event);
}

// The injector posts from its own thread, never the main one.
static void *post_from_a_thread(void *unused) {
  (void)unused;
  assert(!pthread_main_np());
  unsigned int count = 0;
  // Discovery is also called on the core worker. Main Thread Checker checks
  // the real AppKit calls; this only reads hardware and never changes it.
  if (zflow_mac_displays(found_display, &count)) assert(count > 0);
  assert(zflow_mac_post_media_key(NX_KEYTYPE_SOUND_UP, 1, MARK) == 0);
  assert(zflow_mac_post_media_key(NX_KEYTYPE_PLAY, 0, MARK) == 0);
  return NULL;
}

// What a keyboard's media key sends: NX_SYSDEFINED subtype 8, the key type
// in the top half of data1, and down (0xA) or up (0xB) in data1 and flags.
static void check(CGEventRef event, uint32_t key, int state) {
  assert(CGEventGetType(event) == NX_SYSDEFINED);
  assert(CGEventGetIntegerValueField(event, kCGEventSourceUserData) == MARK);
  assert(CGEventGetFlags(event) == (CGEventFlags)(state << 8));
  NSEvent *event_read = [NSEvent eventWithCGEvent:event];
  assert(event_read.type == NSEventTypeSystemDefined && event_read.subtype == 8);
  assert(event_read.data1 == (((NSInteger)key << 16) | (state << 8)));
  assert(event_read.data2 == -1);
}

int main(void) {
  @autoreleasepool {
    pthread_t thread;
    assert(pthread_create(&thread, NULL, post_from_a_thread, NULL) == 0);
    assert(pthread_join(thread, NULL) == 0);
    assert(posted_count == 2 && !posted_on_main);
    check(posted[0], NX_KEYTYPE_SOUND_UP, 0xA);
    check(posted[1], NX_KEYTYPE_PLAY, 0xB);
    for (size_t i = 0; i < posted_count; i++) CFRelease(posted[i]);

    char buffer[8];
    assert(zflow_mac_frontmost_bundle_id(NULL, sizeof(buffer)) == -1);
    assert(zflow_mac_frontmost_bundle_id(buffer, 0) == -1);
    double delay = 0, interval = 0;
    assert(zflow_mac_appkit_key_repeat(NULL, &interval) == -1);
    assert(zflow_mac_appkit_key_repeat(&delay, NULL) == -1);
  }
  puts("macOS media tests passed (fake post)");
  return 0;
}
