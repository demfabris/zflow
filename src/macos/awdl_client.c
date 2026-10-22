#include <Security/Security.h>
#include <dispatch/dispatch.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
#include <xpc/xpc.h>

static void set_message(char *error, size_t size, const char *message) {
  if (error && size) snprintf(error, size, "%s", message);
}

API_AVAILABLE(macos(14.4))
static int request_lease(int *input, int *output, char *error, size_t error_size) {
  xpc_connection_t connection = xpc_connection_create_mach_service(
      "io.zflow.awdl", NULL, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED);
  if (xpc_connection_set_peer_team_identity_requirement(
          connection, "io.zflow.awdl-daemon") != 0) {
    set_message(error, error_size, "could not require the AWDL helper's signature");
    xpc_release(connection);
    return -1;
  }
  xpc_connection_set_event_handler(connection, ^(xpc_object_t event) { (void)event; });
  xpc_connection_activate(connection);
  xpc_object_t request = xpc_dictionary_create(NULL, NULL, 0);
  xpc_dictionary_set_string(request, "command", "lease");
  xpc_object_t reply = xpc_connection_send_message_with_reply_sync(connection, request);
  xpc_release(request);
  int status = -1;
  if (xpc_get_type(reply) != XPC_TYPE_DICTIONARY) {
    set_message(error, error_size,
                reply == XPC_ERROR_PEER_CODE_SIGNING_REQUIREMENT
                    ? "the AWDL helper is not signed by this app's team"
                    : "the AWDL helper is not available");
  } else if (xpc_dictionary_get_string(reply, "error")) {
    set_message(error, error_size, xpc_dictionary_get_string(reply, "error"));
  } else {
    *input = xpc_dictionary_dup_fd(reply, "input");
    *output = xpc_dictionary_dup_fd(reply, "output");
    // A write after the helper ends must fail with EPIPE, not kill the app.
    if (*input >= 0 && *output >= 0 && fcntl(*input, F_SETNOSIGPIPE, 1) == 0) {
      status = 0;
    } else {
      if (*input >= 0) close(*input);
      if (*output >= 0) close(*output);
      *input = *output = -1;
      set_message(error, error_size, "the AWDL helper returned no lease");
    }
  }
  xpc_release(reply);
  xpc_connection_cancel(connection);
  xpc_release(connection);
  return status;
}

// Asks the privileged helper for an AWDL lease. The helper answers only the
// signed zflow app, and this side accepts only the helper from the same team.
// On success the caller owns both pipe ends of the helper's lease protocol.
int zflow_awdl_lease(int *input, int *output, char *error, size_t error_size) {
  *input = -1;
  *output = -1;
  if (__builtin_available(macOS 14.4, *)) {
    return request_lease(input, output, error, error_size);
  }
  set_message(error, error_size, "the AWDL helper needs macOS 14.4 or later");
  return -1;
}

API_AVAILABLE(macos(14.4))
static int check_helper(void) {
  xpc_connection_t connection = xpc_connection_create_mach_service(
      "io.zflow.awdl", NULL, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED);
  if (xpc_connection_set_peer_team_identity_requirement(
          connection, "io.zflow.awdl-daemon") != 0) {
    xpc_release(connection);
    return 0;
  }
  xpc_connection_set_event_handler(connection, ^(xpc_object_t event) { (void)event; });
  xpc_connection_activate(connection);
  xpc_object_t request = xpc_dictionary_create(NULL, NULL, 0);
  xpc_dictionary_set_string(request, "command", "check");
  __block int ready = 0;
  dispatch_semaphore_t answered = dispatch_semaphore_create(0);
  xpc_connection_send_message_with_reply(connection, request, NULL, ^(xpc_object_t reply) {
    if (xpc_get_type(reply) == XPC_TYPE_DICTIONARY) ready = xpc_dictionary_get_bool(reply, "ready");
    dispatch_semaphore_signal(answered);
  });
  xpc_release(request);
  // Cancelling answers a pending reply with an error, so a stuck helper
  // cannot hold the check open.
  if (dispatch_semaphore_wait(answered, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC)) != 0) {
    xpc_connection_cancel(connection);
    dispatch_semaphore_wait(answered, DISPATCH_TIME_FOREVER);
  } else {
    xpc_connection_cancel(connection);
  }
  dispatch_release(answered);
  xpc_release(connection);
  return ready;
}

// Asks the helper whether it can reach awdl0. A helper that does not answer
// within three seconds is not ready.
int zflow_awdl_check(void) {
  if (__builtin_available(macOS 14.4, *)) {
    return check_helper();
  }
  return 0;
}

// Whether this app is signed by a team. Only such a build can install the
// helper, and that cannot change while it runs.
int zflow_signing_team(void) {
  SecCodeRef code = NULL;
  SecStaticCodeRef static_code = NULL;
  CFDictionaryRef information = NULL;
  int team = 0;
  if (SecCodeCopySelf(kSecCSDefaultFlags, &code) == errSecSuccess &&
      SecCodeCopyStaticCode(code, kSecCSDefaultFlags, &static_code) == errSecSuccess &&
      SecCodeCopySigningInformation(static_code, kSecCSSigningInformation, &information) ==
          errSecSuccess) {
    team = CFDictionaryGetValue(information, kSecCodeInfoTeamIdentifier) != NULL;
  }
  if (information) CFRelease(information);
  if (static_code) CFRelease(static_code);
  if (code) CFRelease(code);
  return team;
}
