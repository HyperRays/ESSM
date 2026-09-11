# Native Findex library

The native library is built automatically by `mix compile` in `findex/` or by
`make findex` from the repository root. It requires macOS, Erlang headers, and
the Xcode Command Line Tools.

| File | Responsibility |
| --- | --- |
| `findex_nif.c` | NIF registration and resource load/unload. |
| `findex_common.c` | Shared field schema, Erlang-term helpers, errno conversion, and checked size arithmetic. |
| `findex_directory.c` | `attrlist` configuration, packed-record validation/decoding, standalone cursors, and native fused-scan batches. |
| `findex_store.c` | Concurrent append-only tree layout, fused scan publication, completion journal, readers, and asynchronous store reclamation. |
| `findex_nif.h` | The narrow internal interface shared by those translation units. It is not a public C API. |

## Static checks

From the repository root:

```sh
make check-native
```

## Runtime checks

These rebuild the native library with sanitizers, run the engine tests, and
restore the optimized native build afterwards. Run them separately from other
builds and tests that load the same library.

```sh
make -C findex/native sanitize
make -C findex/native sanitize-thread
```
