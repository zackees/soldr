# heavy-leaf-0006

Cryptography & hashing subgraph (pure-Rust): `sha2`, `sha3`,
`blake2`, `blake3`, `md-5`, `hmac`, `hkdf`, `pbkdf2`, `argon2`,
`scrypt`, `aes` + `aes-gcm`, `chacha20poly1305`, `ed25519-dalek`,
`curve25519-dalek`, `x25519-dalek`, `signature`, `digest`. No
`cc-rs` build scripts (ring / rusqlite / libpq avoided on
purpose). Heavy generic + trait machinery — distinct from the
prior five leaves' web / http-client / cli / math / parser
subgraphs. See parent `../README.md` and soldr#648.
