// Genuinely requires the C++ runtime (std::string allocates), so the
// final link fails loudly if the stdlib the objects want is absent.
#include <string>

extern "C" int ccrs_pin_len(const char *s) {
    return static_cast<int>(std::string(s).size());
}
