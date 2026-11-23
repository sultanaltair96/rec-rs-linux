/* Exercise the same dlopen and symbols used by the Wayland hide barrier. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>

int main(void) {
    const char *symbols[] = {"wl_display_create_queue", "wl_proxy_create_wrapper",
        "wl_proxy_wrapper_destroy", "wl_proxy_set_queue", "wl_proxy_marshal_constructor",
        "wl_proxy_add_listener", "wl_callback_interface", "wl_display_dispatch_queue_pending",
        "wl_display_flush", "wl_proxy_destroy", "wl_event_queue_destroy", NULL};
    void *lib = dlopen("libwayland-client.so.0", RTLD_NOW);
    if (!lib) { fprintf(stderr, "%s\n", dlerror()); return 1; }
    for (int i = 0; symbols[i]; i++) {
        void *p = dlsym(lib, symbols[i]);
        if (!p) { fprintf(stderr, "%s: %s\n", symbols[i], dlerror()); return 1; }
        if (i == 0) {
            Dl_info info;
            if (!dladdr(p, &info)) return 1;
            puts(info.dli_fname);
        }
    }
    dlclose(lib);
    return 0;
}
