#import <AppKit/AppKit.h>
#include <ApplicationServices/ApplicationServices.h>
#include <math.h>
#include <stdint.h>
#include <string.h>

typedef void (*display_fn)(int32_t, int32_t, int32_t, int32_t,
    const char *, const char *, uint32_t, uint32_t, void *);

int zflow_mac_displays(display_fn callback, void *context) {
  @autoreleasepool {
    CGDirectDisplayID ids[16]; uint32_t count = 0;
    if (CGGetActiveDisplayList(16, ids, &count) != kCGErrorSuccess || !count) return 0;
    for (uint32_t i = 0; i < count; ++i) {
      if (CGDisplayMirrorsDisplay(ids[i]) != kCGNullDirectDisplay) continue;
      CFUUIDRef uuid = CGDisplayCreateUUIDFromDisplayID(ids[i]);
      if (!uuid) return 0;
      NSString *identity = CFBridgingRelease(CFUUIDCreateString(NULL, uuid));
      CFRelease(uuid);
      NSString *name = CGDisplayIsBuiltin(ids[i]) ? @"Built-in display" : @"External display";
      for (NSScreen *screen in NSScreen.screens) {
        if ([screen.deviceDescription[@"NSScreenNumber"] unsignedIntValue] == ids[i]) {
          name = screen.localizedName; break;
        }
      }
      CGRect r = CGDisplayBounds(ids[i]);
      CGSize size = CGDisplayScreenSize(ids[i]);
      if (fmod(CGDisplayRotation(ids[i]), 180.0) != 0) {
        double swap = size.width; size.width = size.height; size.height = swap;
      }
      BOOL sized = size.width >= 10 && size.height >= 10 && size.width <= 4000 && size.height <= 4000;
      callback((int32_t)lround(r.origin.x), (int32_t)lround(r.origin.y),
          (int32_t)lround(r.size.width), (int32_t)lround(r.size.height),
          identity.UTF8String, name.UTF8String, sized ? (uint32_t)lround(size.width) : 0,
          sized ? (uint32_t)lround(size.height) : 0, context);
    }
    return 1;
  }
}

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
