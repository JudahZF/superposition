#include <stdint.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>

int superposition_shm_open_create(const char *name, int flags, unsigned int mode) {
    return shm_open(name, flags, (mode_t)mode);
}

int superposition_shm_length(int file_descriptor, int64_t *length) {
    struct stat status;
    if (fstat(file_descriptor, &status) != 0) {
        return -1;
    }
    *length = (int64_t)status.st_size;
    return 0;
}
