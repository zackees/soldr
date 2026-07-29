# `ban_raw_process_creation`

Requires every production child process launched from the `soldr-daemon`
crate to execute through `running-process`. Daemon relocation now lives in
that crate, so the policy follows crate ownership rather than a filename
allowlist in `soldr-cli`.

Command construction and configuration remain allowed. Direct standard or
Tokio command execution, Windows creation flags, and raw platform spawn APIs
are denied. Dedicated test-fixture modules are outside the production scope;
the lint's own UI fixtures remain in scope and prove every prohibited path.
