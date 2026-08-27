#ifndef FINDEX_NIF_H
#define FINDEX_NIF_H

#include <erl_nif.h>

#include <stddef.h>
#include <stdint.h>

enum output_format {
  OUTPUT_ENTRIES = 0,
  OUTPUT_PACKED = 1,
};

enum path_policy {
  PATH_FOLLOW_SYMLINKS = 0,
  PATH_REJECT_ANY_SYMLINK = 1,
};

enum traversal_mount_policy {
  TRAVERSAL_CROSS_MOUNTS = 0,
  TRAVERSAL_STAY_ON_FILESYSTEM = 1,
};

enum entry_field {
  FIELD_STRUCT,
  FIELD_NAME,
  FIELD_TYPE,
  FIELD_OBJECT_TAG,
  FIELD_ERROR,
  FIELD_DEVICE,
  FIELD_FILESYSTEM_ID,
  FIELD_FILE_ID,
  FIELD_PARENT_ID,
  FIELD_CREATED_AT,
  FIELD_MODIFIED_AT,
  FIELD_CHANGED_AT,
  FIELD_ACCESSED_AT,
  FIELD_BACKED_UP_AT,
  FIELD_ADDED_AT,
  FIELD_OWNER_ID,
  FIELD_GROUP_ID,
  FIELD_MODE,
  FIELD_FLAGS,
  FIELD_USER_ACCESS,
  FIELD_FINDER_INFO,
  FIELD_OWNER_UUID,
  FIELD_GROUP_UUID,
  FIELD_ACL,
  FIELD_DATA_PROTECTION_FLAGS,
  FIELD_GENERATION_COUNT,
  FIELD_DOCUMENT_ID,
  FIELD_LINK_COUNT,
  FIELD_TOTAL_SIZE,
  FIELD_ALLOCATED_SIZE,
  FIELD_IO_BLOCK_SIZE,
  FIELD_DEVICE_TYPE,
  FIELD_FORK_COUNT,
  FIELD_DATA_SIZE,
  FIELD_DATA_ALLOCATED_SIZE,
  FIELD_RESOURCE_FORK_SIZE,
  FIELD_RESOURCE_FORK_ALLOCATED_SIZE,
  FIELD_DIRECTORY_ENTRY_COUNT,
  FIELD_MOUNT_STATUS,
  FIELD_PRIVATE_SIZE,
  FIELD_LINK_ID,
  FIELD_REAL_DEVICE,
  FIELD_REAL_FILESYSTEM_ID,
  FIELD_CLONE_ID,
  FIELD_EXTENDED_FLAGS,
  FIELD_RECURSIVE_GENERATION_COUNT,
  FIELD_ATTRIBUTION_TAG,
  FIELD_CLONE_REFERENCE_COUNT,
  FIELD_RETURNED_ATTRIBUTES,
  ENTRY_FIELD_COUNT
};

#define FIELD_BIT(field) (UINT64_C(1) << (field))

typedef struct {
  const unsigned char *data;
  size_t size;
} path_component_t;

typedef struct findex_native_batch {
  size_t count;
  size_t storage_size;
  unsigned char *storage;
  unsigned char *column_allocation;
  unsigned char *columns[ENTRY_FIELD_COUNT];
  unsigned char *validity[ENTRY_FIELD_COUNT];
  struct findex_native_batch *next;
} findex_native_batch_t;

typedef struct {
  uint32_t error_number;
  uint64_t count;
} findex_error_count_t;

typedef struct {
  uint64_t requested_fields;
  findex_native_batch_t *first_batch;
  findex_native_batch_t *last_batch;
  uint32_t *child_entry_indices;
  size_t child_count;
  size_t child_capacity;
  findex_error_count_t *error_counts;
  size_t error_count;
  size_t error_capacity;
  uint64_t entries;
  uint64_t directories;
  uint64_t regular_files;
  uint64_t symlinks;
  uint64_t other;
  uint64_t metadata_errors;
  uint64_t skipped_mounts;
} findex_directory_scan_t;

enum findex_scan_status {
  FINDEX_SCAN_OK = 0,
  FINDEX_SCAN_SYSTEM_ERROR,
  FINDEX_SCAN_INVALID_RECORD,
  FINDEX_SCAN_OUT_OF_MEMORY,
  FINDEX_SCAN_INVALID_ARGUMENT,
};

typedef struct store_cleanup_context store_cleanup_context_t;

extern const char *const findex_entry_field_names[ENTRY_FIELD_COUNT];

ERL_NIF_TERM findex_atom(ErlNifEnv *env, const char *name);
ERL_NIF_TERM findex_ok_tuple(ErlNifEnv *env, ERL_NIF_TERM value);
ERL_NIF_TERM findex_error_tuple(ErlNifEnv *env, ERL_NIF_TERM reason);
ERL_NIF_TERM findex_errno_term(ErlNifEnv *env, int error_number);
int findex_errno_from_term(ErlNifEnv *env, ERL_NIF_TERM term,
                           int *error_number);
ERL_NIF_TERM findex_copy_binary(ErlNifEnv *env, const void *data, size_t size);
int findex_valid_path_component(const unsigned char *data, size_t size);
int findex_entry_field_from_term(ErlNifEnv *env, ERL_NIF_TERM term,
                                 enum entry_field *field);
size_t findex_packed_field_width(enum entry_field field);
int findex_checked_add_size(size_t left, size_t right, size_t *result);
int findex_checked_multiply_size(size_t left, size_t right, size_t *result);

int findex_directory_resource_init(ErlNifEnv *env);
int findex_open_directory_path(const ErlNifBinary *path, int path_policy,
                               int *error_number);
int findex_open_directory_component(int parent_fd,
                                    const path_component_t *component,
                                    int *error_number);
enum findex_scan_status findex_scan_directory_fd(
    ErlNifEnv *env, int fd, ERL_NIF_TERM fields, unsigned int buffer_size,
    enum traversal_mount_policy mount_policy, findex_directory_scan_t *scan,
    int *error_number);
void findex_directory_scan_destroy(findex_directory_scan_t *scan);
ERL_NIF_TERM findex_make_directory_cursor(ErlNifEnv *env, int fd,
                                          ERL_NIF_TERM fields,
                                          int output_format);
ERL_NIF_TERM findex_nif_open_directory(ErlNifEnv *env, int argc,
                                       const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_next_directory_batch(ErlNifEnv *env, int argc,
                                             const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_close_directory(ErlNifEnv *env, int argc,
                                        const ERL_NIF_TERM argv[]);

store_cleanup_context_t *findex_store_cleanup_context_create(void);
void findex_store_cleanup_context_destroy(store_cleanup_context_t *context);
int findex_store_resource_init(ErlNifEnv *env);
ERL_NIF_TERM findex_nif_index_store_new(ErlNifEnv *env, int argc,
                                        const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_open_directory(ErlNifEnv *env, int argc,
                                                   const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_close_traversal(ErlNifEnv *env, int argc,
                                                    const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_publish(ErlNifEnv *env, int argc,
                                            const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_scan_and_publish(
    ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_fail(ErlNifEnv *env, int argc,
                                         const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_completed_since(ErlNifEnv *env, int argc,
                                                    const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_fetch(ErlNifEnv *env, int argc,
                                          const ERL_NIF_TERM argv[]);
ERL_NIF_TERM findex_nif_index_store_stats(ErlNifEnv *env, int argc,
                                          const ERL_NIF_TERM argv[]);

#endif
