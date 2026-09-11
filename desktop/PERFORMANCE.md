# UI performance

ESSM 0.1.1 removes repeated sibling-list allocation from visualization updates.
The model already sorts children by subtree size after each backend batch.
Views now borrow the positive-size prefix, found by binary search, and the
node graph prepares only its 12 visible rows. Newly populated directories are
included after the next batch; zero-size directories remain hidden.

The manual benchmark builds a synthetic directory with 100,000 children and
selects the 100 largest children 10,000 times. On the release build used for
this update, three runs measured:

| Implementation | Time for 10,000 selections |
| --- | --- |
| Before | 896.798, 913.581, 952.704 ms |
| After | 0.131, 0.114, 0.108 ms |

This isolates child selection and allocation. It does not measure complete
rendering or filesystem traversal, so these timings are not a scan speedup.
Native C code, traversal policy, and size accounting are unchanged.

To repeat the benchmark from the repository root:

```sh
cargo test --locked --release --manifest-path desktop/Cargo.toml \
  wide_directory_view_benchmark -- --ignored --nocapture
```

The automated model tests cover child ordering, zero-size and missing
directories, and an empty child gaining bytes in a later batch. The full
`make verify` run also covers the real backend, wire protocol, and native
diagnostics.
