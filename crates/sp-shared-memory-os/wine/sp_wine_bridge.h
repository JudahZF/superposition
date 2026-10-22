/* Shared declarations for sp_wine_bridge: the PE DLL that a Windows plug-in worker loads under
 * Wine, and its unixlib. The worker (Rust, crates/sp-shared-memory-os/src/sys/windows.rs)
 * resolves the four exports below with GetProcAddress. */
#ifndef SP_WINE_BRIDGE_H
#define SP_WINE_BRIDGE_H

#include <stdint.h>

enum sp_wine_call {
    SP_WINE_CLOCK,
    SP_WINE_WAKE,
    SP_WINE_WAIT,
    SP_WINE_SCHEDULER,
    SP_WINE_CALL_COUNT
};

struct sp_wine_wake_params { void *address; int32_t result; };
struct sp_wine_wait_params { void *address; uint32_t observed; uint64_t timeout_ns; int32_t result; };
/* policy: 1 = SCHED_FIFO at priority, 0 = SCHED_OTHER. Applies to the calling thread. */
struct sp_wine_scheduler_params { int32_t policy; int32_t priority; int32_t result; };

#endif
