#define ZFLOW_AWDL_HELPER_TEST
#include "../src/macos/awdl_helper.c"
#include <assert.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>

typedef struct {
  bool up;
  bool locked;
  bool fail_get;
  int fail_sets;
  int sets;
} Fake;

static int fake_lock(void *context) {
  Fake *fake = context;
  if (fake->locked) return -1;
  fake->locked = true;
  return 0;
}

static int fake_get(void *context, bool *up) {
  Fake *fake = context;
  if (fake->fail_get) return -1;
  *up = fake->up;
  return 0;
}

static int fake_set(void *context, bool up) {
  Fake *fake = context;
  fake->sets++;
  if (fake->fail_sets > 0) {
    fake->fail_sets--;
    return -1;
  }
  fake->up = up;
  return 0;
}

static Backend backend(Fake *fake) {
  return (Backend){fake, fake_lock, fake_get, fake_set};
}

static void state_machine_tests(void) {
  for (int initial = 0; initial <= 1; initial++) {
    Fake fake = {.up = initial};
    Guardian guardian = guardian_new(backend(&fake), 0);
    assert(fake.sets == 0);
    assert(guardian_step(&guardian, 'A', 10) == ACTIVE_REPLY);
    assert(!fake.up);
    assert(guardian_step(&guardian, 'H', 1500) == HELD_REPLY);
    assert(guardian.deadline == 3500);
    fake.up = true;
    assert(guardian_step(&guardian, 0, 1600) == NO_REPLY);
    assert(!fake.up);
    assert(guardian_step(&guardian, 'R', 2000) == RELEASED_REPLY);
    assert(fake.up == (bool)initial);
    assert(!guardian.restore_needed);
  }
  const int end_commands[] = {-1, 'X', 'A', 0};
  for (size_t i = 0; i < sizeof(end_commands) / sizeof(end_commands[0]); i++) {
    Fake fake = {.up = true};
    Guardian guardian = guardian_new(backend(&fake), 0);
    assert(guardian_step(&guardian, 'A', 10) == ACTIVE_REPLY);
    assert(guardian_step(&guardian, end_commands[i], i == 3 ? 2010 : 20) == FAILED_REPLY);
    assert(restore(&guardian) == 0);
    assert(fake.up);
  }
  Fake fake = {.up = true};
  Guardian guardian = guardian_new(backend(&fake), 0);
  assert(guardian_step(&guardian, 0, LEASE_MS) == FAILED_REPLY);
  assert(fake.sets == 0);
  guardian = guardian_new(backend(&fake), 0);
  assert(guardian_step(&guardian, 'H', 1) == FAILED_REPLY);
  assert(fake.sets == 0);
  guardian = guardian_new(backend(&fake), 0);
  fake.fail_sets = 1;
  assert(guardian_step(&guardian, 'A', 1) == FAILED_REPLY);
  assert(guardian.restore_needed);
  assert(restore(&guardian) == 0);
  assert(fake.up);

  fake = (Fake){.up = true};
  guardian = guardian_new(backend(&fake), 0);
  assert(guardian_step(&guardian, 'A', 1) == ACTIVE_REPLY);
  Guardian second = guardian_new(backend(&fake), 0);
  assert(guardian_step(&second, 'A', 2) == FAILED_REPLY);
  assert(!second.restore_needed);
  assert(!fake.up);
  fake.fail_sets = 1;
  assert(guardian_step(&guardian, 'R', 3) == FAILED_REPLY);
  assert(guardian.restore_needed);
  assert(restore(&guardian) == 0);
  assert(fake.up);

  fake = (Fake){.up = true};
  guardian = guardian_new(backend(&fake), 0);
  assert(guardian_step(&guardian, 'A', 1) == ACTIVE_REPLY);
  fake.fail_get = true;
  assert(guardian_step(&guardian, 0, 101) == FAILED_REPLY);
  assert(restore(&guardian) == 0);
  assert(fake.up);
}

static void expect_line(int fd, const char *expected) {
  for (size_t i = 0; i < strlen(expected); i++) {
    struct pollfd input = {.fd = fd, .events = POLLIN};
    assert(poll(&input, 1, 1000) == 1);
    char byte;
    assert(read(fd, &byte, 1) == 1);
    assert(byte == expected[i]);
  }
}

static void process_test(int ending) {
  Fake *fake = mmap(NULL, sizeof(*fake), PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_ANON, -1, 0);
  assert(fake != MAP_FAILED);
  *fake = (Fake){.up = true};
  int input[2], output[2];
  assert(pipe(input) == 0 && pipe(output) == 0);
  pid_t child = fork();
  assert(child >= 0);
  if (child == 0) {
    close(input[1]);
    close(output[0]);
    assert(dup2(input[0], STDIN_FILENO) >= 0);
    assert(dup2(output[1], STDOUT_FILENO) >= 0);
    close(input[0]);
    close(output[1]);
    assert(fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) == 0);
    assert(fcntl(STDOUT_FILENO, F_SETFL, O_NONBLOCK) == 0);
    _exit(run_guardian(backend(fake), -1));
  }
  close(input[0]);
  close(output[1]);
  expect_line(output[0], "READY\n");
  assert(fake->sets == 0);
  assert(write(input[1], "A", 1) == 1);
  expect_line(output[0], "ACTIVE\n");
  assert(!fake->up);
  assert(write(input[1], "H", 1) == 1);
  expect_line(output[0], "HELD\n");
  if (ending == 0) {
    assert(write(input[1], "R", 1) == 1);
    expect_line(output[0], "RELEASED\n");
  } else if (ending == 1) {
    close(input[1]);
    input[1] = -1;
  }
  int status;
  uint64_t deadline = milliseconds() + 3500;
  while (waitpid(child, &status, WNOHANG) == 0) {
    assert(milliseconds() < deadline);
    struct timespec delay = {.tv_nsec = 10000000};
    nanosleep(&delay, NULL);
  }
  assert(WIFEXITED(status));
  assert(WEXITSTATUS(status) == (ending == 0 ? 0 : 1));
  assert(fake->up);
  if (input[1] >= 0) close(input[1]);
  close(output[0]);
  munmap(fake, sizeof(*fake));
}

int main(void) {
  state_machine_tests();
  process_test(0);
  process_test(1);
  process_test(2);
  puts("AWDL guardian state and pipe-process tests passed");
  return 0;
}
