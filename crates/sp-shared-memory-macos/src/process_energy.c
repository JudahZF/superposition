#include <errno.h>
#include <libproc.h>
#include <stdint.h>
#include <sys/resource.h>

// `rusage_info_t` is a `void *` typedef, so the Darwin header declares this API as taking a
// `void **`. Keep the typed call in C to preserve the SDK's intended ABI and extract only the
// initialized V6 energy field for Rust.
int superposition_process_energy_nj(int process_id, uint64_t *energy_nj) {
    if (energy_nj == NULL) {
        return EINVAL;
    }
    struct rusage_info_v6 usage = {0};
    if (proc_pid_rusage(process_id, RUSAGE_INFO_V6,
                        (rusage_info_t *)&usage) != 0) {
        return errno;
    }
    *energy_nj = usage.ri_energy_nj;
    return 0;
}
