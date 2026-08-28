# `soldr cc` CMake acceptance fixture

This mixed C/C++ project is the standalone-compiler acceptance case for
soldr#2335 and the CMake/C++ consumer shape discussed in soldr#1591.

```bash
soldr cargo build -p soldr-cli --bin soldr
export CC="$PWD/target/debug/soldr cc --target x86_64-linux-gnu.2.17"
export CXX="$PWD/target/debug/soldr c++ --target x86_64-linux-gnu.2.17"
cmake -S ci/fixtures/soldr-cc-cmake -B /tmp/soldr-cc-cmake-build -G Ninja
cmake --build /tmp/soldr-cc-cmake-build --verbose
/tmp/soldr-cc-cmake-build/soldr-cc-cmake
```

The expected output is `hello from soldr cc`.

The matching network-gated acceptance test is opt-in because a cold run
downloads the catalogue toolchain:

```bash
SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_cc::
```
