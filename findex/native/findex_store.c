#include "findex_nif.h"

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <string.h>
#include <unistd.h>

#define DIRECTORY_RECORDS_PER_SEGMENT 4096U
#define ROOT_DIRECTORY_ID 0U
#define NO_DIRECTORY_ID UINT32_MAX

static ErlNifResourceType *index_store_type = NULL;

typedef struct directory_block directory_block_t;
typedef struct store_cleanup_job store_cleanup_job_t;

enum directory_state {
  DIRECTORY_PENDING = 0,
  DIRECTORY_PUBLISHED = 1,
  DIRECTORY_FAILED = 2,
};

typedef struct {
  uint32_t parent_id;
  uint32_t parent_entry_index;
  uint32_t error_number;
  enum directory_state state;
  directory_block_t *block;
} directory_record_t;

typedef struct {
  directory_record_t records[DIRECTORY_RECORDS_PER_SEGMENT];
} directory_segment_t;

struct directory_block {
  uint64_t count;
  uint64_t storage_size;
  uint64_t allocation_size;
  uint32_t first_child_id;
  uint32_t child_count;
  unsigned char data[];
};

typedef struct {
  ErlNifMutex *lock;
  uint64_t fields;
  size_t field_count;
  directory_segment_t **segments;
  size_t segment_count;
  size_t segment_capacity;
  uint32_t directory_count;
  uint32_t published_directory_count;
  uint32_t failed_directory_count;
  uint32_t *completion_journal;
  size_t completion_count;
  size_t completion_capacity;
  uint64_t entry_count;
  uint64_t block_bytes;
  uint64_t payload_bytes;
  unsigned char *root_name;
  size_t root_name_size;
  int root_fd;
  int root_open_error;
  store_cleanup_context_t *cleanup_context;
  store_cleanup_job_t *cleanup_job;
} index_store_t;

struct store_cleanup_job {
  directory_segment_t **segments;
  size_t segment_count;
  uint32_t directory_count;
  uint32_t *completion_journal;
  unsigned char *root_name;
  store_cleanup_job_t *next;
};

struct store_cleanup_context {
  ErlNifMutex *lock;
  ErlNifCond *ready;
  ErlNifTid thread;
  int thread_started;
  int stopping;
  store_cleanup_job_t *head;
  store_cleanup_job_t *tail;
};

static directory_record_t *directory_record_locked(index_store_t *store,
                                                   uint32_t directory_id) {
  if (directory_id >= store->directory_count) {
    return NULL;
  }

  size_t segment_index = directory_id / DIRECTORY_RECORDS_PER_SEGMENT;
  size_t record_index = directory_id % DIRECTORY_RECORDS_PER_SEGMENT;
  return &store->segments[segment_index]->records[record_index];
}

static void destroy_store_cleanup_job(store_cleanup_job_t *job) {
  for (uint32_t directory_id = 0; directory_id < job->directory_count;
       directory_id++) {
    size_t segment_index = directory_id / DIRECTORY_RECORDS_PER_SEGMENT;
    size_t record_index = directory_id % DIRECTORY_RECORDS_PER_SEGMENT;
    directory_record_t *record =
        &job->segments[segment_index]->records[record_index];
    enif_free(record->block);
  }

  for (size_t index = 0; index < job->segment_count; index++) {
    enif_free(job->segments[index]);
  }
  enif_free(job->segments);
  enif_free(job->completion_journal);
  enif_free(job->root_name);
  enif_free(job);
}

static void *store_cleanup_thread(void *argument) {
  store_cleanup_context_t *context = argument;

  for (;;) {
    enif_mutex_lock(context->lock);
    while (context->head == NULL && !context->stopping) {
      enif_cond_wait(context->ready, context->lock);
    }

    store_cleanup_job_t *job = context->head;
    if (job != NULL) {
      context->head = job->next;
      if (context->head == NULL) {
        context->tail = NULL;
      }
    } else if (context->stopping) {
      enif_mutex_unlock(context->lock);
      return NULL;
    }
    enif_mutex_unlock(context->lock);

    destroy_store_cleanup_job(job);
  }
}

store_cleanup_context_t *findex_store_cleanup_context_create(void) {
  store_cleanup_context_t *context = enif_alloc(sizeof(*context));
  if (context == NULL) {
    return NULL;
  }
  memset(context, 0, sizeof(*context));

  context->lock = enif_mutex_create("findex_store_cleanup_lock");
  context->ready = enif_cond_create("findex_store_cleanup_ready");
  if (context->lock == NULL || context->ready == NULL) {
    if (context->ready != NULL) {
      enif_cond_destroy(context->ready);
    }
    if (context->lock != NULL) {
      enif_mutex_destroy(context->lock);
    }
    enif_free(context);
    return NULL;
  }

  char thread_name[] = "findex_store_cleanup";
  if (enif_thread_create(thread_name, &context->thread, store_cleanup_thread,
                         context, NULL) != 0) {
    enif_cond_destroy(context->ready);
    enif_mutex_destroy(context->lock);
    enif_free(context);
    return NULL;
  }
  context->thread_started = 1;
  return context;
}

static void enqueue_store_cleanup(store_cleanup_context_t *context,
                                  store_cleanup_job_t *job) {
  job->next = NULL;
  enif_mutex_lock(context->lock);
  if (context->tail == NULL) {
    context->head = job;
  } else {
    context->tail->next = job;
  }
  context->tail = job;
  enif_cond_signal(context->ready);
  enif_mutex_unlock(context->lock);
}

void findex_store_cleanup_context_destroy(store_cleanup_context_t *context) {
  if (context == NULL) {
    return;
  }

  if (context->thread_started) {
    enif_mutex_lock(context->lock);
    context->stopping = 1;
    enif_cond_signal(context->ready);
    enif_mutex_unlock(context->lock);
    (void)enif_thread_join(context->thread, NULL);
  }

  enif_cond_destroy(context->ready);
  enif_mutex_destroy(context->lock);
  enif_free(context);
}

static int ensure_directory_capacity_locked(index_store_t *store,
                                            uint64_t required_count) {
  if (required_count > UINT32_MAX) {
    return 0;
  }

  size_t required_segments =
      ((size_t)required_count + DIRECTORY_RECORDS_PER_SEGMENT - 1U) /
      DIRECTORY_RECORDS_PER_SEGMENT;

  if (required_segments > store->segment_capacity) {
    size_t capacity =
        store->segment_capacity == 0 ? 4U : store->segment_capacity;
    while (capacity < required_segments) {
      if (capacity > SIZE_MAX / 2U) {
        return 0;
      }
      capacity *= 2U;
    }

    size_t bytes;
    if (!findex_checked_multiply_size(capacity, sizeof(*store->segments),
                                      &bytes)) {
      return 0;
    }

    directory_segment_t **segments = enif_realloc(store->segments, bytes);
    if (segments == NULL) {
      return 0;
    }

    store->segments = segments;
    store->segment_capacity = capacity;
  }

  while (store->segment_count < required_segments) {
    directory_segment_t *segment = enif_alloc(sizeof(*segment));
    if (segment == NULL) {
      return 0;
    }

    memset(segment, 0, sizeof(*segment));
    store->segments[store->segment_count++] = segment;
  }

  return 1;
}

