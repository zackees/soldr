# Local-only act runner image. `catthehacker/ubuntu:act-24.04` is a minimal
# stand-in for GitHub's ubuntu-24.04 runner and omits the preinstalled Rust
# toolchain that image ships, so `_build-and-test.yml`'s
# "Install rustup 1.95.0" step dies with `rustup: command not found`.
# Adding rustup here reproduces the runner's starting state without
# changing the workflow.
FROM catthehacker/ubuntu:act-24.04
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
RUN curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh \
 && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain none \
 && rm /tmp/rustup-init.sh \
 && ln -s /usr/local/cargo/bin/rustup /usr/local/bin/rustup \
 && ln -s /usr/local/cargo/bin/cargo /usr/local/bin/cargo \
 && ln -s /usr/local/cargo/bin/rustc /usr/local/bin/rustc \
 && chmod -R a+w /usr/local/rustup /usr/local/cargo
