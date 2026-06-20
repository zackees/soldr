#include <stdint.h>
#include <stddef.h>

uint64_t rust_native_checksum(const uint8_t *bytes, size_t len) {
    uint64_t hash = 1469598103934665603ULL;
    for (size_t i = 0; i < len; ++i) {
        hash ^= (uint64_t)bytes[i];
        hash *= 1099511628211ULL;
    }
    return hash;
}
