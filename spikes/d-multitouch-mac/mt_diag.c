// Spike D diag: is this process allowed to listen to raw HID input?
//   clang -O2 -o mt_diag mt_diag.c -framework IOKit -framework CoreFoundation
//   ./mt_diag            # check only
//   ./mt_diag --request  # trigger the Input Monitoring TCC prompt
#include <IOKit/hidsystem/IOHIDLib.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

static const char *name(IOHIDAccessType t) {
  switch (t) {
    case kIOHIDAccessTypeGranted: return "granted";
    case kIOHIDAccessTypeDenied: return "denied";
    default: return "unknown (not yet asked)";
  }
}

int main(int argc, char **argv) {
  printf("ListenEvent (input monitoring): %s\n",
         name(IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)));
  printf("PostEvent   (accessibility):    %s\n",
         name(IOHIDCheckAccess(kIOHIDRequestTypePostEvent)));
  if (argc > 1 && !strcmp(argv[1], "--request")) {
    bool ok = IOHIDRequestAccess(kIOHIDRequestTypeListenEvent);
    printf("IOHIDRequestAccess(ListenEvent) -> %s\n", ok ? "granted" : "not granted");
  }
  return 0;
}
