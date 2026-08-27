#include "findex_nif.h"

#include <stdio.h>

static ERL_NIF_TERM hello(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
  (void)argv;

  if (argc != 0) {
    return enif_make_badarg(env);
  }

  puts("Hello, world!");
  fflush(stdout);
  return findex_atom(env, "ok");
}

static int load(ErlNifEnv *env, void **private_data, ERL_NIF_TERM load_info) {
  (void)load_info;

  store_cleanup_context_t *cleanup_context =
      findex_store_cleanup_context_create();
  if (cleanup_context == NULL) {
    return -1;
  }

  if (!findex_directory_resource_init(env) ||
      !findex_store_resource_init(env)) {
    findex_store_cleanup_context_destroy(cleanup_context);
    return -1;
  }

  *private_data = cleanup_context;
  return 0;
}

static void unload(ErlNifEnv *env, void *private_data) {
  (void)env;
  findex_store_cleanup_context_destroy(private_data);
}

static ErlNifFunc nif_functions[] = {
    {"hello", 0, hello, 0},
    {"open_directory", 4, findex_nif_open_directory,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"next_directory_batch", 2, findex_nif_next_directory_batch,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"close_directory", 1, findex_nif_close_directory,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"index_store_new", 2, findex_nif_index_store_new,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"index_store_open_directory", 4, findex_nif_index_store_open_directory,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"index_store_close_traversal", 1, findex_nif_index_store_close_traversal,
     ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"index_store_publish", 4, findex_nif_index_store_publish,
     ERL_NIF_DIRTY_JOB_CPU_BOUND},
    {"index_store_scan_and_publish", 6,
     findex_nif_index_store_scan_and_publish, ERL_NIF_DIRTY_JOB_IO_BOUND},
    {"index_store_fail", 3, findex_nif_index_store_fail,
     ERL_NIF_DIRTY_JOB_CPU_BOUND},
    {"index_store_completed_since", 3, findex_nif_index_store_completed_since,
     ERL_NIF_DIRTY_JOB_CPU_BOUND},
    {"index_store_fetch", 2, findex_nif_index_store_fetch,
     ERL_NIF_DIRTY_JOB_CPU_BOUND},
    {"index_store_stats", 1, findex_nif_index_store_stats, 0},
};

ERL_NIF_INIT(Elixir.Findex.Nif, nif_functions, load, NULL, NULL, unload)
