# soldr-cli fetch

Fetch / install / verify pipeline for managed runtime tools (crgx, cargo-chef) and rustup bootstrap. As of soldr#1368 zccache is no longer fetched here — it ships as a compiled-in `[[bin]]` built from the `_vender/zccache` library dep.
