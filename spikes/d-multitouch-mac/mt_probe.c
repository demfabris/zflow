// Spike D probe: does MultitouchSupport.framework still deliver raw contact
// frames on this machine, and do they keep arriving while a
// kCGEventTapOptionDefault event tap swallows mouse/scroll events?
//
//   clang -O2 -Wall -o mt_probe mt_probe.c \
//     -framework CoreFoundation -framework ApplicationServices
//   ./mt_probe [--tap] [--secs N]
//
// Private API resolved via dlopen/dlsym (the framework binary lives in the
// dyld shared cache, not on disk). Struct layout is the long-stable one used
// by OpenMultitouchSupport and Karabiner's MultitouchPrivate.h; the sanity
// check that normalized positions land in [0,1] guards against layout drift.

#include <ApplicationServices/ApplicationServices.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { float x, y; } mtPoint;
typedef struct { mtPoint pos, vel; } mtReadout;

typedef struct {
  int frame;
  double timestamp;
  int identifier;
  int state;
  int fingerId;
  int handId;
  mtReadout normalized;
  float size;
  int zero1;
  float angle;
  float majorAxis;
  float minorAxis;
  mtReadout absoluteVector;
  int zero2[2];
  float zDensity;
} MTTouch;

typedef void *MTDeviceRef;
typedef int (*MTContactCallbackFunction)(MTDeviceRef, MTTouch *, int, double, int);

static CFMutableArrayRef (*MTDeviceCreateList)(void);
static MTDeviceRef (*MTDeviceCreateDefault)(void);
static void (*MTRegisterContactFrameCallback)(MTDeviceRef, MTContactCallbackFunction);
static int (*MTDeviceStart)(MTDeviceRef, int);          // OSStatus
static int (*MTDeviceStop)(MTDeviceRef);                // OSStatus
static bool (*MTDeviceIsRunning)(MTDeviceRef);
static bool (*MTDeviceIsAvailable)(void);
static void (*MTEasyInstallPrintCallbacks)(MTDeviceRef, int, int, int, int, int, int);
static bool (*MTDeviceIsBuiltIn)(MTDeviceRef);
static int (*MTDeviceGetFamilyID)(MTDeviceRef, int *);
static int (*MTDeviceGetDeviceID)(MTDeviceRef, unsigned long long *);
static int (*MTDeviceGetSensorDimensions)(MTDeviceRef, int *, int *);

// written on the MT callback thread, read from the main run loop
static _Atomic long g_frames = 0;        // callback invocations
static _Atomic long g_touch_frames = 0;  // frames with >=1 contact
static _Atomic long g_multi_frames = 0;  // frames with >=3 contacts
static _Atomic int g_max_touches = 0;
static _Atomic double g_first_touch_ts = 0.0;
static _Atomic double g_last_touch_ts = 0.0;
static _Atomic long g_swallowed = 0;
static _Atomic int g_pos_out_of_range = 0;
static MTDeviceRef g_devs[8];
static long g_ndev_reg = 0;
static _Atomic long g_dev_frames[8];

static int contact_cb(MTDeviceRef dev, MTTouch *touches, int n, double ts, int frame) {
  (void)frame;
  atomic_fetch_add(&g_frames, 1);
  for (long i = 0; i < g_ndev_reg; i++)
    if (g_devs[i] == dev) { atomic_fetch_add(&g_dev_frames[i], 1); break; }
  if (n <= 0) return 0;
  atomic_fetch_add(&g_touch_frames, 1);
  double zero = 0.0;
  atomic_compare_exchange_strong(&g_first_touch_ts, &zero, ts);
  atomic_store(&g_last_touch_ts, ts);
  int prev = atomic_load(&g_max_touches);
  while (n > prev && !atomic_compare_exchange_weak(&g_max_touches, &prev, n)) {}
  if (n >= 3) atomic_fetch_add(&g_multi_frames, 1);
  for (int i = 0; i < n; i++) {
    float x = touches[i].normalized.pos.x, y = touches[i].normalized.pos.y;
    if (x < -0.01f || x > 1.01f || y < -0.01f || y > 1.01f)
      atomic_store(&g_pos_out_of_range, 1);
  }
  long tf = atomic_load(&g_touch_frames);
  if (tf % 30 == 1) {  // ~4 lines/s at 125 Hz
    printf("frame ts=%.4f n=%d", ts, n);
    for (int i = 0; i < n && i < 5; i++)
      printf("  [id=%d st=%d x=%.3f y=%.3f sz=%.2f]",
             touches[i].identifier, touches[i].state,
             touches[i].normalized.pos.x, touches[i].normalized.pos.y,
             touches[i].size);
    printf("\n");
    fflush(stdout);
  }
  return 0;
}

static CFMachPortRef g_tap = NULL;

static CGEventRef tap_cb(CGEventTapProxy proxy, CGEventType type, CGEventRef ev, void *info) {
  (void)proxy; (void)info;
  if (type == kCGEventTapDisabledByTimeout) {
    CGEventTapEnable(g_tap, true);
    return ev;
  }
  if (type == kCGEventTapDisabledByUserInput) return ev;  // escape hatch: stay off
  atomic_fetch_add(&g_swallowed, 1);
  return NULL;  // swallow
}

