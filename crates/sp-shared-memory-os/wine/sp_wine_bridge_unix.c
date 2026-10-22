/* Unix side of sp_wine_bridge: the shared futex, CLOCK_MONOTONIC, and thread scheduling that a
 * PE worker cannot reach directly (Wine 11.5+ traps raw syscalls from PE code). */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include "sp_wine_bridge.h"

typedef long NTSTATUS;
typedef NTSTATUS (*unixlib_entry_t)(void *);

static NTSTATUS sp_clock(void *args)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    *(uint64_t *)args = (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec;
    return 0;
}

static NTSTATUS sp_wake(void *args)
{
    struct sp_wine_wake_params *params = args;
    long woken = syscall(SYS_futex, params->address, FUTEX_WAKE, 1, NULL, NULL, 0);
    params->result = woken < 0 ? -errno : (int32_t)woken;
    return 0;
}

static NTSTATUS sp_wait(void *args)
{
    struct sp_wine_wait_params *params = args;
    struct timespec timeout = {
        (time_t)(params->timeout_ns / 1000000000ull),
        (long)(params->timeout_ns % 1000000000ull),
    };
    long status = syscall(SYS_futex, params->address, FUTEX_WAIT, params->observed, &timeout, NULL, 0);
    if (status == 0 || errno == EAGAIN || errno == ETIMEDOUT || errno == EINTR) params->result = 0;
    else params->result = errno;
    return 0;
}

static NTSTATUS sp_scheduler(void *args)
{
    struct sp_wine_scheduler_params *params = args;
    struct sched_param parameter = { .sched_priority = params->policy ? params->priority : 0 };
    params->result = pthread_setschedparam(pthread_self(), params->policy ? SCHED_FIFO : SCHED_OTHER,
                                           &parameter);
    return 0;
}

/* Xlib entry points, declared here so the bridge does not link libX11. */
typedef void *(*x_open_display_t)(const char *);
typedef unsigned long (*x_intern_atom_t)(void *, const char *, int);
typedef int (*x_change_property_t)(void *, unsigned long, unsigned long, unsigned long, int, int,
                                   const unsigned char *, int);
typedef int (*x_close_display_t)(void *);

static NTSTATUS sp_x11_label(void *args)
{
    struct sp_wine_x11_label_params *params = args;
    /* Wine's X11 driver has loaded libX11 once a window exists; never load a second copy. */
    void *x11 = dlopen("libX11.so.6", RTLD_NOW | RTLD_NOLOAD);
    params->result = -1;
    if (!x11) return 0;
    x_open_display_t open_display = (x_open_display_t)dlsym(x11, "XOpenDisplay");
    x_intern_atom_t intern_atom = (x_intern_atom_t)dlsym(x11, "XInternAtom");
    x_change_property_t change_property = (x_change_property_t)dlsym(x11, "XChangeProperty");
    x_close_display_t close_display = (x_close_display_t)dlsym(x11, "XCloseDisplay");
    void *display = open_display && intern_atom && change_property && close_display
        ? open_display(NULL) : NULL;
    if (display) {
        /* Format-32 property data is an array of C longs in Xlib. 6 is XA_CARDINAL, 0 is
         * PropModeReplace. Closing the display flushes the requests. */
        long rack = params->rack, slot = params->slot;
        change_property(display, params->window, intern_atom(display, "_SUPERPOSITION_RACK", 0), 6,
                        32, 0, (const unsigned char *)&rack, 1);
        change_property(display, params->window, intern_atom(display, "_SUPERPOSITION_SLOT", 0), 6,
                        32, 0, (const unsigned char *)&slot, 1);
        close_display(display);
        params->result = 0;
    }
    dlclose(x11);
    return 0;
}

__attribute__((visibility("default")))
const unixlib_entry_t __wine_unix_call_funcs[SP_WINE_CALL_COUNT] = {
    sp_clock,
    sp_wake,
    sp_wait,
    sp_scheduler,
    sp_x11_label,
};
