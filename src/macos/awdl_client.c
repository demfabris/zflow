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