// listen-only tap: proves whether pointer/key events flow while MT is silent
static _Atomic long g_ev_moved = 0, g_ev_scroll = 0, g_ev_key = 0;

static CGEventRef monitor_cb(CGEventTapProxy proxy, CGEventType type, CGEventRef ev, void *info) {
  (void)proxy; (void)info;
  switch (type) {
    case kCGEventMouseMoved:
    case kCGEventLeftMouseDragged: atomic_fetch_add(&g_ev_moved, 1); break;
    case kCGEventScrollWheel: atomic_fetch_add(&g_ev_scroll, 1); break;
    case kCGEventKeyDown: atomic_fetch_add(&g_ev_key, 1); break;
    default: break;
  }
  return ev;
}

static void timeout_cb(CFRunLoopTimerRef t, void *info) {
  (void)t; (void)info;
  CFRunLoopStop(CFRunLoopGetCurrent());
}

static void poll_cb(CFRunLoopTimerRef t, void *info) {
  (void)t; (void)info;
  static long last_moved = 0, last_scroll = 0, last_key = 0;
  long m = atomic_load(&g_ev_moved), s = atomic_load(&g_ev_scroll),
       k = atomic_load(&g_ev_key);
  if (m != last_moved || s != last_scroll || k != last_key) {
    printf("EV moved=%ld scroll=%ld key=%ld | mt_callbacks=%ld\n", m, s, k,
           atomic_load(&g_frames));
    fflush(stdout);
    last_moved = m; last_scroll = s; last_key = k;
  }
  // enough >=3-finger evidence collected: stop early
  if (atomic_load(&g_multi_frames) >= 400) CFRunLoopStop(CFRunLoopGetCurrent());
}

