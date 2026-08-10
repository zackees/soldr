//! A tokio app with one deliberately blocked task (#788).
//!
//! Usage: `testbin-tokio-blocked <port> [seconds]`
//!
//! The port is an argument rather than console-subscriber's default 6669
//! because the tests run in parallel, each starting its own fixture. Sharing
//! one port made them collide and fail intermittently.
//!
//! Serves `console-api` on that port and prints `READY <endpoint>` once the
//! server is up, so a test can wait for that line rather than sleeping and
//! hoping.
//!
//! # Why a blocked task
//!
//! An off-CPU profile answers "what is waiting". A task parked on a long sleep
//! is the cleanest possible instance of that: it accrues idle time and almost
//! no busy time, so it should dominate an idle-weighted profile by a margin no
//! scheduling noise can close.
//!
//! # If this was built wrong you will know
//!
//! Without `--cfg tokio_unstable`, `console_subscriber::init()` panics with a
//! message naming the missing flag. That is deliberate on their part and
//! useful here: a fixture built without instrumentation cannot come up, serve
//! an empty task list, and be mistaken for an idle program.

use std::time::Duration;

/// Long enough that the task is still parked for the whole sampling window.
const BLOCK_SECONDS: u64 = 3600;

/// The task the profile should be dominated by. Never inlined so its spawn
/// site is its own line.
#[inline(never)]
async fn blocked_forever() {
    tokio::time::sleep(Duration::from_secs(BLOCK_SECONDS)).await;
}

/// A task that actually does work, so the profile has something to contrast
/// the blocked one against.
#[inline(never)]
async fn busy_enough() {
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(6669);
    let lifetime = std::env::args()
        .nth(2)
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(30);

    console_subscriber::ConsoleLayer::builder()
        .server_addr(([127, 0, 0, 1], port))
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async move {
        tokio::spawn(blocked_forever());
        tokio::spawn(busy_enough());

        // The subscriber's gRPC server binds during `init()`, but the runtime
        // needs a moment before it is accepting. Announcing after a short
        // settle is what lets the test wait on a line instead of a sleep.
        tokio::time::sleep(Duration::from_millis(300)).await;
        println!("READY http://127.0.0.1:{port}");
        use std::io::Write as _;
        std::io::stdout().flush().expect("flush");

        tokio::time::sleep(Duration::from_secs(lifetime)).await;
    });
}
