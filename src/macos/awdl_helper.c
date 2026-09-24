#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define LEASE_MS 2000
#define CHECK_MS 100

// The daemon grants one lease at a time, so the guardian needs no lock.
typedef struct {
  void *context;
  int (*get_up)(void *, bool *);
  int (*set_up)(void *, bool);
} Backend;

typedef struct {
  Backend backend;
  bool active;
  bool restore_needed;
  bool original_up;
  bool done;
  uint64_t deadline;
  uint64_t next_check;
} Guardian;

enum Reply { NO_REPLY, ACTIVE_REPLY, HELD_REPLY, RELEASED_REPLY, FAILED_REPLY };
static atomic_bool stopped;

static uint64_t milliseconds(void) {
  struct timespec value;
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return UINT64_MAX;
  return (uint64_t)value.tv_sec * 1000 + (uint64_t)value.tv_nsec / 1000000;
}

static int run_guardian_fds(Backend backend, int input_fd, int output_fd);


static int restore(Guardian *guardian) {
  if (!guardian->restore_needed) return 0;
  if (guardian->backend.set_up(guardian->backend.context,
                               guardian->original_up) != 0) return -1;
  guardian->restore_needed = false;
  guardian->active = false;
  return 0;
}

static Guardian guardian_new(Backend backend, uint64_t now) {
  return (Guardian){.backend = backend, .deadline = now + LEASE_MS};
}

static enum Reply guardian_step(Guardian *guardian, int command, uint64_t now) {
  if (guardian->done) return FAILED_REPLY;
  if (now >= guardian->deadline || command == -1) goto failed;
  if (command == 'A') {
    if (guardian->active) goto failed;
    if (guardian->backend.get_up(guardian->backend.context,
                                 &guardian->original_up) != 0) goto failed;
    guardian->restore_needed = true;
    if (guardian->backend.set_up(guardian->backend.context, false) != 0)
      goto failed;
    guardian->active = true;
    guardian->deadline = now + LEASE_MS;
    guardian->next_check = now + CHECK_MS;
    return ACTIVE_REPLY;
  }
  if (command == 'R') {
    guardian->done = true;
    return restore(guardian) == 0 ? RELEASED_REPLY : FAILED_REPLY;
  }
  if (command == 'H') {
    if (!guardian->active) goto failed;
    guardian->deadline = now + LEASE_MS;
  } else if (command != 0) {
    goto failed;
  }
  if (guardian->active && now >= guardian->next_check) {
    bool up;
    guardian->next_check = now + CHECK_MS;
    if (guardian->backend.get_up(guardian->backend.context, &up) != 0 ||
        (up && guardian->backend.set_up(guardian->backend.context, false) != 0))
      goto failed;
  }
  return command == 'H' ? HELD_REPLY : NO_REPLY;

failed:
  guardian->done = true;
  return FAILED_REPLY;
}

#ifndef ZFLOW_AWDL_HELPER_TEST
#include <dirent.h>
#include <fcntl.h>
#include <grp.h>
#include <net/if.h>
#include <stddef.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/socket.h>

typedef struct { int control_fd; } Native;

static int native_get(void *context, bool *up) {
  Native *native = context;
  struct ifreq request = {0};
  memcpy(request.ifr_name, "awdl0", sizeof("awdl0"));
  if (ioctl(native->control_fd, SIOCGIFFLAGS, &request) != 0) return -1;
  *up = (request.ifr_flags & IFF_UP) != 0;
  return 0;
}

static int native_set(void *context, bool up) {
  Native *native = context;
  struct ifreq request = {0};
  memcpy(request.ifr_name, "awdl0", sizeof("awdl0"));
  if (ioctl(native->control_fd, SIOCGIFFLAGS, &request) != 0) return -1;
  if (((request.ifr_flags & IFF_UP) != 0) == up) return 0;
  if (up) request.ifr_flags |= IFF_UP;
  else request.ifr_flags &= ~IFF_UP;
  return ioctl(native->control_fd, SIOCSIFFLAGS, &request);
}
#endif

