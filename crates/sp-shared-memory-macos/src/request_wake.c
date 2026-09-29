#include <errno.h>
#include <os/clock.h>
#include <os/os_sync_wait_on_address.h>
#include <stdint.h>

int superposition_request_wake_supported(void) {
    if (__builtin_available(macOS 14.4, *)) {
        return 1;
    }
    return 0;
}

int superposition_notify_request(void *address) {
    if (__builtin_available(macOS 14.4, *)) {
        if (os_sync_wake_by_address_any(address, sizeof(uint32_t),
                                        OS_SYNC_WAKE_BY_ADDRESS_SHARED) == 0) {
            return 1;
        }
        if (errno == ENOENT) {
            return 0;
        }
        return -errno;
    }
    return -ENOTSUP;
}

int superposition_wait_for_request(void *address, uint32_t observed,
                                   uint64_t timeout_ns) {
    if (__builtin_available(macOS 14.4, *)) {
        if (os_sync_wait_on_address_with_timeout(
                address, observed, sizeof(uint32_t),
                OS_SYNC_WAIT_ON_ADDRESS_SHARED, OS_CLOCK_MACH_ABSOLUTE_TIME,
                timeout_ns) >= 0 ||
            errno == ETIMEDOUT || errno == EINTR) {
            return 0;
        }
        return errno;
    }
    return ENOTSUP;
}
