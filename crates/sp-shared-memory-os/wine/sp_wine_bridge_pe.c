/* PE side of sp_wine_bridge. Each export forwards to the unixlib through __wine_unix_call. */
#include <stdarg.h>
#include "windef.h"
#include "winbase.h"
#include "winternl.h"
#include "wine/unixlib.h"
#include "sp_wine_bridge.h"

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, void *reserved)
{
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    DisableThreadLibraryCalls(instance);
    return !__wine_init_unix_call();
}

__declspec(dllexport) UINT64 CDECL sp_wine_clock_ns(void)
{
    UINT64 now = 0;
    WINE_UNIX_CALL(SP_WINE_CLOCK, &now);
    return now;
}

__declspec(dllexport) int CDECL sp_wine_futex_wake(void *address)
{
    struct sp_wine_wake_params params = { address, 0 };
    WINE_UNIX_CALL(SP_WINE_WAKE, &params);
    return params.result;
}

__declspec(dllexport) int CDECL sp_wine_futex_wait(void *address, UINT32 observed, UINT64 timeout_ns)
{
    struct sp_wine_wait_params params = { address, observed, timeout_ns, 0 };
    WINE_UNIX_CALL(SP_WINE_WAIT, &params);
    return params.result;
}

__declspec(dllexport) int CDECL sp_wine_set_scheduler(int policy, int priority)
{
    struct sp_wine_scheduler_params params = { policy, priority, 0 };
    WINE_UNIX_CALL(SP_WINE_SCHEDULER, &params);
    return params.result;
}

__declspec(dllexport) int CDECL sp_wine_x11_label(UINT64 window, UINT32 rack, UINT32 slot)
{
    struct sp_wine_x11_label_params params = { window, rack, slot, 0 };
    WINE_UNIX_CALL(SP_WINE_X11_LABEL, &params);
    return params.result;
}
