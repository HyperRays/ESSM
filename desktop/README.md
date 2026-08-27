# Desktop source

This directory contains the iced desktop application. It expects this sibling
workspace layout:

```text
workspace/
├── findex/
├── rust_client/
└── desktop/
```

From the workspace root:

```sh
(cd rust_client/backend && mix compile)
cargo build --release --manifest-path desktop/Cargo.toml
desktop/target/release/essm
```

Run the desktop checks with:

```sh
cargo fmt --check --manifest-path desktop/Cargo.toml
cargo clippy --all-targets --manifest-path desktop/Cargo.toml -- -D warnings
cargo test --manifest-path desktop/Cargo.toml
```
