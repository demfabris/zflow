#include <stdbool.h>
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

typedef struct {
  void *context;
  int (*lock)(void *);
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
static volatile sig_atomic_t stopped;

static uint64_t milliseconds(void) {
  struct timespec value;
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return UINT64_MAX;
  return (uint64_t)value.tv_sec * 1000 + (uint64_t)value.tv_nsec / 1000000;
}

static int run_guardian(Backend backend, int route_fd);

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
    if (guardian->active || guardian->backend.lock(guardian->backend.context) != 0)
      goto failed;
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
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>

typedef struct { int control_fd; int lock_fd; } Native;
static void stop_signal(int number) { (void)number; stopped = 1; }

static int native_lock(void *context) {
  Native *native = context;
  native->lock_fd = open("/var/run/zflow-awdl.lock",
                         O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK,
                         0600);
  if (native->lock_fd < 0) return -1;
  struct stat info;
  if (fstat(native->lock_fd, &info) != 0 || !S_ISREG(info.st_mode) ||
      info.st_uid != 0 || info.st_nlink != 1 || (info.st_mode & 0077) != 0) {
    errno = EPERM;
    return -1;
  }
  return flock(native->lock_fd, LOCK_EX | LOCK_NB);
}

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

static int reply(const char *message) {
  size_t length = strlen(message);
  return write(STDOUT_FILENO, message, length) == (ssize_t)length ? 0 : -1;
}

#ifndef ZFLOW_AWDL_HELPER_TEST
static int close_inherited(void) {
  DIR *directory = opendir("/dev/fd");
  if (!directory) return -1;
  struct dirent *entry;
  while ((entry = readdir(directory)) != NULL) {
    char *end;
    long fd = strtol(entry->d_name, &end, 10);
    if (*end == '\0' && fd > 2 && fd <= INT32_MAX && fd != dirfd(directory))
      close((int)fd);
  }
  return closedir(directory);
}

int main(int argc, char **argv) {
  (void)argv;
  if (argc != 1 || geteuid() != 0) {
    diagnostic("zflow AWDL helper: requires an administrator-installed helper; no arguments accepted\n");
    return 1;
  }
  struct stat input, output;
  if (fstat(STDIN_FILENO, &input) != 0 || !S_ISFIFO(input.st_mode) ||
      fstat(STDOUT_FILENO, &output) != 0 || !S_ISFIFO(output.st_mode)) return 1;
  if (close_inherited() != 0) return 1;
  umask(077);
  if (setgroups(0, NULL) != 0 || setgid(getgid()) != 0) return 1;
  if (fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) != 0 ||
      fcntl(STDOUT_FILENO, F_SETFL, O_NONBLOCK) != 0) return 1;
  struct sigaction action = {0};
  action.sa_handler = stop_signal;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGTERM, &action, NULL) != 0 ||
      sigaction(SIGINT, &action, NULL) != 0 ||
      sigaction(SIGHUP, &action, NULL) != 0) return 1;
  action.sa_handler = SIG_IGN;
  if (sigaction(SIGPIPE, &action, NULL) != 0) return 1;
  Native native = {.control_fd = socket(AF_INET, SOCK_DGRAM, 0), .lock_fd = -1};
  if (native.control_fd < 0) return 1;
  int route_fd = socket(AF_ROUTE, SOCK_RAW, 0);
  if (route_fd >= 0 && fcntl(route_fd, F_SETFL, O_NONBLOCK) != 0) {
    close(route_fd);
    route_fd = -1;
  }
  int status = run_guardian((Backend){&native, native_lock, native_get, native_set},
                            route_fd);
  if (route_fd >= 0) close(route_fd);
  close(native.control_fd);
  if (native.lock_fd >= 0) close(native.lock_fd);
  return status;
}
#endif

static int run_guardian(Backend backend, int route_fd) {
  uint64_t now = milliseconds();
  if (now == UINT64_MAX) return 1;
  Guardian guardian = guardian_new(backend, now);
  uint64_t route_resume = 0;
  int status = reply("READY\n") == 0 ? 0 : 1;
  while (status == 0 && !guardian.done) {
    struct pollfd fds[2] = {{.fd = STDIN_FILENO, .events = POLLIN},
                           {.fd = now >= route_resume ? route_fd : -1,
                            .events = POLLIN}};
    uint64_t remaining = guardian.deadline > now ? guardian.deadline - now : 0;
    int wait_ms = remaining < CHECK_MS ? (int)remaining : CHECK_MS;
    int count = poll(fds, 2, wait_ms);
    int command = 0;
    if (stopped || (count < 0 && errno != EINTR)) command = -1;
    if (command == 0 && (fds[0].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL))) {
      unsigned char byte;
      ssize_t received = read(STDIN_FILENO, &byte, 1);
      if (received == 1) command = byte == 0 ? -1 : byte;
      else if (received == 0 || (errno != EINTR && errno != EAGAIN)) command = -1;
    }
    if (fds[1].revents & POLLIN) {
      char events[8192];
      if (read(route_fd, events, sizeof(events)) == 0) route_fd = -1;
      route_resume = milliseconds() + CHECK_MS;
    }
    if (fds[1].revents & (POLLHUP | POLLERR | POLLNVAL)) route_fd = -1;
    now = milliseconds();
    if (now == UINT64_MAX) command = -1;
    enum Reply response = guardian_step(&guardian, command, now);
    if (response == ACTIVE_REPLY && reply("ACTIVE\n") != 0) status = 1;
    if (response == HELD_REPLY && reply("HELD\n") != 0) status = 1;
    if (response == RELEASED_REPLY && reply("RELEASED\n") != 0) status = 1;
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