static int ensure_completion_capacity_locked(index_store_t *store,
                                             size_t required_count) {
  if (required_count <= store->completion_capacity) {
    return 1;
  }

  size_t capacity =
      store->completion_capacity == 0 ? 1024U : store->completion_capacity;
  while (capacity < required_count) {
    if (capacity > SIZE_MAX / 2U) {
      return 0;
    }
    capacity *= 2U;
  }

  size_t bytes;
  if (!findex_checked_multiply_size(
          capacity, sizeof(*store->completion_journal), &bytes)) {
    return 0;
  }

  uint32_t *journal = enif_realloc(store->completion_journal, bytes);
  if (journal == NULL) {
    return 0;
  }

  store->completion_journal = journal;
  store->completion_capacity = capacity;
  return 1;
}

static int configure_store_fields(ErlNifEnv *env, index_store_t *store,
                                  ERL_NIF_TERM fields) {
  store->fields = FIELD_BIT(FIELD_NAME) | FIELD_BIT(FIELD_ERROR) |
                  FIELD_BIT(FIELD_RETURNED_ATTRIBUTES);

  ERL_NIF_TERM head;
  ERL_NIF_TERM tail = fields;
  while (enif_get_list_cell(env, tail, &head, &tail)) {
    enum entry_field field;
    if (!findex_entry_field_from_term(env, head, &field) ||
        findex_packed_field_width(field) == 0) {
      return 0;
    }
    store->fields |= FIELD_BIT(field);
  }

  if (!enif_is_empty_list(env, tail)) {
    return 0;
  }

  store->field_count = 0;
  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) != 0) {
      store->field_count++;
    }
  }

  return 1;
}

static int map_value(ErlNifEnv *env, ERL_NIF_TERM map, const char *key,
                     ERL_NIF_TERM *value) {
  return enif_get_map_value(env, map, findex_atom(env, key), value);
}

static int inspect_packed_batch(ErlNifEnv *env, const index_store_t *store,
                                ERL_NIF_TERM batch, size_t *count,
                                ErlNifBinary *storage, ERL_NIF_TERM *columns,
                                ERL_NIF_TERM *validity) {
  ERL_NIF_TERM count_term;
  ERL_NIF_TERM storage_term;
  ERL_NIF_TERM struct_term;
  ErlNifUInt64 count_value;

  if (!map_value(env, batch, "__struct__", &struct_term) ||
      !enif_is_identical(struct_term,
                         findex_atom(env, "Elixir.Findex.Batch")) ||
      !map_value(env, batch, "count", &count_term) ||
      !enif_get_uint64(env, count_term, &count_value) ||
      count_value > UINT32_MAX ||
      !map_value(env, batch, "storage", &storage_term) ||
      !enif_inspect_binary(env, storage_term, storage) ||
      !map_value(env, batch, "columns", columns) ||
      !map_value(env, batch, "validity", validity) ||
      !enif_is_map(env, *columns) || !enif_is_map(env, *validity)) {
    return 0;
  }

  *count = (size_t)count_value;
  size_t validity_size = (*count + 7U) / 8U;

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    ERL_NIF_TERM column_term;
    ERL_NIF_TERM validity_term;
    ErlNifBinary column_binary;
    ErlNifBinary validity_binary;
    size_t expected_column_size;

    if (!findex_checked_multiply_size(
            *count, findex_packed_field_width((enum entry_field)field),
            &expected_column_size) ||
        !enif_get_map_value(env, *columns,
                            findex_atom(env, findex_entry_field_names[field]),
                            &column_term) ||
        !enif_inspect_binary(env, column_term, &column_binary) ||
        column_binary.size != expected_column_size ||
        !enif_get_map_value(env, *validity,
                            findex_atom(env, findex_entry_field_names[field]),
                            &validity_term) ||
        !enif_inspect_binary(env, validity_term, &validity_binary) ||
        validity_binary.size != validity_size) {
      return 0;
    }
  }

  return 1;
}

static size_t block_columns_size(const index_store_t *store,
                                 const directory_block_t *block) {
  size_t size = 0;
  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) != 0) {
      size += findex_packed_field_width((enum entry_field)field) *
              (size_t)block->count;
    }
  }
  return size;
}

static unsigned char *block_column(const index_store_t *store,
                                   const directory_block_t *block,
                                   enum entry_field requested_field) {
  size_t offset = (size_t)block->storage_size;
  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }
    if ((enum entry_field)field == requested_field) {
      return (unsigned char *)block->data + offset;
    }
    offset += findex_packed_field_width((enum entry_field)field) *
              (size_t)block->count;
  }
  return NULL;
}

static unsigned char *block_validity(const index_store_t *store,
                                     const directory_block_t *block,
                                     enum entry_field requested_field) {
  size_t offset =
      (size_t)block->storage_size + block_columns_size(store, block);
  size_t validity_size = ((size_t)block->count + 7U) / 8U;

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }
    if ((enum entry_field)field == requested_field) {
      return (unsigned char *)block->data + offset;
    }
    offset += validity_size;
  }
  return NULL;
}

static unsigned char *
block_child_entry_indices(const index_store_t *store,
                          const directory_block_t *block) {
  size_t validity_size = ((size_t)block->count + 7U) / 8U;
  size_t offset = (size_t)block->storage_size +
                  block_columns_size(store, block) +
                  store->field_count * validity_size;
  return (unsigned char *)block->data + offset;
}

static int block_reference(const index_store_t *store,
                           const directory_block_t *block,
                           enum entry_field field, uint32_t index,
                           const unsigned char **data, size_t *size);
static int validate_directory_block(const index_store_t *store,
                                    const directory_block_t *block);

