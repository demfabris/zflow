#import <AppKit/AppKit.h>
#include <ImageIO/ImageIO.h>
#include <dispatch/dispatch.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// AppKit's pasteboard is only safe on the main thread, so each call below
// does its pasteboard part there and waits a bounded time for it. The main
// thread never waits on the caller, so a busy main thread only makes the
// call fail. Image conversion and copying stay on the caller's thread.

// What a read found. Text and PNG match ClipKind's wire codes.
enum {
  ZFLOW_CLIP_EMPTY = 0,
  ZFLOW_CLIP_TEXT = 1,
  ZFLOW_CLIP_PNG = 2,
  // The change count was still the one the caller wrote, so nothing was read.
  ZFLOW_CLIP_UNCHANGED = 3,
};

// Written beside every clip zflow puts on the pasteboard, so clipboard
// managers and anyone listing its types can tell where it came from.
static NSString *const ZFLOW_CLIP_MARKER = @"io.zflow.clip";

// How long a call waits for the main thread, in seconds. Tests shorten it.
static double zflow_pasteboard_wait = 3.0;

enum { WORK_PENDING, WORK_RUNNING, WORK_ABANDONED };

// Runs `work` on the main thread, in place when already there. Returns false
// when the main thread did not start it within the wait; it then never runs.
static bool on_main(void (^work)(void)) {
  if (pthread_main_np()) {
    work();
    return true;
  }
  dispatch_semaphore_t done = dispatch_semaphore_create(0);
  __block _Atomic int state = WORK_PENDING;
  dispatch_async(dispatch_get_main_queue(), ^{
    int pending = WORK_PENDING;
    if (!atomic_compare_exchange_strong(&state, &pending, WORK_RUNNING)) return;
    work();
    dispatch_semaphore_signal(done);
  });
  int64_t wait = (int64_t)(zflow_pasteboard_wait * (double)NSEC_PER_SEC);
  if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, wait)) == 0) return true;
  int pending = WORK_PENDING;
  if (atomic_compare_exchange_strong(&state, &pending, WORK_ABANDONED)) return false;
  // The main thread started it as the wait ended, and finishes it without
  // waiting on anyone.
  dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
  return true;
}

static NSPasteboard *pasteboard_named(NSString *name) {
  return name ? [NSPasteboard pasteboardWithName:name] : NSPasteboard.generalPasteboard;
}

// The first image in `tiff`, as PNG, or nil. ImageIO is safe off the main thread.
static NSData *png_from_tiff(NSData *tiff) {
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)tiff, NULL);
  if (!source) return nil;
  NSMutableData *png = [NSMutableData data];
  CGImageDestinationRef destination =
      CGImageDestinationCreateWithData((__bridge CFMutableDataRef)png, CFSTR("public.png"), 1, NULL);
  bool converted = false;
  if (destination && CGImageSourceGetCount(source) > 0) {
    CGImageDestinationAddImageFromSource(destination, source, 0, NULL);
    converted = CGImageDestinationFinalize(destination);
  }
  if (destination) CFRelease(destination);
  CFRelease(source);
  return converted ? png : nil;
}

// Reads the pasteboard `name`, or the general one when it is NULL: UTF-8
// text if there is any, else a PNG, else a TIFF turned into a PNG. When the
// change count is still `*unchanged_at`, reads nothing and says so in `kind`.
// `length` is the clip's size; `data` gets a copy only when that is at most
// `limit`, to free with zflow_mac_pasteboard_free. Returns 0, or -1 when the
// main thread did not answer in time or an image could not be converted.
int zflow_mac_pasteboard_read(const char *name, const int64_t *unchanged_at, size_t limit,
                              uint32_t *kind, uint8_t **data, size_t *length,
                              int64_t *change_count) {
  if (!kind || !data || !length || !change_count) return -1;
  *kind = ZFLOW_CLIP_EMPTY;
  *data = NULL;
  *length = 0;
  @autoreleasepool {
    NSString *board_name = name ? [NSString stringWithUTF8String:name] : nil;
    bool skip = unchanged_at != NULL;
    int64_t unchanged = skip ? *unchanged_at : 0;
    __block NSInteger count = 0;
    __block NSData *found = nil;
    __block uint32_t found_kind = ZFLOW_CLIP_EMPTY;
    __block bool tiff = false;
    bool answered = on_main(^{
      NSPasteboard *board = pasteboard_named(board_name);
      count = board.changeCount;
      if (skip && count == unchanged) {
        found_kind = ZFLOW_CLIP_UNCHANGED;
        return;
      }
      NSString *text = [board stringForType:NSPasteboardTypeString];
      if (text) {
        found = [text dataUsingEncoding:NSUTF8StringEncoding];
        found_kind = ZFLOW_CLIP_TEXT;
      } else if ((found = [board dataForType:NSPasteboardTypePNG])) {
        found_kind = ZFLOW_CLIP_PNG;
      } else if ((found = [board dataForType:NSPasteboardTypeTIFF])) {
        found_kind = ZFLOW_CLIP_PNG;
        tiff = true;
      }
    });
    if (!answered) return -1;
    *change_count = count;
    *kind = found_kind;
    if (!found) return 0;
    if (tiff && !(found = png_from_tiff(found))) return -1;
    *length = found.length;
    if (found.length == 0 || found.length > limit) return 0;
    *data = malloc(found.length);
    if (!*data) return -1;
    memcpy(*data, found.bytes, found.length);
    return 0;
  }
}

void zflow_mac_pasteboard_free(uint8_t *data) {
  free(data);
}

// Puts `length` bytes of `kind`, text or PNG, on the pasteboard `name`, or the
// general one when it is NULL, beside zflow's marker. Sets `change_count` to
// the count after the write. Returns 0, or -1 when the main thread did not
// answer in time or the pasteboard refused.
int zflow_mac_pasteboard_write(const char *name, uint32_t kind, const uint8_t *data, size_t length,
                               int64_t *change_count) {
  if (!data || length == 0 || !change_count) return -1;
  if (kind != ZFLOW_CLIP_TEXT && kind != ZFLOW_CLIP_PNG) return -1;
  @autoreleasepool {
    NSString *board_name = name ? [NSString stringWithUTF8String:name] : nil;
    NSData *bytes = [NSData dataWithBytes:data length:length];
    NSPasteboardType type = kind == ZFLOW_CLIP_TEXT ? NSPasteboardTypeString : NSPasteboardTypePNG;
    __block NSInteger count = 0;
    __block BOOL written = NO;
    bool answered = on_main(^{
      NSPasteboardItem *item = [NSPasteboardItem new];
      if (![item setData:bytes forType:type] ||
          ![item setData:[NSData data] forType:ZFLOW_CLIP_MARKER])
        return;
      NSPasteboard *board = pasteboard_named(board_name);
      [board clearContents];
      written = [board writeObjects:@[ item ]];
      count = board.changeCount;
    });
    if (!answered || !written) return -1;
    *change_count = count;
    return 0;
  }
}
