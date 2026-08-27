| File | Responsibility |
| --- | --- |
| `findex_nif.c` | NIF registration, load/unload, and the diagnostic `hello/0` entry point. |
| `findex_common.c` | Shared field schema, Erlang-term helpers, errno conversion, and checked size arithmetic. |
| `findex_directory.c` | `attrlist` configuration, packed-record validation/decoding, standalone cursors, and native fused-scan batches. |
| `findex_store.c` | Concurrent append-only tree layout, fused scan publication, completion journal, readers, and asynchronous store reclamation. |
| `findex_nif.h` | The narrow internal interface shared by those translation units. It is not a public C API. |

## Static checks

```sh
make -C native analyze
```

## Runtime checks

```sh
make -C native sanitize
make -C native sanitize-thread
```
