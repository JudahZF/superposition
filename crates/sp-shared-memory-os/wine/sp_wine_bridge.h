/* Shared declarations for sp_wine_bridge: the PE DLL that a Windows plug-in worker loads under
 * Wine, and its unixlib. The worker resolves the exports with GetProcAddress: the clock, futex,
 * and scheduler in crates/sp-shared-memory-os/src/sys/windows.rs, and the X11 window label in
 * crates/sp-vst3/src/win32_editor_window.rs. */
#ifndef SP_WINE_BRIDGE_H
#define SP_WINE_BRIDGE_H

#include <stdint.h>

enum sp_wine_call {
    SP_WINE_CLOCK,
    SP_WINE_WAKE,
    SP_WINE_WAIT,
    SP_WINE_SCHEDULER,
    SP_WINE_X11_LABEL,
    SP_WINE_CALL_COUNT
};

struct sp_wine_wake_params { void *address; int32_t result; };
struct sp_wine_wait_params { void *address; uint32_t observed; uint64_t timeout_ns; int32_t result; };
/* policy: 1 = SCHED_FIFO at priority, 0 = SCHED_OTHER. Applies to the calling thread. */
struct sp_wine_scheduler_params { int32_t policy; int32_t priority; int32_t result; };
/* Sets the CARDINAL _SUPERPOSITION_RACK and _SUPERPOSITION_SLOT properties on an X11 window,
 * as Linux editor windows carry them, through the libX11 that Wine's X11 driver loaded. */
struct sp_wine_x11_label_params { uint64_t window; uint32_t rack; uint32_t slot; int32_t result; };

#endif
