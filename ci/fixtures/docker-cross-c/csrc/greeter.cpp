// soldr#2319 fixture: a trivial C++ function linked via the cc crate, so the
// container's g++ + libstdc++ path is exercised too.
#include <cstddef>

extern "C" int soldr_fixture_len(const char *s) {
    std::size_t n = 0;
    while (s && s[n]) {
        ++n;
    }
    return static_cast<int>(n);
}
