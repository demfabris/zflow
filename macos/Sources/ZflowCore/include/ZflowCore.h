#ifndef ZFLOW_CORE_H
#define ZFLOW_CORE_H
typedef struct ZflowApp ZflowApp;
ZflowApp *zflow_app_create(const char *path, char **error);
char *zflow_app_request(ZflowApp *app, const char *request);
void zflow_string_free(char *value);
void zflow_app_destroy(ZflowApp *app);
#endif