static int build_directory_block(ErlNifEnv *env, const index_store_t *store,
                                 ERL_NIF_TERM batches,
                                 const uint32_t *child_entry_indices,
                                 size_t child_count,
                                 directory_block_t **result) {
  size_t total_count = 0;
  size_t total_storage_size = 0;
  ERL_NIF_TERM head;
  ERL_NIF_TERM tail = batches;

  while (enif_get_list_cell(env, tail, &head, &tail)) {
    size_t count;
    ErlNifBinary storage;
    ERL_NIF_TERM columns;
    ERL_NIF_TERM validity;
    if (!inspect_packed_batch(env, store, head, &count, &storage, &columns,
                              &validity) ||
        !findex_checked_add_size(total_count, count, &total_count) ||
        !findex_checked_add_size(total_storage_size, storage.size,
                                 &total_storage_size) ||
        total_count > UINT32_MAX || total_storage_size > UINT32_MAX) {
      return 0;
    }
  }

  if (!enif_is_empty_list(env, tail) || child_count > UINT32_MAX) {
    return 0;
  }

  size_t validity_size = (total_count + 7U) / 8U;
  size_t data_size = total_storage_size;

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    size_t column_size;
    if (!findex_checked_multiply_size(
            total_count, findex_packed_field_width((enum entry_field)field),
            &column_size) ||
        !findex_checked_add_size(data_size, column_size, &data_size) ||
        !findex_checked_add_size(data_size, validity_size, &data_size)) {
      return 0;
    }
  }

  size_t child_bytes;
  size_t allocation_size;
  if (!findex_checked_multiply_size(child_count, sizeof(uint32_t),
                                    &child_bytes) ||
      !findex_checked_add_size(data_size, child_bytes, &data_size) ||
      !findex_checked_add_size(sizeof(directory_block_t), data_size,
                               &allocation_size)) {
    return 0;
  }

  directory_block_t *block = enif_alloc(allocation_size);
  if (block == NULL) {
    return -1;
  }

  block->count = total_count;
  block->storage_size = total_storage_size;
  block->allocation_size = allocation_size;
  block->first_child_id = NO_DIRECTORY_ID;
  block->child_count = (uint32_t)child_count;
  memset(block->data, 0, data_size);

  size_t storage_offset = 0;
  tail = batches;
  while (enif_get_list_cell(env, tail, &head, &tail)) {
    size_t count;
    ErlNifBinary storage;
    ERL_NIF_TERM columns;
    ERL_NIF_TERM validity;
    if (!inspect_packed_batch(env, store, head, &count, &storage, &columns,
                              &validity)) {
      enif_free(block);
      return 0;
    }
    memcpy(block->data + storage_offset, storage.data, storage.size);
    storage_offset += storage.size;
  }

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    enum entry_field entry_field = (enum entry_field)field;
    size_t width = findex_packed_field_width(entry_field);
    unsigned char *destination = block_column(store, block, entry_field);
    unsigned char *destination_validity =
        block_validity(store, block, entry_field);
    size_t row_offset = 0;
    size_t batch_storage_offset = 0;

    tail = batches;
    while (enif_get_list_cell(env, tail, &head, &tail)) {
      size_t count;
      ErlNifBinary storage;
      ERL_NIF_TERM columns;
      ERL_NIF_TERM validity;
      ERL_NIF_TERM column_term;
      ERL_NIF_TERM validity_term;
      ErlNifBinary column;
      ErlNifBinary source_validity;

      if (!inspect_packed_batch(env, store, head, &count, &storage, &columns,
                                &validity) ||
          !enif_get_map_value(env, columns,
                              findex_atom(env, findex_entry_field_names[field]),
                              &column_term) ||
          !enif_inspect_binary(env, column_term, &column) ||
          !enif_get_map_value(env, validity,
                              findex_atom(env, findex_entry_field_names[field]),
                              &validity_term) ||
          !enif_inspect_binary(env, validity_term, &source_validity)) {
        enif_free(block);
        return 0;
      }

      if (entry_field == FIELD_NAME || entry_field == FIELD_ACL) {
        for (size_t index = 0; index < count; index++) {
          uint32_t reference[2];
          memcpy(reference, column.data + index * width, sizeof(reference));
          if ((uint64_t)reference[0] + (uint64_t)reference[1] > storage.size ||
              (uint64_t)reference[0] + batch_storage_offset > UINT32_MAX) {
            enif_free(block);
            return 0;
          }
          reference[0] += (uint32_t)batch_storage_offset;
          memcpy(destination + (row_offset + index) * width, reference,
                 sizeof(reference));
        }
      } else {
        memcpy(destination + row_offset * width, column.data, count * width);
      }

      for (size_t index = 0; index < count; index++) {
        if ((source_validity.data[index / 8U] &
             (unsigned char)(1U << (index % 8U))) != 0) {
          size_t destination_index = row_offset + index;
          destination_validity[destination_index / 8U] |=
              (unsigned char)(1U << (destination_index % 8U));
        }
      }

      row_offset += count;
      batch_storage_offset += storage.size;
    }
  }

  unsigned char *stored_child_indices = block_child_entry_indices(store, block);
  for (size_t index = 0; index < child_count; index++) {
    uint32_t entry_index = child_entry_indices[index];
    memcpy(stored_child_indices + index * sizeof(uint32_t), &entry_index,
           sizeof(uint32_t));
  }

  int validation_result = validate_directory_block(store, block);
  if (validation_result <= 0) {
    enif_free(block);
    return validation_result;
  }

  *result = block;
  return 1;
}

static int block_reference(const index_store_t *store,
                           const directory_block_t *block,
                           enum entry_field field, uint32_t index,
                           const unsigned char **data, size_t *size) {
  if (index >= block->count || (store->fields & FIELD_BIT(field)) == 0) {
    return 0;
  }

  unsigned char *validity = block_validity(store, block, field);
  if ((validity[index / 8U] & (unsigned char)(1U << (index % 8U))) == 0) {
    return 0;
  }

  uint32_t reference[2];
  unsigned char *column = block_column(store, block, field);
  memcpy(reference, column + (size_t)index * sizeof(reference),
         sizeof(reference));
  if ((uint64_t)reference[0] + (uint64_t)reference[1] > block->storage_size) {
    return 0;
  }

  *data = block->data + reference[0];
  *size = reference[1];
  return 1;
}

static int validate_directory_block(const index_store_t *store,
                                    const directory_block_t *block) {
  size_t total_count = (size_t)block->count;
  size_t child_count = (size_t)block->child_count;
  size_t validity_size = (total_count + 7U) / 8U;
  unsigned char *stored_child_indices =
      block_child_entry_indices(store, block);
  unsigned char *seen_child_entries = NULL;

  if (child_count > 0) {
    seen_child_entries = enif_alloc(validity_size);
    if (seen_child_entries == NULL) {
      return -1;
    }
    memset(seen_child_entries, 0, validity_size);
  }

  unsigned char *types = NULL;
  unsigned char *type_validity = NULL;
  if ((store->fields & FIELD_BIT(FIELD_TYPE)) != 0) {
    types = block_column(store, block, FIELD_TYPE);
    type_validity = block_validity(store, block, FIELD_TYPE);
  }

  unsigned char *name_validity = block_validity(store, block, FIELD_NAME);
  unsigned char *error_validity = block_validity(store, block, FIELD_ERROR);

  for (size_t index = 0; index < child_count; index++) {
    uint32_t entry_index;
    memcpy(&entry_index, stored_child_indices + index * sizeof(entry_index),
           sizeof(entry_index));

    if ((uint64_t)entry_index >= block->count) {
      enif_free(seen_child_entries);
      return 0;
    }

    unsigned char child_bit = (unsigned char)(1U << (entry_index % 8U));
    if ((seen_child_entries[entry_index / 8U] & child_bit) != 0 ||
        (name_validity[entry_index / 8U] & child_bit) == 0 ||
        (error_validity[entry_index / 8U] & child_bit) != 0 ||
        (types != NULL &&
         ((type_validity[entry_index / 8U] & child_bit) == 0 ||
          types[entry_index] != 2U))) {
      enif_free(seen_child_entries);
      return 0;
    }
    seen_child_entries[entry_index / 8U] |= child_bit;
  }

  for (size_t index = 0; index < total_count; index++) {
    const unsigned char *name;
    size_t name_size;
    unsigned char name_bit = (unsigned char)(1U << (index % 8U));
    if ((name_validity[index / 8U] & name_bit) != 0 &&
        (!block_reference(store, block, FIELD_NAME, (uint32_t)index, &name,
                          &name_size) ||
         !findex_valid_path_component(name, name_size))) {
      enif_free(seen_child_entries);
      return 0;
    }
  }

  enif_free(seen_child_entries);
  return 1;
}