static void diagnostic(const char *message) {
  struct pollfd output = {.fd = STDERR_FILENO, .events = POLLOUT};
  if (poll(&output, 1, 0) == 1 && (output.revents & POLLOUT))
    (void)write(STDERR_FILENO, message, strlen(message));
}

static int reply(int output_fd, const char *message) {
  size_t length = strlen(message);
  return write(output_fd, message, length) == (ssize_t)length ? 0 : -1;
}

#ifndef ZFLOW_AWDL_HELPER_TEST
void zflow_awdl_stop(void) { atomic_store(&stopped, true); }

int zflow_awdl_check(void) {
  if (geteuid() != 0 || atomic_load(&stopped)) return -1;
  Native native = {.control_fd = socket(AF_INET, SOCK_DGRAM, 0)};
  if (native.control_fd < 0) return -1;
  bool up;
  int result = native_get(&native, &up);
  close(native.control_fd);
  return result;
}

int zflow_awdl_run(int input_fd, int output_fd) {
  if (geteuid() != 0 || atomic_load(&stopped)) return 1;
  if (fcntl(input_fd, F_SETFL, O_NONBLOCK) != 0 ||
      fcntl(output_fd, F_SETFL, O_NONBLOCK) != 0) return 1;
  Native native = {.control_fd = socket(AF_INET, SOCK_DGRAM, 0)};
  if (native.control_fd < 0) return 1;
  int status = run_guardian_fds((Backend){&native, native_get, native_set},
                                input_fd, output_fd);
  close(native.control_fd);
  return status;
}
#endif

// Rechecks awdl0 every CHECK_MS while leased, and ends the lease when the app
// misses its deadline or closes the pipe.
static int run_guardian_fds(Backend backend, int input_fd, int output_fd) {
  uint64_t now = milliseconds();
  if (now == UINT64_MAX) return 1;
  Guardian guardian = guardian_new(backend, now);
  int status = reply(output_fd, "READY\n") == 0 ? 0 : 1;
  while (status == 0 && !guardian.done) {
    struct pollfd input = {.fd = input_fd, .events = POLLIN};
    uint64_t remaining = guardian.deadline > now ? guardian.deadline - now : 0;
    int wait_ms = remaining < CHECK_MS ? (int)remaining : CHECK_MS;
    int count = poll(&input, 1, wait_ms);
    int command = 0;
    if (atomic_load(&stopped) || (count < 0 && errno != EINTR)) command = -1;
    if (command == 0 && (input.revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL))) {
      unsigned char byte;
      ssize_t received = read(input_fd, &byte, 1);
      if (received == 1) command = byte == 0 ? -1 : byte;
      else if (received == 0 || (errno != EINTR && errno != EAGAIN)) command = -1;
    }
    now = milliseconds();
    if (now == UINT64_MAX) command = -1;
    enum Reply response = guardian_step(&guardian, command, now);
    if (response == ACTIVE_REPLY && reply(output_fd, "ACTIVE\n") != 0) status = 1;
    if (response == HELD_REPLY && reply(output_fd, "HELD\n") != 0) status = 1;
    if (response == RELEASED_REPLY && reply(output_fd, "RELEASED\n") != 0) status = 1;
    if (response == FAILED_REPLY) status = 1;
  }
  for (int attempt = 0; guardian.restore_needed && attempt < 3; attempt++) {
    if (restore(&guardian) == 0) break;
    struct timespec delay = {.tv_nsec = 20000000};
    (void)nanosleep(&delay, NULL);
  }
  if (guardian.restore_needed) {
    diagnostic("zflow AWDL helper: could not restore awdl0; restore it manually\n");
    status = 1;
  } else if (status != 0) {
    diagnostic("zflow AWDL helper: lease ended or command/setup failed; AWDL state restored if acquired\n");
  }
  return status;
}
