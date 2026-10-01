#import <AppKit/AppKit.h>
#include <ApplicationServices/ApplicationServices.h>
#include <math.h>
#include <stdint.h>
#include <string.h>

// Media keys have no CGEvent constructor. AppKit builds the NX_SYSDEFINED
// event a keyboard's volume and play keys send: subtype 8, the NX_KEYTYPE in
// the top half of data1 and key down (0xA) or up (0xB) below it.
int zflow_mac_post_media_key(uint32_t key, int down, int64_t mark) {
  @autoreleasepool {
    NSInteger state = down ? 0xA : 0xB;
    NSEvent *event = [NSEvent otherEventWithType:NSEventTypeSystemDefined
                                        location:NSZeroPoint
                                   modifierFlags:(NSEventModifierFlags)(state << 8)
                                       timestamp:0
                                    windowNumber:0
                                         context:nil
                                         subtype:8
                                           data1:((NSInteger)key << 16) | (state << 8)
                                           data2:-1];
    CGEventRef posted = event.CGEvent;
    if (!posted) return -1;
    CGEventSetIntegerValueField(posted, kCGEventSourceUserData, mark);
    CGEventPost(kCGHIDEventTap, posted);
    return 0;
  }
}

// Writes the front app's bundle identifier and returns its length, or -1.
int zflow_mac_frontmost_bundle_id(char *buffer, size_t capacity) {
  if (!buffer || capacity == 0) return -1;
  @autoreleasepool {
    NSString *bundle = NSWorkspace.sharedWorkspace.frontmostApplication.bundleIdentifier;
    if (!bundle || ![bundle getCString:buffer maxLength:capacity encoding:NSUTF8StringEncoding])
      return -1;
    return (int)strlen(buffer);
  }
}

// The user's double-click speed in seconds.
double zflow_mac_double_click_interval(void) {
  return NSEvent.doubleClickInterval;
}

// The key repeat AppKit reports, in seconds. The last resort when IOHIDSystem
// and the preferences have no rate.
int zflow_mac_appkit_key_repeat(double *delay, double *interval) {
  if (!delay || !interval) return -1;
  *delay = NSEvent.keyRepeatDelay;
  *interval = NSEvent.keyRepeatInterval;
  return isfinite(*delay) && isfinite(*interval) && *delay > 0 && *interval > 0 ? 0 : -1;
}