static int build_directory_block_from_scan(
    const index_store_t *store, const findex_directory_scan_t *scan,
    directory_block_t **result) {
  if (scan->requested_fields != store->fields || scan->entries > UINT32_MAX ||
      scan->child_count > UINT32_MAX) {
    return 0;
  }

  size_t total_count = 0;
  size_t total_storage_size = 0;
  for (const findex_native_batch_t *batch = scan->first_batch; batch != NULL;
       batch = batch->next) {
    if (!findex_checked_add_size(total_count, batch->count, &total_count) ||
        !findex_checked_add_size(total_storage_size, batch->storage_size,
                                 &total_storage_size) ||
        total_count > UINT32_MAX || total_storage_size > UINT32_MAX) {
      return 0;
    }
  }
  if ((uint64_t)total_count != scan->entries) {
    return 0;
  }

  size_t validity_size = (total_count + 7U) / 8U;
  size_t data_size = total_storage_size;
  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    size_t column_size;
    if (!findex_checked_multiply_size(
            total_count, findex_packed_field_width((enum entry_field)field),
            &column_size) ||
        !findex_checked_add_size(data_size, column_size, &data_size) ||
        !findex_checked_add_size(data_size, validity_size, &data_size)) {
      return 0;
    }
  }

  size_t child_bytes;
  size_t allocation_size;
  if (!findex_checked_multiply_size(scan->child_count, sizeof(uint32_t),
                                    &child_bytes) ||
      !findex_checked_add_size(data_size, child_bytes, &data_size) ||
      !findex_checked_add_size(sizeof(directory_block_t), data_size,
                               &allocation_size)) {
    return 0;
  }

  directory_block_t *block = enif_alloc(allocation_size);
  if (block == NULL) {
    return -1;
  }
  block->count = total_count;
  block->storage_size = total_storage_size;
  block->allocation_size = allocation_size;
  block->first_child_id = NO_DIRECTORY_ID;
  block->child_count = (uint32_t)scan->child_count;
  memset(block->data, 0, data_size);

  size_t storage_offset = 0;
  for (const findex_native_batch_t *batch = scan->first_batch; batch != NULL;
       batch = batch->next) {
    memcpy(block->data + storage_offset, batch->storage, batch->storage_size);
    storage_offset += batch->storage_size;
  }

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    enum entry_field entry_field = (enum entry_field)field;
    size_t width = findex_packed_field_width(entry_field);
    unsigned char *destination = block_column(store, block, entry_field);
    unsigned char *destination_validity =
        block_validity(store, block, entry_field);
    size_t row_offset = 0;
    size_t batch_storage_offset = 0;

    for (const findex_native_batch_t *batch = scan->first_batch;
         batch != NULL; batch = batch->next) {
      const unsigned char *source = batch->columns[field];
      const unsigned char *source_validity = batch->validity[field];
      if (source == NULL || source_validity == NULL) {
        enif_free(block);
        return 0;
      }

      if (entry_field == FIELD_NAME || entry_field == FIELD_ACL) {
        for (size_t index = 0; index < batch->count; index++) {
          uint32_t reference[2];
          memcpy(reference, source + index * width, sizeof(reference));
          if ((uint64_t)reference[0] + (uint64_t)reference[1] >
                  batch->storage_size ||
              (uint64_t)reference[0] + batch_storage_offset > UINT32_MAX) {
            enif_free(block);
            return 0;
          }
          reference[0] += (uint32_t)batch_storage_offset;
          memcpy(destination + (row_offset + index) * width, reference,
                 sizeof(reference));
        }
      } else {
        memcpy(destination + row_offset * width, source,
               batch->count * width);
      }

      for (size_t index = 0; index < batch->count; index++) {
        if ((source_validity[index / 8U] &
             (unsigned char)(1U << (index % 8U))) != 0) {
          size_t destination_index = row_offset + index;
          destination_validity[destination_index / 8U] |=
              (unsigned char)(1U << (destination_index % 8U));
        }
      }

      row_offset += batch->count;
      batch_storage_offset += batch->storage_size;
    }
  }

  unsigned char *stored_child_indices = block_child_entry_indices(store, block);
  if (scan->child_count > 0) {
    memcpy(stored_child_indices, scan->child_entry_indices, child_bytes);
  }

  int validation_result = validate_directory_block(store, block);
  if (validation_result <= 0) {
    enif_free(block);
    return validation_result;
  }

  *result = block;
  return 1;
}

static ERL_NIF_TERM resource_binary(ErlNifEnv *env, void *resource,
                                    const unsigned char *data, size_t size) {
  if (size == 0) {
    return findex_copy_binary(env, NULL, 0);
  }
  return enif_make_resource_binary(env, resource, data, size);
}

static int make_stored_batch(ErlNifEnv *env, const index_store_t *store,
                             const directory_block_t *block,
                             ERL_NIF_TERM *result) {
  ERL_NIF_TERM column_keys[ENTRY_FIELD_COUNT];
  ERL_NIF_TERM column_values[ENTRY_FIELD_COUNT];
  ERL_NIF_TERM validity_values[ENTRY_FIELD_COUNT];
  size_t field_count = 0;
  size_t validity_size = ((size_t)block->count + 7U) / 8U;
  ERL_NIF_TERM fields = enif_make_list(env, 0);

  for (size_t field = FIELD_NAME; field < ENTRY_FIELD_COUNT; field++) {
    if ((store->fields & FIELD_BIT(field)) == 0) {
      continue;
    }

    size_t column_size = findex_packed_field_width((enum entry_field)field) *
                         (size_t)block->count;
    column_keys[field_count] =
        findex_atom(env, findex_entry_field_names[field]);
    column_values[field_count] = resource_binary(
        env, (void *)store, block_column(store, block, (enum entry_field)field),
        column_size);
    validity_values[field_count] = resource_binary(
        env, (void *)store,
        block_validity(store, block, (enum entry_field)field), validity_size);
    field_count++;
  }

  for (size_t field = ENTRY_FIELD_COUNT; field-- > FIELD_NAME;) {
    if ((store->fields & FIELD_BIT(field)) != 0) {
      fields = enif_make_list_cell(
          env, findex_atom(env, findex_entry_field_names[field]), fields);
    }
  }

  ERL_NIF_TERM column_map;
  ERL_NIF_TERM validity_map;
  if (!enif_make_map_from_arrays(env, column_keys, column_values, field_count,
                                 &column_map) ||
      !enif_make_map_from_arrays(env, column_keys, validity_values, field_count,
                                 &validity_map)) {
    return 0;
  }

  ERL_NIF_TERM keys[] = {
      findex_atom(env, "__struct__"), findex_atom(env, "count"),
      findex_atom(env, "fields"),     findex_atom(env, "storage"),
      findex_atom(env, "columns"),    findex_atom(env, "validity"),
  };
  ERL_NIF_TERM values[] = {
      findex_atom(env, "Elixir.Findex.Batch"),
      enif_make_uint64(env, block->count),
      fields,
      resource_binary(env, (void *)store, block->data,
                      (size_t)block->storage_size),
      column_map,
      validity_map,
  };

  return enif_make_map_from_arrays(env, keys, values,
                                   sizeof(keys) / sizeof(keys[0]), result);
}