int main(int argc, char **argv) {
  int secs = 60, tap = 0;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--tap")) tap = 1;
    else if (!strcmp(argv[i], "--secs") && i + 1 < argc) secs = atoi(argv[++i]);
  }

  void *h = dlopen(
      "/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport",
      RTLD_NOW);
  if (!h) { fprintf(stderr, "dlopen failed: %s\n", dlerror()); return 1; }
  MTDeviceCreateList = dlsym(h, "MTDeviceCreateList");
  MTDeviceCreateDefault = dlsym(h, "MTDeviceCreateDefault");
  MTRegisterContactFrameCallback = dlsym(h, "MTRegisterContactFrameCallback");
  MTDeviceStart = dlsym(h, "MTDeviceStart");
  MTDeviceStop = dlsym(h, "MTDeviceStop");
  MTDeviceIsRunning = dlsym(h, "MTDeviceIsRunning");
  MTDeviceIsAvailable = dlsym(h, "MTDeviceIsAvailable");
  MTEasyInstallPrintCallbacks = dlsym(h, "MTEasyInstallPrintCallbacks");
  printf("symbols: MTDeviceCreateList=%p MTDeviceCreateDefault=%p "
         "MTRegisterContactFrameCallback=%p MTDeviceStart=%p MTDeviceStop=%p "
         "MTDeviceIsRunning=%p MTDeviceIsAvailable=%p "
         "MTEasyInstallPrintCallbacks=%p\n",
         (void *)MTDeviceCreateList, (void *)MTDeviceCreateDefault,
         (void *)MTRegisterContactFrameCallback, (void *)MTDeviceStart,
         (void *)MTDeviceStop, (void *)MTDeviceIsRunning,
         (void *)MTDeviceIsAvailable, (void *)MTEasyInstallPrintCallbacks);
  if (!MTDeviceCreateList || !MTRegisterContactFrameCallback || !MTDeviceStart) {
    fprintf(stderr, "missing required symbols\n");
    return 1;
  }
  if (MTDeviceIsAvailable)
    printf("MTDeviceIsAvailable: %d\n", MTDeviceIsAvailable());

  int use_default = 0, easyprint = 0;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--default")) use_default = 1;
    else if (!strcmp(argv[i], "--easyprint")) easyprint = 1;
  }

  CFMutableArrayRef devs = NULL;
  long ndev = 0;
  MTDeviceRef single = NULL;
  if (use_default && MTDeviceCreateDefault) {
    single = MTDeviceCreateDefault();
    ndev = single ? 1 : 0;
    printf("MTDeviceCreateDefault: %p\n", single);
  } else {
    devs = MTDeviceCreateList();
    ndev = devs ? CFArrayGetCount(devs) : 0;
    printf("MTDeviceCreateList: %ld device(s)\n", ndev);
  }
  if (ndev == 0) { fprintf(stderr, "no multitouch devices\n"); return 1; }
  MTDeviceIsBuiltIn = dlsym(h, "MTDeviceIsBuiltIn");
  MTDeviceGetFamilyID = dlsym(h, "MTDeviceGetFamilyID");
  MTDeviceGetDeviceID = dlsym(h, "MTDeviceGetDeviceID");
  MTDeviceGetSensorDimensions = dlsym(h, "MTDeviceGetSensorDimensions");
  for (long i = 0; i < ndev; i++) {
    MTDeviceRef d = single ? single : (MTDeviceRef)CFArrayGetValueAtIndex(devs, i);
    int fam = -1, rows = -1, cols = -1;
    unsigned long long did = 0;
    if (MTDeviceGetFamilyID) MTDeviceGetFamilyID(d, &fam);
    if (MTDeviceGetDeviceID) MTDeviceGetDeviceID(d, &did);
    if (MTDeviceGetSensorDimensions) MTDeviceGetSensorDimensions(d, &rows, &cols);
    printf("device[%ld]: family=%d id=%llu builtin=%d sensor=%dx%d\n", i, fam,
           did, MTDeviceIsBuiltIn ? (int)MTDeviceIsBuiltIn(d) : -1, rows, cols);
    if (i < 8) { g_devs[i] = d; g_ndev_reg = i + 1; }
    MTRegisterContactFrameCallback(d, contact_cb);
    if (easyprint && MTEasyInstallPrintCallbacks)
      MTEasyInstallPrintCallbacks(d, 1, 0, 0, 0, 0, 0);
    int st = MTDeviceStart(d, 0);
    printf("MTDeviceStart[%ld] status=%d running=%d\n", i, st,
           MTDeviceIsRunning ? (int)MTDeviceIsRunning(d) : -1);
  }

  // always-on listen-only tap for input-flow correlation (never swallows)
  CGEventMask mon_mask = CGEventMaskBit(kCGEventMouseMoved) |
                         CGEventMaskBit(kCGEventLeftMouseDragged) |
                         CGEventMaskBit(kCGEventScrollWheel) |
                         CGEventMaskBit(kCGEventKeyDown);
  CFMachPortRef mon = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap,
                                       kCGEventTapOptionListenOnly, mon_mask,
                                       monitor_cb, NULL);
  if (mon) {
    CFRunLoopSourceRef msrc = CFMachPortCreateRunLoopSource(NULL, mon, 0);
    CFRunLoopAddSource(CFRunLoopGetCurrent(), msrc, kCFRunLoopCommonModes);
    CGEventTapEnable(mon, true);
    printf("monitor tap: active (listen-only)\n");
  } else {
    printf("monitor tap: FAILED to create\n");
  }

  if (tap) {
    CGEventMask mask = CGEventMaskBit(kCGEventMouseMoved) |
                       CGEventMaskBit(kCGEventScrollWheel) |
                       CGEventMaskBit(kCGEventLeftMouseDragged) |
                       CGEventMaskBit(kCGEventRightMouseDragged);
    g_tap = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap,
                             kCGEventTapOptionDefault, mask, tap_cb, NULL);
    if (!g_tap) {
      fprintf(stderr, "TAP: CGEventTapCreate failed (accessibility permission "
                      "not granted to this terminal?)\n");
      return 2;
    }
    CFRunLoopSourceRef src = CFMachPortCreateRunLoopSource(NULL, g_tap, 0);
    CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
    CGEventTapEnable(g_tap, true);
    printf("TAP ACTIVE: mouse-move/drag/scroll swallowed for max %d s\n", secs);
  }

  printf("listening for %d s; touch the trackpad (3+ fingers)\n", secs);
  fflush(stdout);

  CFRunLoopTimerRef deadline = CFRunLoopTimerCreate(
      NULL, CFAbsoluteTimeGetCurrent() + secs, 0, 0, 0, timeout_cb, NULL);
  CFRunLoopAddTimer(CFRunLoopGetCurrent(), deadline, kCFRunLoopCommonModes);
  CFRunLoopTimerRef poll = CFRunLoopTimerCreate(
      NULL, CFAbsoluteTimeGetCurrent() + 0.5, 0.5, 0, 0, poll_cb, NULL);
  CFRunLoopAddTimer(CFRunLoopGetCurrent(), poll, kCFRunLoopCommonModes);
  CFRunLoopRun();

  if (g_tap) CGEventTapEnable(g_tap, false);
  for (long i = 0; i < ndev; i++)
    if (MTDeviceStop) MTDeviceStop((MTDeviceRef)CFArrayGetValueAtIndex(devs, i));

  long frames = atomic_load(&g_frames);
  long touch_frames = atomic_load(&g_touch_frames);
  long multi = atomic_load(&g_multi_frames);
  double span = atomic_load(&g_last_touch_ts) - atomic_load(&g_first_touch_ts);
  printf("SUMMARY: callbacks=%ld touch_frames=%ld multi3_frames=%ld "
         "max_touches=%d active_span=%.2fs rate=%.1fHz pos_in_range=%s "
         "swallowed=%ld\n",
         frames, touch_frames, multi, atomic_load(&g_max_touches), span,
         span > 0.5 ? (double)touch_frames / span : 0.0,
         atomic_load(&g_pos_out_of_range) ? "NO" : "yes",
         atomic_load(&g_swallowed));
  for (long i = 0; i < g_ndev_reg; i++)
    printf("  device[%ld] frames=%ld\n", i, atomic_load(&g_dev_frames[i]));
  return 0;
}
