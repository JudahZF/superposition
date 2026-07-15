#include <sys/mman.h>
#include <sys/types.h>

int superposition_shm_open_create(const char *name, int flags, unsigned int mode) {
    return shm_open(name, flags, (mode_t)mode);
}