static void index_store_destructor(ErlNifEnv *env, void *object) {
  (void)env;
  index_store_t *store = object;

  if (store->root_fd >= 0) {
    close(store->root_fd);
    store->root_fd = -1;
  }

  if (store->lock != NULL) {
    enif_mutex_destroy(store->lock);
    store->lock = NULL;
  }

  store_cleanup_job_t *job = store->cleanup_job;
  if (job != NULL) {
    job->segments = store->segments;
    job->segment_count = store->segment_count;
    job->directory_count = store->directory_count;
    job->completion_journal = store->completion_journal;
    job->root_name = store->root_name;
    store->cleanup_job = NULL;
    if (store->cleanup_context == NULL) {
      destroy_store_cleanup_job(job);
    } else {
      enqueue_store_cleanup(store->cleanup_context, job);
    }
  } else {
    for (uint32_t directory_id = 0; directory_id < store->directory_count;
         directory_id++) {
      size_t segment_index = directory_id / DIRECTORY_RECORDS_PER_SEGMENT;
      size_t record_index = directory_id % DIRECTORY_RECORDS_PER_SEGMENT;
      directory_record_t *record =
          &store->segments[segment_index]->records[record_index];
      enif_free(record->block);
    }
    for (size_t index = 0; index < store->segment_count; index++) {
      enif_free(store->segments[index]);
    }
    enif_free(store->segments);
    enif_free(store->completion_journal);
    enif_free(store->root_name);
  }
}

ERL_NIF_TERM findex_nif_index_store_new(ErlNifEnv *env, int argc,
                                        const ERL_NIF_TERM argv[]) {
  ErlNifBinary root_name;
  if (argc != 2 || !enif_inspect_iolist_as_binary(env, argv[0], &root_name) ||
      !enif_is_list(env, argv[1])) {
    return enif_make_badarg(env);
  }

  index_store_t *store = enif_alloc_resource(index_store_type, sizeof(*store));
  if (store == NULL) {
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  memset(store, 0, sizeof(*store));
  store->root_fd = -1;
  store->cleanup_context = enif_priv_data(env);
  store->cleanup_job = enif_alloc(sizeof(*store->cleanup_job));
  if (store->cleanup_context == NULL || store->cleanup_job == NULL) {
    enif_release_resource(store);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }
  memset(store->cleanup_job, 0, sizeof(*store->cleanup_job));
  store->lock = enif_mutex_create("findex_index_store");
  if (store->lock == NULL || !configure_store_fields(env, store, argv[1]) ||
      !ensure_directory_capacity_locked(store, 1U)) {
    int invalid_fields = store->lock != NULL && store->field_count == 0;
    enif_release_resource(store);
    return findex_error_tuple(
        env, findex_atom(env, invalid_fields ? "invalid_fields" : "enomem"));
  }

  if (root_name.size > 0) {
    store->root_name = enif_alloc(root_name.size);
    if (store->root_name == NULL) {
      enif_release_resource(store);
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }
    memcpy(store->root_name, root_name.data, root_name.size);
  }
  store->root_name_size = root_name.size;
  store->root_fd = findex_open_directory_path(&root_name, PATH_FOLLOW_SYMLINKS,
                                              &store->root_open_error);
  store->directory_count = 1U;
  directory_record_t *root = directory_record_locked(store, ROOT_DIRECTORY_ID);
  root->parent_id = NO_DIRECTORY_ID;
  root->parent_entry_index = NO_DIRECTORY_ID;

  ERL_NIF_TERM resource = enif_make_resource(env, store);
  enif_release_resource(store);
  return findex_ok_tuple(env, resource);
}

static int open_store_directory_fd(ErlNifEnv *env, index_store_t *store,
                                   uint32_t directory_id, int *result_fd,
                                   ERL_NIF_TERM *reason) {
  enif_mutex_lock(store->lock);
  directory_record_t *record = directory_record_locked(store, directory_id);
  if (record == NULL) {
    enif_mutex_unlock(store->lock);
    *reason = findex_atom(env, "not_found");
    return 0;
  }
  if (store->root_fd < 0) {
    int root_open_error = store->root_open_error;
    enif_mutex_unlock(store->lock);
    *reason = findex_errno_term(env, root_open_error);
    return 0;
  }

  size_t component_count = 0;
  uint32_t current_id = directory_id;
  while (current_id != ROOT_DIRECTORY_ID) {
    directory_record_t *current = directory_record_locked(store, current_id);
    if (current == NULL || current->parent_id == NO_DIRECTORY_ID ||
        current->parent_id >= current_id ||
        ++component_count > store->directory_count) {
      enif_mutex_unlock(store->lock);
      *reason = findex_atom(env, "invalid_tree");
      return 0;
    }
    current_id = current->parent_id;
  }

  path_component_t *components = NULL;
  if (component_count > 0) {
    size_t component_bytes;
    if (!findex_checked_multiply_size(component_count, sizeof(*components),
                                      &component_bytes)) {
      enif_mutex_unlock(store->lock);
      *reason = findex_atom(env, "enomem");
      return 0;
    }
    components = enif_alloc(component_bytes);
    if (components == NULL) {
      enif_mutex_unlock(store->lock);
      *reason = findex_atom(env, "enomem");
      return 0;
    }
  }

  current_id = directory_id;
  size_t component_index = 0;
  while (current_id != ROOT_DIRECTORY_ID) {
    directory_record_t *current = directory_record_locked(store, current_id);
    directory_record_t *parent =
        current == NULL ? NULL
                        : directory_record_locked(store, current->parent_id);
    const unsigned char *name;
    size_t name_size;

    if (current == NULL || parent == NULL || parent->block == NULL ||
        component_index >= component_count ||
        !block_reference(store, parent->block, FIELD_NAME,
                         current->parent_entry_index, &name, &name_size)) {
      enif_mutex_unlock(store->lock);
      enif_free(components);
      *reason = findex_atom(env, "invalid_tree");
      return 0;
    }

    components[component_index].data = name;
    components[component_index].size = name_size;
    component_index++;
    current_id = current->parent_id;
  }

  int fd;
  do {
    fd = fcntl(store->root_fd, F_DUPFD_CLOEXEC, 0);
  } while (fd < 0 && errno == EINTR);
  int open_error = fd < 0 ? errno : 0;
  enif_mutex_unlock(store->lock);

  if (fd < 0) {
    enif_free(components);
    *reason = findex_errno_term(env, open_error);
    return 0;
  }

  for (size_t index = component_count; index-- > 0;) {
    int child_fd =
        findex_open_directory_component(fd, &components[index], &open_error);
    close(fd);
    if (child_fd < 0) {
      enif_free(components);
      *reason = findex_errno_term(env, open_error);
      return 0;
    }
    fd = child_fd;
  }

  enif_free(components);
  *result_fd = fd;
  return 1;
}

ERL_NIF_TERM findex_nif_index_store_open_directory(ErlNifEnv *env, int argc,
                                                   const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  unsigned int directory_id;
  int output_format;

  if (argc != 4 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint(env, argv[1], &directory_id) ||
      !enif_is_list(env, argv[2]) ||
      !enif_get_int(env, argv[3], &output_format) ||
      (output_format != OUTPUT_ENTRIES && output_format != OUTPUT_PACKED)) {
    return enif_make_badarg(env);
  }

  int fd;
  ERL_NIF_TERM reason;
  if (!open_store_directory_fd(env, store, directory_id, &fd, &reason)) {
    return findex_error_tuple(env, reason);
  }
  return findex_make_directory_cursor(env, fd, argv[2], output_format);
}

ERL_NIF_TERM findex_nif_index_store_close_traversal(ErlNifEnv *env, int argc,
                                                    const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  if (argc != 1 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store)) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(store->lock);
  if (store->root_fd >= 0) {
    close(store->root_fd);
    store->root_fd = -1;
    store->root_open_error = EBADF;
  }
  enif_mutex_unlock(store->lock);
  return findex_atom(env, "ok");
}

