#import <AppKit/AppKit.h>
#include <ImageIO/ImageIO.h>
#include <assert.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

// Every check uses a pasteboard with a unique name, never the general one,
// so the tests leave the person's clipboard alone.
#include "../src/macos/pasteboard.m"

static const uint8_t PNG_SIGNATURE[] = {0x89, 'P', 'N', 'G', '\r', '\n', 0x1a, '\n'};

static NSPasteboard *board;
static const char *board_name;

typedef struct {
  int status;
  uint32_t kind;
  size_t length;
  int64_t count;
  NSData *data;
} Read;

static Read read_board(const int64_t *unchanged_at, size_t limit) {
  Read read = {0};
  uint8_t *data = NULL;
  read.status = zflow_mac_pasteboard_read(board_name, unchanged_at, limit, &read.kind, &data,
                                          &read.length, &read.count);
  if (data) {
    read.data = [NSData dataWithBytes:data length:read.length];
    zflow_mac_pasteboard_free(data);
  }
  return read;
}

static int64_t write_board(uint32_t kind, NSData *data) {
  int64_t count = -1;
  assert(zflow_mac_pasteboard_write(board_name, kind, data.bytes, data.length, &count) == 0);
  return count;
}

// The checks run on a thread of their own, as the app's do, and look at the
// pasteboard on the main thread.
static void on_main_thread(void (^work)(void)) {
  dispatch_sync(dispatch_get_main_queue(), work);
}

// A width x height image, encoded as `type` by ImageIO.
static NSData *image(CFStringRef type, size_t width, size_t height) {
  CGColorSpaceRef space = CGColorSpaceCreateDeviceRGB();
  CGContextRef context = CGBitmapContextCreate(NULL, width, height, 8, 0, space,
                                               (CGBitmapInfo)kCGImageAlphaPremultipliedLast);
  CGContextSetRGBFillColor(context, 0.2, 0.4, 0.8, 1);
  CGContextFillRect(context, CGRectMake(0, 0, width, height));
  CGImageRef picture = CGBitmapContextCreateImage(context);
  NSMutableData *encoded = [NSMutableData data];
  CGImageDestinationRef destination =
      CGImageDestinationCreateWithData((__bridge CFMutableDataRef)encoded, type, 1, NULL);
  CGImageDestinationAddImage(destination, picture, NULL);
  assert(CGImageDestinationFinalize(destination));
  CFRelease(destination);
  CGImageRelease(picture);
  CGContextRelease(context);
  CGColorSpaceRelease(space);
  return encoded;
}

static void check_size(NSData *png, size_t width, size_t height) {
  assert(png.length > sizeof(PNG_SIGNATURE));
  assert(memcmp(png.bytes, PNG_SIGNATURE, sizeof(PNG_SIGNATURE)) == 0);
  CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)png, NULL);
  CGImageRef picture = CGImageSourceCreateImageAtIndex(source, 0, NULL);
  assert(CGImageGetWidth(picture) == width && CGImageGetHeight(picture) == height);
  CGImageRelease(picture);
  CFRelease(source);
}

