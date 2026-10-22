/* Unix side of sp_wine_bridge: the shared futex, CLOCK_MONOTONIC, and thread scheduling that a
 * PE worker cannot reach directly (Wine 11.5+ traps raw syscalls from PE code). */
#define _GNU_SOURCE
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

__attribute__((visibility("default")))
const unixlib_entry_t __wine_unix_call_funcs[SP_WINE_CALL_COUNT] = {
    sp_clock,
    sp_wake,
    sp_wait,
    sp_scheduler,
};