enum publish_block_status {
  PUBLISH_BLOCK_OK = 0,
  PUBLISH_BLOCK_NOT_FOUND,
  PUBLISH_BLOCK_ALREADY_COMPLETED,
  PUBLISH_BLOCK_OUT_OF_MEMORY,
};

static enum publish_block_status publish_directory_block(
    index_store_t *store, uint32_t directory_id, directory_block_t *block,
    uint32_t *first_child_id) {
  enif_mutex_lock(store->lock);
  directory_record_t *record = directory_record_locked(store, directory_id);
  if (record == NULL) {
    enif_mutex_unlock(store->lock);
    return PUBLISH_BLOCK_NOT_FOUND;
  }
  if (record->state != DIRECTORY_PENDING) {
    enif_mutex_unlock(store->lock);
    return PUBLISH_BLOCK_ALREADY_COMPLETED;
  }

  uint32_t child_count = block->child_count;
  uint64_t required_count =
      (uint64_t)store->directory_count + (uint64_t)child_count;
  size_t required_completions;
  if (!findex_checked_add_size(store->completion_count, 1U,
                               &required_completions) ||
      !ensure_directory_capacity_locked(store, required_count) ||
      !ensure_completion_capacity_locked(store, required_completions)) {
    enif_mutex_unlock(store->lock);
    return PUBLISH_BLOCK_OUT_OF_MEMORY;
  }

  *first_child_id = store->directory_count;
  if (child_count > 0) {
    block->first_child_id = *first_child_id;
  }

  unsigned char *child_entry_indices =
      block_child_entry_indices(store, block);
  for (uint32_t index = 0; index < child_count; index++) {
    uint32_t entry_index;
    memcpy(&entry_index,
           child_entry_indices + (size_t)index * sizeof(entry_index),
           sizeof(entry_index));

    uint32_t new_directory_id = *first_child_id + index;
    size_t segment_index = new_directory_id / DIRECTORY_RECORDS_PER_SEGMENT;
    size_t record_index = new_directory_id % DIRECTORY_RECORDS_PER_SEGMENT;
    directory_record_t *child =
        &store->segments[segment_index]->records[record_index];
    child->parent_id = directory_id;
    child->parent_entry_index = entry_index;
    child->error_number = 0U;
    child->state = DIRECTORY_PENDING;
    child->block = NULL;
  }

  record->block = block;
  record->state = DIRECTORY_PUBLISHED;
  store->directory_count += child_count;
  store->published_directory_count++;
  store->entry_count += block->count;
  store->block_bytes += block->allocation_size;
  store->payload_bytes += block->allocation_size - sizeof(*block);
  store->completion_journal[store->completion_count++] = directory_id;
  enif_mutex_unlock(store->lock);
  return PUBLISH_BLOCK_OK;
}

static ERL_NIF_TERM publish_block_error(ErlNifEnv *env,
                                        enum publish_block_status status) {
  switch (status) {
  case PUBLISH_BLOCK_NOT_FOUND:
    return findex_atom(env, "not_found");
  case PUBLISH_BLOCK_ALREADY_COMPLETED:
    return findex_atom(env, "already_completed");
  case PUBLISH_BLOCK_OUT_OF_MEMORY:
    return findex_atom(env, "enomem");
  case PUBLISH_BLOCK_OK:
    break;
  }
  return findex_atom(env, "invalid_block");
}

ERL_NIF_TERM findex_nif_index_store_publish(ErlNifEnv *env, int argc,
                                            const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  unsigned int directory_id;
  unsigned int child_count;

  if (argc != 4 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint(env, argv[1], &directory_id) ||
      !enif_is_list(env, argv[2]) ||
      !enif_get_list_length(env, argv[3], &child_count)) {
    return enif_make_badarg(env);
  }

  uint32_t *child_entry_indices = NULL;
  if (child_count > 0) {
    size_t child_entry_bytes;
    if (!findex_checked_multiply_size(sizeof(*child_entry_indices),
                                      (size_t)child_count,
                                      &child_entry_bytes)) {
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }
    child_entry_indices = enif_alloc(child_entry_bytes);
    if (child_entry_indices == NULL) {
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }
  }

  ERL_NIF_TERM head;
  ERL_NIF_TERM tail = argv[3];
  size_t child_index = 0;
  while (enif_get_list_cell(env, tail, &head, &tail)) {
    unsigned int entry_index;
    if (child_index >= child_count || !enif_get_uint(env, head, &entry_index)) {
      enif_free(child_entry_indices);
      return enif_make_badarg(env);
    }
    child_entry_indices[child_index++] = entry_index;
  }
  if (!enif_is_empty_list(env, tail) || child_index != child_count) {
    enif_free(child_entry_indices);
    return enif_make_badarg(env);
  }

  directory_block_t *block;
  int build_result = build_directory_block(
      env, store, argv[2], child_entry_indices, child_count, &block);
  if (build_result <= 0) {
    enif_free(child_entry_indices);
    return findex_error_tuple(
        env, findex_atom(env, build_result < 0 ? "enomem" : "invalid_block"));
  }

  uint32_t first_child_id;
  enum publish_block_status publish_result = publish_directory_block(
      store, (uint32_t)directory_id, block, &first_child_id);
  if (publish_result != PUBLISH_BLOCK_OK) {
    enif_free(block);
    enif_free(child_entry_indices);
    return findex_error_tuple(env,
                              publish_block_error(env, publish_result));
  }

  ERL_NIF_TERM ids = enif_make_list(env, 0);
  for (size_t index = child_count; index-- > 0;) {
    uint32_t child_id = first_child_id + (uint32_t)index;
    ids = enif_make_list_cell(env, enif_make_uint(env, child_id), ids);
  }

  enif_free(child_entry_indices);
  return findex_ok_tuple(env, ids);
}

static ERL_NIF_TERM scan_phase_error(ErlNifEnv *env, const char *phase,
                                     ERL_NIF_TERM reason) {
  return enif_make_tuple3(env, findex_atom(env, "error"),
                          findex_atom(env, phase), reason);
}

static ERL_NIF_TERM scan_error_number_term(ErlNifEnv *env,
                                           uint32_t error_number) {
  if (error_number <= INT_MAX) {
    return findex_errno_term(env, (int)error_number);
  }
  return enif_make_uint(env, error_number);
}

