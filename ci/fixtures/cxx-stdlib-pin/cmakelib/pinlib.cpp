#include <string>

extern "C" int cmake_pin_len(const char *s) {
    return static_cast<int>(std::string(s).size());
}