static void *checks(void *unused) {
  (void)unused;
  @autoreleasepool {
    assert(!pthread_main_np());
    Read empty = read_board(NULL, 1024);
    assert(empty.status == 0 && empty.kind == ZFLOW_CLIP_EMPTY && !empty.data);

    // Text goes on with the marker, and reads back byte for byte.
    NSData *text = [@"zflow ✓ text" dataUsingEncoding:NSUTF8StringEncoding];
    __block NSInteger before = 0;
    on_main_thread(^{
      before = board.changeCount;
    });
    int64_t written = write_board(ZFLOW_CLIP_TEXT, text);
    on_main_thread(^{
      // Clearing moved the count once, and writing did not move it again.
      assert(written == before + 1);
      assert(board.changeCount == written);
      assert([board.types containsObject:NSPasteboardTypeString]);
      assert([board.types containsObject:ZFLOW_CLIP_MARKER]);
      assert([[board stringForType:NSPasteboardTypeString] isEqualToString:@"zflow ✓ text"]);
    });
    Read ours = read_board(&written, 1024);
    assert(ours.status == 0 && ours.kind == ZFLOW_CLIP_UNCHANGED && !ours.data);
    assert(ours.count == written);
    Read again = read_board(NULL, 1024);
    assert(again.kind == ZFLOW_CLIP_TEXT && [again.data isEqualToData:text]);

    // Over the limit, only the size comes back.
    Read large = read_board(NULL, text.length - 1);
    assert(large.status == 0 && large.kind == ZFLOW_CLIP_TEXT);
    assert(large.length == text.length && !large.data);

    // Text wins over an image copied with it, and any other change is news.
    NSData *png = image(CFSTR("public.png"), 3, 2);
    on_main_thread(^{
      [board clearContents];
      [board setString:@"caption" forType:NSPasteboardTypeString];
      [board setData:png forType:NSPasteboardTypePNG];
    });
    Read caption = read_board(&written, 1024);
    assert(caption.kind == ZFLOW_CLIP_TEXT && caption.count != written);
    assert([caption.data isEqualToData:[@"caption" dataUsingEncoding:NSUTF8StringEncoding]]);

    // A password manager's copy, or one meant only for a moment, stays here,
    // though it is news.
    for (NSString *marker in @[ CONCEALED_TYPE, TRANSIENT_TYPE ]) {
      on_main_thread(^{
        [board clearContents];
        [board setString:@"hunter2" forType:NSPasteboardTypeString];
        [board setData:[NSData data] forType:marker];
      });
      Read secret = read_board(&written, 1024);
      assert(secret.status == 0 && secret.kind == ZFLOW_CLIP_EMPTY);
      assert(!secret.data && secret.length == 0 && secret.count != written);
    }

    // A PNG reads as it is.
    on_main_thread(^{
      [board clearContents];
      [board setData:png forType:NSPasteboardTypePNG];
    });
    Read picture = read_board(NULL, 1 << 20);
    assert(picture.kind == ZFLOW_CLIP_PNG && [picture.data isEqualToData:png]);

    // A TIFF alone, as from a screenshot tool, becomes a PNG of the same size.
    NSData *tiff = image(CFSTR("public.tiff"), 5, 4);
    on_main_thread(^{
      [board clearContents];
      [board setData:tiff forType:NSPasteboardTypeTIFF];
    });
    Read converted = read_board(NULL, 1 << 20);
    assert(converted.status == 0 && converted.kind == ZFLOW_CLIP_PNG);
    check_size(converted.data, 5, 4);

    // Bytes that are not an image fail instead of going out as one.
    on_main_thread(^{
      [board clearContents];
      [board setData:[@"not a tiff" dataUsingEncoding:NSUTF8StringEncoding]
             forType:NSPasteboardTypeTIFF];
    });
    assert(read_board(NULL, 1 << 20).status == -1);

    // A PNG goes on as PNG with the marker, and nothing else.
    int64_t image_written = write_board(ZFLOW_CLIP_PNG, png);
    on_main_thread(^{
      NSArray<NSPasteboardType> *types = board.types;
      assert([types containsObject:NSPasteboardTypePNG]);
      assert([types containsObject:ZFLOW_CLIP_MARKER]);
      assert(![types containsObject:NSPasteboardTypeString]);
      assert(board.changeCount == image_written);
    });
    Read back = read_board(NULL, 1 << 20);
    assert(back.kind == ZFLOW_CLIP_PNG && [back.data isEqualToData:png]);

    // Calls that cannot work are refused before touching the pasteboard.
    int64_t count = 0;
    uint8_t *data = NULL;
    size_t length = 0;
    assert(zflow_mac_pasteboard_read(board_name, NULL, 8, NULL, &data, &length, &count) == -1);
    assert(zflow_mac_pasteboard_write(board_name, 7, png.bytes, png.length, &count) == -1);
    assert(zflow_mac_pasteboard_write(board_name, ZFLOW_CLIP_TEXT, text.bytes, 0, &count) == -1);
    assert(zflow_mac_pasteboard_write(board_name, ZFLOW_CLIP_TEXT, NULL, 4, &count) == -1);

    // A busy main thread makes a call give up, and the write it gave up on
    // never happens later.
    zflow_pasteboard_wait = 0.05;
    dispatch_semaphore_t busy = dispatch_semaphore_create(0);
    dispatch_async(dispatch_get_main_queue(), ^{
      dispatch_semaphore_signal(busy);
      usleep(300 * 1000);
    });
    dispatch_semaphore_wait(busy, DISPATCH_TIME_FOREVER);
    assert(zflow_mac_pasteboard_write(board_name, ZFLOW_CLIP_TEXT, text.bytes, text.length,
                                      &count) == -1);
    assert(read_board(NULL, 8).status == -1);
    zflow_pasteboard_wait = 3.0;
    on_main_thread(^{
      assert(board.changeCount == image_written);
      assert(![board.types containsObject:NSPasteboardTypeString]);
    });

    dispatch_async(dispatch_get_main_queue(), ^{
      CFRunLoopStop(CFRunLoopGetMain());
    });
  }
  return NULL;
}

int main(void) {
  @autoreleasepool {
    board = [NSPasteboard pasteboardWithUniqueName];
    board_name = strdup(board.name.UTF8String);
    // On the main thread a call runs in place.
    Read direct = read_board(NULL, 8);
    assert(direct.status == 0 && direct.kind == ZFLOW_CLIP_EMPTY);
    pthread_t thread;
    assert(pthread_create(&thread, NULL, checks, NULL) == 0);
    // Answers the checks' calls, as the app's main thread does.
    CFRunLoopRun();
    assert(pthread_join(thread, NULL) == 0);
    [board releaseGlobally];
  }
  puts("macOS pasteboard tests passed (uniquely named pasteboard)");
  return 0;
}