ERL_NIF_TERM findex_nif_index_store_scan_and_publish(
    ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  unsigned int directory_id;
  ErlNifBinary path;
  unsigned int buffer_size;
  int mount_policy_value;

  if (argc != 6 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint(env, argv[1], &directory_id) ||
      !enif_inspect_iolist_as_binary(env, argv[2], &path) ||
      !enif_is_list(env, argv[3]) ||
      !enif_get_uint(env, argv[4], &buffer_size) ||
      !enif_get_int(env, argv[5], &mount_policy_value) ||
      (mount_policy_value != TRAVERSAL_CROSS_MOUNTS &&
       mount_policy_value != TRAVERSAL_STAY_ON_FILESYSTEM)) {
    return enif_make_badarg(env);
  }

  int open_error;
  int fd = findex_open_directory_path(&path, PATH_REJECT_ANY_SYMLINK,
                                      &open_error);
  if (fd < 0 && (open_error == ENAMETOOLONG || open_error == ELOOP)) {
    ERL_NIF_TERM reason;
    if (!open_store_directory_fd(env, store, (uint32_t)directory_id, &fd,
                                 &reason)) {
      return scan_phase_error(env, "open", reason);
    }
  } else if (fd < 0) {
    return scan_phase_error(env, "open",
                            findex_errno_term(env, open_error));
  }

  findex_directory_scan_t scan;
  enum findex_scan_status scan_result = findex_scan_directory_fd(
      env, fd, argv[3], buffer_size,
      (enum traversal_mount_policy)mount_policy_value, &scan, &open_error);
  close(fd);

  if (scan_result != FINDEX_SCAN_OK) {
    findex_directory_scan_destroy(&scan);
    switch (scan_result) {
    case FINDEX_SCAN_SYSTEM_ERROR:
      return scan_phase_error(env, "read",
                              findex_errno_term(env, open_error));
    case FINDEX_SCAN_INVALID_RECORD:
      return scan_phase_error(env, "read",
                              findex_atom(env, "invalid_native_record"));
    case FINDEX_SCAN_OUT_OF_MEMORY:
      return scan_phase_error(env, "read", findex_atom(env, "enomem"));
    case FINDEX_SCAN_INVALID_ARGUMENT:
      return enif_make_badarg(env);
    case FINDEX_SCAN_OK:
      break;
    }
  }

  directory_block_t *block;
  int build_result = build_directory_block_from_scan(store, &scan, &block);
  if (build_result <= 0) {
    findex_directory_scan_destroy(&scan);
    return scan_phase_error(
        env, "store",
        findex_atom(env, build_result < 0 ? "enomem" : "invalid_block"));
  }

  uint32_t first_child_id;
  enum publish_block_status publish_result = publish_directory_block(
      store, (uint32_t)directory_id, block, &first_child_id);
  if (publish_result != PUBLISH_BLOCK_OK) {
    enif_free(block);
    findex_directory_scan_destroy(&scan);
    return scan_phase_error(env, "store",
                            publish_block_error(env, publish_result));
  }

  ERL_NIF_TERM children = enif_make_list(env, 0);
  unsigned char *stored_child_indices = block_child_entry_indices(store, block);
  for (size_t index = scan.child_count; index-- > 0;) {
    uint32_t entry_index;
    memcpy(&entry_index, stored_child_indices + index * sizeof(entry_index),
           sizeof(entry_index));

    const unsigned char *name;
    size_t name_size;
    if (!block_reference(store, block, FIELD_NAME, entry_index, &name,
                         &name_size)) {
      findex_directory_scan_destroy(&scan);
      return scan_phase_error(env, "store", findex_atom(env, "invalid_tree"));
    }

    ERL_NIF_TERM child = enif_make_tuple3(
        env, enif_make_uint(env, first_child_id + (uint32_t)index),
        enif_make_uint(env, entry_index),
        resource_binary(env, store, name, name_size));
    children = enif_make_list_cell(env, child, children);
  }

  ERL_NIF_TERM error_counts = enif_make_list(env, 0);
  for (size_t index = scan.error_count; index-- > 0;) {
    ERL_NIF_TERM item = enif_make_tuple2(
        env, scan_error_number_term(env, scan.error_counts[index].error_number),
        enif_make_uint64(env, scan.error_counts[index].count));
    error_counts = enif_make_list_cell(env, item, error_counts);
  }

  ERL_NIF_TERM counter_values[] = {
      enif_make_uint64(env, scan.entries),
      enif_make_uint64(env, scan.directories),
      enif_make_uint64(env, scan.regular_files),
      enif_make_uint64(env, scan.symlinks),
      enif_make_uint64(env, scan.other),
      enif_make_uint64(env, scan.metadata_errors),
      error_counts,
      enif_make_uint64(env, scan.skipped_mounts),
  };
  ERL_NIF_TERM counters = enif_make_tuple_from_array(
      env, counter_values, sizeof(counter_values) / sizeof(counter_values[0]));

  findex_directory_scan_destroy(&scan);
  return enif_make_tuple3(env, findex_atom(env, "ok"), children, counters);
}

ERL_NIF_TERM findex_nif_index_store_fail(ErlNifEnv *env, int argc,
                                         const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  unsigned int directory_id;
  int error_number;
  if (argc != 3 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint(env, argv[1], &directory_id) ||
      !findex_errno_from_term(env, argv[2], &error_number)) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(store->lock);
  directory_record_t *record = directory_record_locked(store, directory_id);
  if (record == NULL) {
    enif_mutex_unlock(store->lock);
    return findex_error_tuple(env, findex_atom(env, "not_found"));
  }
  if (record->state != DIRECTORY_PENDING) {
    enif_mutex_unlock(store->lock);
    return findex_error_tuple(env, findex_atom(env, "already_completed"));
  }

  size_t required_completions;
  if (!findex_checked_add_size(store->completion_count, 1U,
                               &required_completions) ||
      !ensure_completion_capacity_locked(store, required_completions)) {
    enif_mutex_unlock(store->lock);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  record->error_number = (uint32_t)error_number;
  record->state = DIRECTORY_FAILED;
  store->failed_directory_count++;
  store->completion_journal[store->completion_count++] = directory_id;
  enif_mutex_unlock(store->lock);
  return findex_atom(env, "ok");
}

ERL_NIF_TERM findex_nif_index_store_completed_since(ErlNifEnv *env, int argc,
                                                    const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  ErlNifUInt64 cursor;
  unsigned int limit;
  if (argc != 3 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint64(env, argv[1], &cursor) ||
      !enif_get_uint(env, argv[2], &limit) || limit == 0U) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(store->lock);
  size_t completion_count = store->completion_count;
  if (cursor > completion_count) {
    enif_mutex_unlock(store->lock);
    return findex_error_tuple(env, findex_atom(env, "invalid_cursor"));
  }

  size_t available = completion_count - (size_t)cursor;
  size_t result_count = available < (size_t)limit ? available : (size_t)limit;
  enif_mutex_unlock(store->lock);

  uint32_t *completed_ids = NULL;
  if (result_count > 0) {
    size_t result_bytes;
    if (!findex_checked_multiply_size(result_count, sizeof(*completed_ids),
                                      &result_bytes)) {
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }

    completed_ids = enif_alloc(result_bytes);
    if (completed_ids == NULL) {
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }

    enif_mutex_lock(store->lock);
    memcpy(completed_ids, store->completion_journal + (size_t)cursor,
           result_bytes);
    enif_mutex_unlock(store->lock);
  }

  ERL_NIF_TERM ids = enif_make_list(env, 0);
  for (size_t index = result_count; index-- > 0;) {
    ids = enif_make_list_cell(env, enif_make_uint(env, completed_ids[index]),
                              ids);
  }

  enif_free(completed_ids);
  ErlNifUInt64 next_cursor = cursor + (ErlNifUInt64)result_count;
  return enif_make_tuple3(env, findex_atom(env, "ok"), ids,
                          enif_make_uint64(env, next_cursor));
}

ERL_NIF_TERM findex_nif_index_store_fetch(ErlNifEnv *env, int argc,
                                          const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  unsigned int directory_id;
  if (argc != 2 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store) ||
      !enif_get_uint(env, argv[1], &directory_id)) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(store->lock);
  directory_record_t *record = directory_record_locked(store, directory_id);
  if (record == NULL) {
    enif_mutex_unlock(store->lock);
    return findex_atom(env, "error");
  }

  uint32_t parent_id = record->parent_id;
  uint32_t error_number = record->error_number;
  enum directory_state state = record->state;
  directory_block_t *block = record->block;
  const unsigned char *name;
  size_t name_size;

  if (directory_id == ROOT_DIRECTORY_ID) {
    name = store->root_name;
    name_size = store->root_name_size;
  } else {
    directory_record_t *parent = directory_record_locked(store, parent_id);
    if (parent == NULL || parent->block == NULL ||
        !block_reference(store, parent->block, FIELD_NAME,
                         record->parent_entry_index, &name, &name_size)) {
      enif_mutex_unlock(store->lock);
      return findex_error_tuple(env, findex_atom(env, "invalid_tree"));
    }
  }
  enif_mutex_unlock(store->lock);

  ERL_NIF_TERM name_term = resource_binary(env, store, name, name_size);
  ERL_NIF_TERM parent_term = parent_id == NO_DIRECTORY_ID
                                 ? findex_atom(env, "nil")
                                 : enif_make_uint(env, parent_id);
  ERL_NIF_TERM entries = findex_atom(env, "nil");
  ERL_NIF_TERM children = enif_make_list(env, 0);

  if (state == DIRECTORY_PUBLISHED) {
    if (block == NULL) {
      return findex_error_tuple(env, findex_atom(env, "invalid_tree"));
    }
    if (!make_stored_batch(env, store, block, &entries)) {
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }

    unsigned char *entry_indices = block_child_entry_indices(store, block);
    for (size_t index = block->child_count; index-- > 0;) {
      uint32_t entry_index;
      uint32_t child_id = block->first_child_id + (uint32_t)index;
      memcpy(&entry_index, entry_indices + index * sizeof(uint32_t),
             sizeof(uint32_t));
      ERL_NIF_TERM child = enif_make_tuple2(
          env, enif_make_uint(env, entry_index), enif_make_uint(env, child_id));
      children = enif_make_list_cell(env, child, children);
    }
  } else if (block != NULL) {
    return findex_error_tuple(env, findex_atom(env, "invalid_tree"));
  }

  ERL_NIF_TERM state_term;
  ERL_NIF_TERM error_term = findex_atom(env, "nil");
  switch (state) {
  case DIRECTORY_PENDING:
    state_term = findex_atom(env, "pending");
    break;
  case DIRECTORY_PUBLISHED:
    state_term = findex_atom(env, "published");
    break;
  case DIRECTORY_FAILED:
    state_term = findex_atom(env, "failed");
    error_term = findex_errno_term(env, (int)error_number);
    break;
  default:
    return findex_error_tuple(env, findex_atom(env, "invalid_tree"));
  }

  ERL_NIF_TERM directory = enif_make_tuple6(
      env, state_term, parent_term, name_term, entries, children, error_term);
  return findex_ok_tuple(env, directory);
}

ERL_NIF_TERM findex_nif_index_store_stats(ErlNifEnv *env, int argc,
                                          const ERL_NIF_TERM argv[]) {
  index_store_t *store;
  if (argc != 1 ||
      !enif_get_resource(env, argv[0], index_store_type, (void **)&store)) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(store->lock);
  uint32_t directory_count = store->directory_count;
  uint32_t published_directory_count = store->published_directory_count;
  uint32_t failed_directory_count = store->failed_directory_count;
  uint32_t pending_directory_count =
      directory_count - published_directory_count - failed_directory_count;
  uint64_t entry_count = store->entry_count;
  uint64_t block_bytes = store->block_bytes;
  uint64_t payload_bytes = store->payload_bytes;
  size_t completion_count = store->completion_count;
  size_t completion_journal_bytes =
      store->completion_capacity * sizeof(*store->completion_journal);
  size_t directory_table_bytes =
      store->segment_count * sizeof(directory_segment_t) +
      store->segment_capacity * sizeof(*store->segments);
  size_t root_name_bytes = store->root_name_size;
  enif_mutex_unlock(store->lock);

  ERL_NIF_TERM keys[] = {
      findex_atom(env, "directory_count"),
      findex_atom(env, "published_directory_count"),
      findex_atom(env, "failed_directory_count"),
      findex_atom(env, "pending_directory_count"),
      findex_atom(env, "completion_count"),
      findex_atom(env, "entry_count"),
      findex_atom(env, "block_bytes"),
      findex_atom(env, "payload_bytes"),
      findex_atom(env, "directory_table_bytes"),
      findex_atom(env, "completion_journal_bytes"),
      findex_atom(env, "root_name_bytes"),
      findex_atom(env, "native_bytes"),
  };
  ERL_NIF_TERM values[] = {
      enif_make_uint(env, directory_count),
      enif_make_uint(env, published_directory_count),
      enif_make_uint(env, failed_directory_count),
      enif_make_uint(env, pending_directory_count),
      enif_make_uint64(env, completion_count),
      enif_make_uint64(env, entry_count),
      enif_make_uint64(env, block_bytes),
      enif_make_uint64(env, payload_bytes),
      enif_make_uint64(env, directory_table_bytes),
      enif_make_uint64(env, completion_journal_bytes),
      enif_make_uint64(env, root_name_bytes),
      enif_make_uint64(env, sizeof(*store) + sizeof(store_cleanup_job_t) +
                                block_bytes + directory_table_bytes +
                                completion_journal_bytes + root_name_bytes),
  };

  ERL_NIF_TERM result;
  if (!enif_make_map_from_arrays(env, keys, values,
                                 sizeof(keys) / sizeof(keys[0]), &result)) {
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }
  return result;
}

int findex_store_resource_init(ErlNifEnv *env) {
  index_store_type =
      enif_open_resource_type(env, NULL, "findex_index_store",
                              index_store_destructor, ERL_NIF_RT_CREATE, NULL);
  return index_store_type != NULL;
}
