#include "findex_nif.h"

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/kauth.h>
#include <sys/mount.h>
#include <sys/types.h>
#include <sys/vnode.h>
#include <unistd.h>

#define MIN_BUFFER_SIZE 4096U
#define MAX_BUFFER_SIZE (16U * 1024U * 1024U)
#define MIN_BULK_RECORD_SIZE                                                   \
  (sizeof(uint32_t) + sizeof(attribute_set_t) + sizeof(uint32_t) +             \
   sizeof(attrreference_t))

typedef struct {
  int fd;
  int exhausted;
  int output_format;
  struct attrlist attributes;
  uint64_t requested_fields;
  uint64_t options;
  ErlNifMutex *lock;
} directory_cursor_t;

typedef struct {
  const unsigned char *current;
  const unsigned char *end;
} buffer_reader_t;

static ErlNifResourceType *directory_cursor_type = NULL;

_Static_assert(sizeof(attrreference_t) == 8,
               "packed references require 32-bit offset and length fields");
_Static_assert(sizeof(attribute_set_t) == 20,
               "returned attribute parsing requires five 32-bit groups");
_Static_assert(sizeof(dev_t) == 4, "packed device IDs require 32-bit dev_t");
_Static_assert(sizeof(fsid_t) == 8, "packed filesystem IDs require 64 bits");
_Static_assert(sizeof(fsobj_type_t) == 4,
               "packed object types require 32-bit fsobj_type_t");
_Static_assert(sizeof(fsobj_tag_t) == 4,
               "packed object tags require 32-bit fsobj_tag_t");
_Static_assert(sizeof(uid_t) == 4, "packed owner IDs require 32-bit uid_t");
_Static_assert(sizeof(gid_t) == 4, "packed group IDs require 32-bit gid_t");
_Static_assert(sizeof(off_t) == 8, "packed sizes require 64-bit off_t");
_Static_assert(sizeof(struct timespec) == 16,
               "packed timestamps require two 64-bit fields");
_Static_assert(sizeof(guid_t) == 16, "packed UUIDs require 128-bit guid_t");

static int read_field(buffer_reader_t *reader, void *destination, size_t size) {
  if (reader->current > reader->end ||
      size > (size_t)(reader->end - reader->current)) {
    return 0;
  }

  memcpy(destination, reader->current, size);
  reader->current += size;
  return 1;
}

static int referenced_data(const unsigned char *record_start,
                           const unsigned char *record_end,
                           const unsigned char *reference_position,
                           const attrreference_t *reference, int trim_nul,
                           const unsigned char **result_data,
                           size_t *result_size) {
  ptrdiff_t reference_offset = reference_position - record_start;

  if (reference->attr_dataoffset < 0 || reference_offset < 0) {
    return 0;
  }

  size_t relative_offset = (size_t)reference_offset;
  size_t attribute_offset = (size_t)reference->attr_dataoffset;
  if (attribute_offset > SIZE_MAX - relative_offset) {
    return 0;
  }

  size_t data_offset = relative_offset + attribute_offset;
  size_t record_size = (size_t)(record_end - record_start);
  size_t data_size = (size_t)reference->attr_length;

  if (data_offset > record_size || data_size > record_size - data_offset) {
    return 0;
  }

  const unsigned char *data = record_start + data_offset;
  if (trim_nul && data_size > 0 && data[data_size - 1] == '\0') {
    data_size--;
  }

  *result_data = data;
  *result_size = data_size;
  return 1;
}

static int referenced_binary(ErlNifEnv *env, const unsigned char *record_start,
                             const unsigned char *record_end,
                             const unsigned char *reference_position,
                             const attrreference_t *reference, int trim_nul,
                             ERL_NIF_TERM *result) {
  const unsigned char *data;
  size_t data_size;
  if (!referenced_data(record_start, record_end, reference_position, reference,
                       trim_nul, &data, &data_size)) {
    return 0;
  }

  *result = findex_copy_binary(env, data, data_size);
  return 1;
}

static ERL_NIF_TERM timespec_term(ErlNifEnv *env,
                                  const struct timespec *value) {
  return enif_make_tuple2(env,
                          enif_make_int64(env, (ErlNifSInt64)value->tv_sec),
                          enif_make_int64(env, (ErlNifSInt64)value->tv_nsec));
}

static ERL_NIF_TERM fsid_term(ErlNifEnv *env, const fsid_t *value) {
  return enif_make_tuple2(env, enif_make_int(env, value->val[0]),
                          enif_make_int(env, value->val[1]));
}

static ERL_NIF_TERM file_type_term(ErlNifEnv *env, fsobj_type_t type) {
  switch (type) {
  case VREG:
    return findex_atom(env, "regular");
  case VDIR:
    return findex_atom(env, "directory");
  case VBLK:
    return findex_atom(env, "block_device");
  case VCHR:
    return findex_atom(env, "character_device");
  case VLNK:
    return findex_atom(env, "symlink");
  case VSOCK:
    return findex_atom(env, "socket");
  case VFIFO:
    return findex_atom(env, "fifo");
  default:
    return findex_atom(env, "unknown");
  }
}

static int add_requested_field(directory_cursor_t *cursor,
                               enum entry_field field) {
  if (field == FIELD_STRUCT || field == FIELD_FORK_COUNT ||
      field >= ENTRY_FIELD_COUNT) {
    return 0;
  }

  cursor->requested_fields |= FIELD_BIT(field);

  switch (field) {
  case FIELD_NAME:
  case FIELD_ERROR:
  case FIELD_RETURNED_ATTRIBUTES:
    /* Required internally and always enabled. */
    return 1;
  case FIELD_TYPE:
    cursor->attributes.commonattr |= ATTR_CMN_OBJTYPE;
    return 1;
  case FIELD_OBJECT_TAG:
    cursor->attributes.commonattr |= ATTR_CMN_OBJTAG;
    return 1;
  case FIELD_DEVICE:
    cursor->attributes.commonattr |= ATTR_CMN_DEVID;
    return 1;
  case FIELD_FILESYSTEM_ID:
    cursor->attributes.commonattr |= ATTR_CMN_FSID;
    return 1;
  case FIELD_FILE_ID:
    cursor->attributes.commonattr |= ATTR_CMN_FILEID;
    return 1;
  case FIELD_PARENT_ID:
    cursor->attributes.commonattr |= ATTR_CMN_PARENTID;
    return 1;
  case FIELD_CREATED_AT:
    cursor->attributes.commonattr |= ATTR_CMN_CRTIME;
    return 1;
  case FIELD_MODIFIED_AT:
    cursor->attributes.commonattr |= ATTR_CMN_MODTIME;
    return 1;
  case FIELD_CHANGED_AT:
    cursor->attributes.commonattr |= ATTR_CMN_CHGTIME;
    return 1;
  case FIELD_ACCESSED_AT:
    cursor->attributes.commonattr |= ATTR_CMN_ACCTIME;
    return 1;
  case FIELD_BACKED_UP_AT:
    cursor->attributes.commonattr |= ATTR_CMN_BKUPTIME;
    return 1;
  case FIELD_ADDED_AT:
    cursor->attributes.commonattr |= ATTR_CMN_ADDEDTIME;
    return 1;
  case FIELD_OWNER_ID:
    cursor->attributes.commonattr |= ATTR_CMN_OWNERID;
    return 1;
  case FIELD_GROUP_ID:
    cursor->attributes.commonattr |= ATTR_CMN_GRPID;
    return 1;
  case FIELD_MODE:
    cursor->attributes.commonattr |= ATTR_CMN_ACCESSMASK;
    return 1;
  case FIELD_FLAGS:
    cursor->attributes.commonattr |= ATTR_CMN_FLAGS;
    return 1;
  case FIELD_USER_ACCESS:
    cursor->attributes.commonattr |= ATTR_CMN_USERACCESS;
    return 1;
  case FIELD_FINDER_INFO:
    cursor->attributes.commonattr |= ATTR_CMN_FNDRINFO;
    return 1;
  case FIELD_OWNER_UUID:
    cursor->attributes.commonattr |= ATTR_CMN_UUID;
    return 1;
  case FIELD_GROUP_UUID:
    cursor->attributes.commonattr |= ATTR_CMN_GRPUUID;
    return 1;
  case FIELD_ACL:
    cursor->attributes.commonattr |= ATTR_CMN_EXTENDED_SECURITY;
    return 1;
  case FIELD_DATA_PROTECTION_FLAGS:
    cursor->attributes.commonattr |= ATTR_CMN_DATA_PROTECT_FLAGS;
    return 1;
  case FIELD_GENERATION_COUNT:
    cursor->attributes.commonattr |= ATTR_CMN_GEN_COUNT;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_DOCUMENT_ID:
    cursor->attributes.commonattr |= ATTR_CMN_DOCUMENT_ID;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_LINK_COUNT:
    cursor->attributes.dirattr |= ATTR_DIR_LINKCOUNT;
    cursor->attributes.fileattr |= ATTR_FILE_LINKCOUNT;
    return 1;
  case FIELD_TOTAL_SIZE:
    cursor->attributes.fileattr |= ATTR_FILE_TOTALSIZE;
    return 1;
  case FIELD_ALLOCATED_SIZE:
    cursor->attributes.dirattr |= ATTR_DIR_ALLOCSIZE;
    cursor->attributes.fileattr |= ATTR_FILE_ALLOCSIZE;
    return 1;
  case FIELD_IO_BLOCK_SIZE:
    cursor->attributes.dirattr |= ATTR_DIR_IOBLOCKSIZE;
    cursor->attributes.fileattr |= ATTR_FILE_IOBLOCKSIZE;
    return 1;
  case FIELD_DEVICE_TYPE:
    cursor->attributes.fileattr |= ATTR_FILE_DEVTYPE;
    return 1;
  case FIELD_FORK_COUNT:
    /* Declared by the SDK but not reliably vended by getattrlistbulk(). */
    return 0;
  case FIELD_DATA_SIZE:
    cursor->attributes.dirattr |= ATTR_DIR_DATALENGTH;
    cursor->attributes.fileattr |= ATTR_FILE_DATALENGTH;
    return 1;
  case FIELD_DATA_ALLOCATED_SIZE:
    cursor->attributes.fileattr |= ATTR_FILE_DATAALLOCSIZE;
    return 1;
  case FIELD_RESOURCE_FORK_SIZE:
    cursor->attributes.fileattr |= ATTR_FILE_RSRCLENGTH;
    return 1;
  case FIELD_RESOURCE_FORK_ALLOCATED_SIZE:
    cursor->attributes.fileattr |= ATTR_FILE_RSRCALLOCSIZE;
    return 1;
  case FIELD_DIRECTORY_ENTRY_COUNT:
    cursor->attributes.dirattr |= ATTR_DIR_ENTRYCOUNT;
    return 1;
  case FIELD_MOUNT_STATUS:
    cursor->attributes.dirattr |= ATTR_DIR_MOUNTSTATUS;
    return 1;
  case FIELD_PRIVATE_SIZE:
    cursor->attributes.forkattr |= ATTR_CMNEXT_PRIVATESIZE;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_LINK_ID:
    cursor->attributes.forkattr |= ATTR_CMNEXT_LINKID;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_REAL_DEVICE:
    cursor->attributes.forkattr |= ATTR_CMNEXT_REALDEVID;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_REAL_FILESYSTEM_ID:
    cursor->attributes.forkattr |= ATTR_CMNEXT_REALFSID;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_CLONE_ID:
    cursor->attributes.forkattr |= ATTR_CMNEXT_CLONEID;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_EXTENDED_FLAGS:
    cursor->attributes.forkattr |= ATTR_CMNEXT_EXT_FLAGS;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_RECURSIVE_GENERATION_COUNT:
    cursor->attributes.forkattr |= ATTR_CMNEXT_RECURSIVE_GENCOUNT;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_ATTRIBUTION_TAG:
    cursor->attributes.forkattr |= ATTR_CMNEXT_ATTRIBUTION_TAG;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_CLONE_REFERENCE_COUNT:
    cursor->attributes.forkattr |= ATTR_CMNEXT_CLONE_REFCNT;
    cursor->options |= FSOPT_ATTR_CMN_EXTENDED;
    return 1;
  case FIELD_STRUCT:
  case ENTRY_FIELD_COUNT:
    return 0;
  }

  return 0;
}

static int configure_attributes(ErlNifEnv *env, directory_cursor_t *cursor,
                                ERL_NIF_TERM fields) {
  memset(&cursor->attributes, 0, sizeof(cursor->attributes));
  cursor->attributes.bitmapcount = ATTR_BIT_MAP_COUNT;
  cursor->attributes.commonattr =
      ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_ERROR;
  cursor->requested_fields = FIELD_BIT(FIELD_NAME) | FIELD_BIT(FIELD_ERROR) |
                             FIELD_BIT(FIELD_RETURNED_ATTRIBUTES);
  cursor->options = FSOPT_PACK_INVAL_ATTRS;

  ERL_NIF_TERM head;
  ERL_NIF_TERM tail = fields;
  while (enif_get_list_cell(env, tail, &head, &tail)) {
    enum entry_field field;
    if (!findex_entry_field_from_term(env, head, &field) ||
        !add_requested_field(cursor, field)) {
      return 0;
    }
  }

  if (!enif_is_empty_list(env, tail)) {
    return 0;
  }

  /*
   * FSOPT_PACK_INVAL_ATTRS packs default values for unsupported attributes.
   * Directory and file attribute groups therefore have to be selected using
   * ATTR_CMN_OBJTYPE, not by guessing from the returned-validity bitmaps.
   */
  if (cursor->attributes.dirattr != 0 || cursor->attributes.fileattr != 0) {
    cursor->attributes.commonattr |= ATTR_CMN_OBJTYPE;
  }

  return 1;
}

static int valid_returned_attributes(const directory_cursor_t *cursor,
                                     const attribute_set_t *returned) {
  return returned->volattr == 0 &&
         (returned->commonattr & ~cursor->attributes.commonattr) == 0 &&
         (returned->dirattr & ~cursor->attributes.dirattr) == 0 &&
         (returned->fileattr & ~cursor->attributes.fileattr) == 0 &&
         (returned->forkattr & ~cursor->attributes.forkattr) == 0;
}

typedef struct {
  int included;
  size_t width;
  unsigned char *data;
  unsigned char *validity;
  ERL_NIF_TERM data_term;
  ERL_NIF_TERM validity_term;
} packed_column_t;

static int
initialize_packed_columns(ErlNifEnv *env, const directory_cursor_t *cursor,
                          size_t count,
                          packed_column_t columns[ENTRY_FIELD_COUNT]) {
  memset(columns, 0, sizeof(*columns) * ENTRY_FIELD_COUNT);
  size_t validity_size = (count + 7U) / 8U;

  for (size_t index = FIELD_NAME; index < ENTRY_FIELD_COUNT; index++) {
    if ((cursor->requested_fields & FIELD_BIT(index)) == 0) {
      continue;
    }

    size_t width = findex_packed_field_width((enum entry_field)index);
    if (width == 0 || count > SIZE_MAX / width) {
      return 0;
    }

    packed_column_t *column = &columns[index];
    column->included = 1;
    column->width = width;
    column->data = enif_make_new_binary(env, count * width, &column->data_term);
    column->validity =
        enif_make_new_binary(env, validity_size, &column->validity_term);

    if (column->data == NULL || column->validity == NULL) {
      return 0;
    }

    memset(column->data, 0, count * width);
    memset(column->validity, 0, validity_size);
  }

  return 1;
}

static int initialize_native_packed_columns(
    const directory_cursor_t *cursor, size_t count,
    findex_native_batch_t *batch,
    packed_column_t columns[ENTRY_FIELD_COUNT]) {
  memset(columns, 0, sizeof(*columns) * ENTRY_FIELD_COUNT);
  memset(batch, 0, sizeof(*batch));
  batch->count = count;

  size_t validity_size = (count + 7U) / 8U;
  size_t allocation_size = 0;
  for (size_t index = FIELD_NAME; index < ENTRY_FIELD_COUNT; index++) {
    if ((cursor->requested_fields & FIELD_BIT(index)) == 0) {
      continue;
    }

    size_t column_size;
    size_t width = findex_packed_field_width((enum entry_field)index);
    if (width == 0 ||
        !findex_checked_multiply_size(count, width, &column_size) ||
        !findex_checked_add_size(allocation_size, column_size,
                                 &allocation_size) ||
        !findex_checked_add_size(allocation_size, validity_size,
                                 &allocation_size)) {
      return 0;
    }
  }

  batch->column_allocation = enif_alloc(allocation_size);
  if (batch->column_allocation == NULL) {
    return 0;
  }
  memset(batch->column_allocation, 0, allocation_size);

  size_t offset = 0;
  for (size_t index = FIELD_NAME; index < ENTRY_FIELD_COUNT; index++) {
    if ((cursor->requested_fields & FIELD_BIT(index)) == 0) {
      continue;
    }

    size_t width = findex_packed_field_width((enum entry_field)index);
    size_t column_size = count * width;
    packed_column_t *column = &columns[index];
    column->included = 1;
    column->width = width;
    column->data = batch->column_allocation + offset;
    batch->columns[index] = column->data;
    offset += column_size;
    column->validity = batch->column_allocation + offset;
    batch->validity[index] = column->validity;
    offset += validity_size;
  }

  return 1;
}

static void packed_store(packed_column_t columns[ENTRY_FIELD_COUNT],
                         enum entry_field field, size_t index,
                         const void *value, size_t size) {
  packed_column_t *column = &columns[field];
  if (!column->included || size != column->width) {
    return;
  }

  memcpy(column->data + index * column->width, value, size);
  column->validity[index / 8U] |= (unsigned char)(1U << (index % 8U));
}

static int packed_store_reference(packed_column_t columns[ENTRY_FIELD_COUNT],
                                  enum entry_field field, size_t index,
                                  const unsigned char *storage_start,
                                  const unsigned char *data, size_t size) {
  ptrdiff_t offset = data - storage_start;
  if (offset < 0 || (uint64_t)offset > UINT32_MAX || size > UINT32_MAX) {
    return 0;
  }

  uint32_t reference[2] = {(uint32_t)offset, (uint32_t)size};
  packed_store(columns, field, index, reference, sizeof(reference));
  return 1;
}

static unsigned char packed_type_code(fsobj_type_t type) {
  switch (type) {
  case VREG:
    return 1;
  case VDIR:
    return 2;
  case VLNK:
    return 3;
  case VBLK:
    return 4;
  case VCHR:
    return 5;
  case VSOCK:
    return 6;
  case VFIFO:
    return 7;
  default:
    return 0;
  }
}

static int make_packed_batch(ErlNifEnv *env, size_t count, ERL_NIF_TERM storage,
                             packed_column_t columns[ENTRY_FIELD_COUNT],
                             ERL_NIF_TERM *result) {
  ERL_NIF_TERM column_keys[ENTRY_FIELD_COUNT];
  ERL_NIF_TERM column_values[ENTRY_FIELD_COUNT];
  ERL_NIF_TERM validity_values[ENTRY_FIELD_COUNT];
  size_t field_count = 0;
  ERL_NIF_TERM fields = enif_make_list(env, 0);

  for (size_t index = FIELD_NAME; index < ENTRY_FIELD_COUNT; index++) {
    if (!columns[index].included) {
      continue;
    }

    column_keys[field_count] =
        findex_atom(env, findex_entry_field_names[index]);
    column_values[field_count] = columns[index].data_term;
    validity_values[field_count] = columns[index].validity_term;
    field_count++;
  }

  for (size_t index = ENTRY_FIELD_COUNT; index-- > FIELD_NAME;) {
    if (columns[index].included) {
      fields = enif_make_list_cell(
          env, findex_atom(env, findex_entry_field_names[index]), fields);
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
      enif_make_uint64(env, count),
      fields,
      storage,
      column_map,
      validity_map,
  };

  return enif_make_map_from_arrays(env, keys, values,
                                   sizeof(keys) / sizeof(keys[0]), result);
}

#define READ_PACKED_FIXED(reader, requested, returned, attribute, type, field, \
                          columns, index)                                      \
  do {                                                                         \
    if ((requested) & (attribute)) {                                           \
      type packed_value;                                                       \
      if (!read_field((reader), &packed_value, sizeof(packed_value))) {        \
        return 0;                                                              \
      }                                                                        \
      if ((returned) & (attribute)) {                                          \
        packed_store((columns), (field), (index), &packed_value,               \
                     sizeof(packed_value));                                    \
      }                                                                        \
    }                                                                          \
  } while (0)

/* Decoders for the packed and materialized output formats. */

static int parse_packed_entry(const directory_cursor_t *cursor,
                              const unsigned char *storage_start,
                              const unsigned char *record_start,
                              const unsigned char *record_end, size_t index,
                              packed_column_t columns[ENTRY_FIELD_COUNT]) {
  buffer_reader_t reader = {
      .current = record_start + sizeof(uint32_t),
      .end = record_end,
  };

  attribute_set_t returned;
  if (!read_field(&reader, &returned, sizeof(returned)) ||
      !valid_returned_attributes(cursor, &returned)) {
    return 0;
  }

  uint32_t returned_attributes[4] = {
      returned.commonattr,
      returned.dirattr,
      returned.fileattr,
      returned.forkattr,
  };
  packed_store(columns, FIELD_RETURNED_ATTRIBUTES, index, returned_attributes,
               sizeof(returned_attributes));

  uint32_t entry_error;
  if (!read_field(&reader, &entry_error, sizeof(entry_error))) {
    return 0;
  }
  if (entry_error != 0) {
    packed_store(columns, FIELD_ERROR, index, &entry_error,
                 sizeof(entry_error));
  }

  const unsigned char *reference_position = reader.current;
  attrreference_t reference;
  if (!read_field(&reader, &reference, sizeof(reference))) {
    return 0;
  }
  if (returned.commonattr & ATTR_CMN_NAME) {
    const unsigned char *name;
    size_t name_size;
    if (!referenced_data(record_start, record_end, reference_position,
                         &reference, 1, &name, &name_size) ||
        !packed_store_reference(columns, FIELD_NAME, index, storage_start, name,
                                name_size)) {
      return 0;
    }
  }

  if (entry_error != 0) {
    return 1;
  }

  if ((returned.commonattr & ATTR_CMN_NAME) == 0) {
    return 0;
  }

  if (cursor->attributes.commonattr & ATTR_CMN_DEVID) {
    dev_t device;
    if (!read_field(&reader, &device, sizeof(device))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_DEVID) {
      uint64_t normalized_device = (uint64_t)(uint32_t)device;
      packed_store(columns, FIELD_DEVICE, index, &normalized_device,
                   sizeof(normalized_device));
    }
  }

  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_FSID, fsid_t, FIELD_FILESYSTEM_ID, columns, index);

  fsobj_type_t object_type = VNON;
  int object_type_returned = 0;
  if (cursor->attributes.commonattr & ATTR_CMN_OBJTYPE) {
    if (!read_field(&reader, &object_type, sizeof(object_type))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_OBJTYPE) {
      object_type_returned = 1;
      unsigned char type_code = packed_type_code(object_type);
      packed_store(columns, FIELD_TYPE, index, &type_code, sizeof(type_code));
    }
  }

  if ((cursor->attributes.dirattr != 0 || cursor->attributes.fileattr != 0) &&
      !object_type_returned) {
    return 0;
  }

  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_OBJTAG, fsobj_tag_t, FIELD_OBJECT_TAG, columns,
                    index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_CRTIME, struct timespec, FIELD_CREATED_AT, columns,
                    index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_MODTIME, struct timespec, FIELD_MODIFIED_AT,
                    columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_CHGTIME, struct timespec, FIELD_CHANGED_AT,
                    columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_ACCTIME, struct timespec, FIELD_ACCESSED_AT,
                    columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_BKUPTIME, struct timespec, FIELD_BACKED_UP_AT,
                    columns, index);

  if (cursor->attributes.commonattr & ATTR_CMN_FNDRINFO) {
    unsigned char finder_info[32];
    if (!read_field(&reader, finder_info, sizeof(finder_info))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_FNDRINFO) {
      packed_store(columns, FIELD_FINDER_INFO, index, finder_info,
                   sizeof(finder_info));
    }
  }

  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_OWNERID, uid_t, FIELD_OWNER_ID, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_GRPID, gid_t, FIELD_GROUP_ID, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_ACCESSMASK, uint32_t, FIELD_MODE, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_FLAGS, uint32_t, FIELD_FLAGS, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_GEN_COUNT, uint32_t, FIELD_GENERATION_COUNT,
                    columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_DOCUMENT_ID, uint32_t, FIELD_DOCUMENT_ID, columns,
                    index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_USERACCESS, uint32_t, FIELD_USER_ACCESS, columns,
                    index);

  if (cursor->attributes.commonattr & ATTR_CMN_EXTENDED_SECURITY) {
    reference_position = reader.current;
    if (!read_field(&reader, &reference, sizeof(reference))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_EXTENDED_SECURITY) {
      const unsigned char *acl;
      size_t acl_size;
      if (!referenced_data(record_start, record_end, reference_position,
                           &reference, 0, &acl, &acl_size) ||
          !packed_store_reference(columns, FIELD_ACL, index, storage_start, acl,
                                  acl_size)) {
        return 0;
      }
    }
  }

  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_UUID, guid_t, FIELD_OWNER_UUID, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_GRPUUID, guid_t, FIELD_GROUP_UUID, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_FILEID, uint64_t, FIELD_FILE_ID, columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_PARENTID, uint64_t, FIELD_PARENT_ID, columns,
                    index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_ADDEDTIME, struct timespec, FIELD_ADDED_AT,
                    columns, index);
  READ_PACKED_FIXED(&reader, cursor->attributes.commonattr, returned.commonattr,
                    ATTR_CMN_DATA_PROTECT_FLAGS, uint32_t,
                    FIELD_DATA_PROTECTION_FLAGS, columns, index);

  if (object_type == VDIR) {
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_LINKCOUNT, uint32_t, FIELD_LINK_COUNT, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_ENTRYCOUNT, uint32_t,
                      FIELD_DIRECTORY_ENTRY_COUNT, columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_MOUNTSTATUS, uint32_t, FIELD_MOUNT_STATUS,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_ALLOCSIZE, off_t, FIELD_ALLOCATED_SIZE, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_IOBLOCKSIZE, uint32_t, FIELD_IO_BLOCK_SIZE,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.dirattr, returned.dirattr,
                      ATTR_DIR_DATALENGTH, off_t, FIELD_DATA_SIZE, columns,
                      index);
  } else {
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_LINKCOUNT, uint32_t, FIELD_LINK_COUNT, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_TOTALSIZE, off_t, FIELD_TOTAL_SIZE, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_ALLOCSIZE, off_t, FIELD_ALLOCATED_SIZE, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_IOBLOCKSIZE, uint32_t, FIELD_IO_BLOCK_SIZE,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_DEVTYPE, uint32_t, FIELD_DEVICE_TYPE, columns,
                      index);

    if (cursor->attributes.fileattr & ATTR_FILE_FORKCOUNT) {
      uint32_t ignored_fork_count;
      if (!read_field(&reader, &ignored_fork_count,
                      sizeof(ignored_fork_count))) {
        return 0;
      }
    }

    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_DATALENGTH, off_t, FIELD_DATA_SIZE, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_DATAALLOCSIZE, off_t, FIELD_DATA_ALLOCATED_SIZE,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_RSRCLENGTH, off_t, FIELD_RESOURCE_FORK_SIZE,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.fileattr, returned.fileattr,
                      ATTR_FILE_RSRCALLOCSIZE, off_t,
                      FIELD_RESOURCE_FORK_ALLOCATED_SIZE, columns, index);
  }

  if (cursor->attributes.forkattr != 0) {
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_PRIVATESIZE, off_t, FIELD_PRIVATE_SIZE,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_LINKID, uint64_t, FIELD_LINK_ID, columns,
                      index);

    if (cursor->attributes.forkattr & ATTR_CMNEXT_REALDEVID) {
      dev_t device;
      if (!read_field(&reader, &device, sizeof(device))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_REALDEVID) {
        uint64_t normalized_device = (uint64_t)(uint32_t)device;
        packed_store(columns, FIELD_REAL_DEVICE, index, &normalized_device,
                     sizeof(normalized_device));
      }
    }

    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_REALFSID, fsid_t, FIELD_REAL_FILESYSTEM_ID,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_CLONEID, uint64_t, FIELD_CLONE_ID, columns,
                      index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_EXT_FLAGS, uint64_t, FIELD_EXTENDED_FLAGS,
                      columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_RECURSIVE_GENCOUNT, uint64_t,
                      FIELD_RECURSIVE_GENERATION_COUNT, columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_ATTRIBUTION_TAG, uint64_t,
                      FIELD_ATTRIBUTION_TAG, columns, index);
    READ_PACKED_FIXED(&reader, cursor->attributes.forkattr, returned.forkattr,
                      ATTR_CMNEXT_CLONE_REFCNT, uint32_t,
                      FIELD_CLONE_REFERENCE_COUNT, columns, index);
  }

  return 1;
}

#undef READ_PACKED_FIXED

static int scan_bit_set(const unsigned char *bitmap, size_t index) {
  return (bitmap[index / 8U] &
          (unsigned char)(1U << (index % 8U))) != 0;
}

static int ensure_scan_child_capacity(findex_directory_scan_t *scan,
                                      size_t required_count) {
  if (required_count <= scan->child_capacity) {
    return 1;
  }

  size_t capacity = scan->child_capacity == 0 ? 64U : scan->child_capacity;
  while (capacity < required_count) {
    if (capacity > SIZE_MAX / 2U) {
      return 0;
    }
    capacity *= 2U;
  }

  size_t bytes;
  if (!findex_checked_multiply_size(capacity,
                                    sizeof(*scan->child_entry_indices),
                                    &bytes)) {
    return 0;
  }

  uint32_t *indices = enif_realloc(scan->child_entry_indices, bytes);
  if (indices == NULL) {
    return 0;
  }
  scan->child_entry_indices = indices;
  scan->child_capacity = capacity;
  return 1;
}

static int ensure_scan_error_capacity(findex_directory_scan_t *scan,
                                      size_t required_count) {
  if (required_count <= scan->error_capacity) {
    return 1;
  }

  size_t capacity = scan->error_capacity == 0 ? 8U : scan->error_capacity;
  while (capacity < required_count) {
    if (capacity > SIZE_MAX / 2U) {
      return 0;
    }
    capacity *= 2U;
  }

  size_t bytes;
  if (!findex_checked_multiply_size(capacity, sizeof(*scan->error_counts),
                                    &bytes)) {
    return 0;
  }

  findex_error_count_t *counts = enif_realloc(scan->error_counts, bytes);
  if (counts == NULL) {
    return 0;
  }
  scan->error_counts = counts;
  scan->error_capacity = capacity;
  return 1;
}

static int record_scan_error(findex_directory_scan_t *scan,
                             uint32_t error_number) {
  for (size_t index = 0; index < scan->error_count; index++) {
    if (scan->error_counts[index].error_number == error_number) {
      if (scan->error_counts[index].count == UINT64_MAX) {
        return 0;
      }
      scan->error_counts[index].count++;
      return 1;
    }
  }

  size_t required_count;
  if (!findex_checked_add_size(scan->error_count, 1U, &required_count) ||
      !ensure_scan_error_capacity(scan, required_count)) {
    return 0;
  }

  scan->error_counts[scan->error_count].error_number = error_number;
  scan->error_counts[scan->error_count].count = 1U;
  scan->error_count = required_count;
  return 1;
}

static int analyze_native_batch(findex_directory_scan_t *scan,
                                const findex_native_batch_t *batch,
                                enum traversal_mount_policy mount_policy) {
  const unsigned char *types = batch->columns[FIELD_TYPE];
  const unsigned char *type_validity = batch->validity[FIELD_TYPE];
  const unsigned char *errors = batch->columns[FIELD_ERROR];
  const unsigned char *error_validity = batch->validity[FIELD_ERROR];
  const unsigned char *mount_status = batch->columns[FIELD_MOUNT_STATUS];
  const unsigned char *mount_validity = batch->validity[FIELD_MOUNT_STATUS];

  if (types == NULL || type_validity == NULL || errors == NULL ||
      error_validity == NULL ||
      (mount_policy == TRAVERSAL_STAY_ON_FILESYSTEM &&
       (mount_status == NULL || mount_validity == NULL)) ||
      scan->entries > UINT32_MAX ||
      batch->count > (size_t)(UINT32_MAX - scan->entries)) {
    return 0;
  }

  uint64_t entry_offset = scan->entries;
  for (size_t index = 0; index < batch->count; index++) {
    int has_error = scan_bit_set(error_validity, index);
    if (has_error) {
      uint32_t error_number;
      memcpy(&error_number, errors + index * sizeof(error_number),
             sizeof(error_number));
      if (!record_scan_error(scan, error_number)) {
        return 0;
      }
      scan->metadata_errors++;
    }

    int valid_type = scan_bit_set(type_validity, index);
    unsigned char type = types[index];
    if (!valid_type) {
      scan->other++;
      continue;
    }

    switch (type) {
    case 1U:
      scan->regular_files++;
      break;
    case 2U:
      scan->directories++;
      break;
    case 3U:
      scan->symlinks++;
      break;
    default:
      scan->other++;
      break;
    }

    if (type != 2U || has_error) {
      continue;
    }

    int traverse = mount_policy == TRAVERSAL_CROSS_MOUNTS;
    if (!traverse && scan_bit_set(mount_validity, index)) {
      uint32_t status;
      memcpy(&status, mount_status + index * sizeof(status), sizeof(status));
      traverse = (status &
                  (uint32_t)(DIR_MNTSTATUS_MNTPOINT |
                             DIR_MNTSTATUS_TRIGGER)) == 0U;
    }

    if (traverse) {
      size_t required_count;
      if (!findex_checked_add_size(scan->child_count, 1U, &required_count) ||
          !ensure_scan_child_capacity(scan, required_count)) {
        return 0;
      }
      scan->child_entry_indices[scan->child_count] =
          (uint32_t)(entry_offset + index);
      scan->child_count = required_count;
    } else {
      scan->skipped_mounts++;
    }
  }

  scan->entries += batch->count;
  return 1;
}

void findex_directory_scan_destroy(findex_directory_scan_t *scan) {
  findex_native_batch_t *batch = scan->first_batch;
  while (batch != NULL) {
    findex_native_batch_t *next = batch->next;
    enif_free(batch->storage);
    enif_free(batch->column_allocation);
    enif_free(batch);
    batch = next;
  }

  enif_free(scan->child_entry_indices);
  enif_free(scan->error_counts);
  memset(scan, 0, sizeof(*scan));
}

enum findex_scan_status findex_scan_directory_fd(
    ErlNifEnv *env, int fd, ERL_NIF_TERM fields, unsigned int buffer_size,
    enum traversal_mount_policy mount_policy, findex_directory_scan_t *scan,
    int *error_number) {
  memset(scan, 0, sizeof(*scan));
  *error_number = 0;

  directory_cursor_t cursor;
  memset(&cursor, 0, sizeof(cursor));
  cursor.fd = fd;
  cursor.output_format = OUTPUT_PACKED;

  if (fd < 0 || buffer_size < MIN_BUFFER_SIZE ||
      buffer_size > MAX_BUFFER_SIZE ||
      (mount_policy != TRAVERSAL_CROSS_MOUNTS &&
       mount_policy != TRAVERSAL_STAY_ON_FILESYSTEM) ||
      !configure_attributes(env, &cursor, fields) ||
      (cursor.requested_fields & FIELD_BIT(FIELD_TYPE)) == 0 ||
      (mount_policy == TRAVERSAL_STAY_ON_FILESYSTEM &&
       (cursor.requested_fields & FIELD_BIT(FIELD_MOUNT_STATUS)) == 0)) {
    return FINDEX_SCAN_INVALID_ARGUMENT;
  }
  scan->requested_fields = cursor.requested_fields;

  size_t current_buffer_size = buffer_size;
  unsigned char *buffer = enif_alloc(current_buffer_size);
  if (buffer == NULL) {
    return FINDEX_SCAN_OUT_OF_MEMORY;
  }

  enum findex_scan_status result = FINDEX_SCAN_OK;
  for (;;) {
    int count;
    do {
      count = getattrlistbulk(fd, &cursor.attributes, buffer,
                              current_buffer_size, cursor.options);
    } while (count < 0 && errno == EINTR);

    if (count < 0 && errno == ERANGE && current_buffer_size < MAX_BUFFER_SIZE) {
      size_t larger_size = current_buffer_size * 2U;
      if (larger_size > MAX_BUFFER_SIZE) {
        larger_size = MAX_BUFFER_SIZE;
      }
      unsigned char *larger_buffer = enif_realloc(buffer, larger_size);
      if (larger_buffer == NULL) {
        result = FINDEX_SCAN_OUT_OF_MEMORY;
        break;
      }
      buffer = larger_buffer;
      current_buffer_size = larger_size;
      continue;
    }

    if (count < 0) {
      *error_number = errno;
      result = FINDEX_SCAN_SYSTEM_ERROR;
      break;
    }
    if (count == 0) {
      break;
    }
    if ((size_t)count > current_buffer_size / MIN_BULK_RECORD_SIZE) {
      result = FINDEX_SCAN_INVALID_RECORD;
      break;
    }

    findex_native_batch_t *batch = enif_alloc(sizeof(*batch));
    packed_column_t columns[ENTRY_FIELD_COUNT];
    if (batch == NULL ||
        !initialize_native_packed_columns(&cursor, (size_t)count, batch,
                                          columns)) {
      enif_free(batch);
      result = FINDEX_SCAN_OUT_OF_MEMORY;
      break;
    }

    const unsigned char *record = buffer;
    const unsigned char *buffer_end = buffer + current_buffer_size;
    for (int index = 0; index < count; index++) {
      if ((size_t)(buffer_end - record) < sizeof(uint32_t)) {
        result = FINDEX_SCAN_INVALID_RECORD;
        break;
      }

      uint32_t record_size;
      memcpy(&record_size, record, sizeof(record_size));
      if (record_size < sizeof(uint32_t) ||
          (size_t)record_size > (size_t)(buffer_end - record)) {
        result = FINDEX_SCAN_INVALID_RECORD;
        break;
      }

      const unsigned char *record_end = record + record_size;
      if (!parse_packed_entry(&cursor, buffer, record, record_end,
                              (size_t)index, columns)) {
        result = FINDEX_SCAN_INVALID_RECORD;
        break;
      }
      record = record_end;
    }

    if (result != FINDEX_SCAN_OK) {
      enif_free(batch->column_allocation);
      enif_free(batch);
      break;
    }

    batch->storage_size = (size_t)(record - buffer);
    batch->storage = enif_alloc(batch->storage_size);
    if (batch->storage == NULL) {
      enif_free(batch->column_allocation);
      enif_free(batch);
      result = FINDEX_SCAN_OUT_OF_MEMORY;
      break;
    }
    memcpy(batch->storage, buffer, batch->storage_size);

    if (scan->last_batch == NULL) {
      scan->first_batch = batch;
    } else {
      scan->last_batch->next = batch;
    }
    scan->last_batch = batch;

    if (!analyze_native_batch(scan, batch, mount_policy)) {
      result = FINDEX_SCAN_OUT_OF_MEMORY;
      break;
    }
  }

  enif_free(buffer);
  return result;
}

static int parse_entry(ErlNifEnv *env, const directory_cursor_t *cursor,
                       const unsigned char *record_start,
                       const unsigned char *record_end,
                       ERL_NIF_TERM keys[ENTRY_FIELD_COUNT],
                       ERL_NIF_TERM *result) {
  buffer_reader_t reader = {
      .current = record_start + sizeof(uint32_t),
      .end = record_end,
  };
  ERL_NIF_TERM values[ENTRY_FIELD_COUNT];
  ERL_NIF_TERM nil = findex_atom(env, "nil");

  for (size_t index = 0; index < ENTRY_FIELD_COUNT; index++) {
    values[index] = nil;
  }
  values[FIELD_STRUCT] = findex_atom(env, "Elixir.Findex.Entry");

  attribute_set_t returned;
  if (!read_field(&reader, &returned, sizeof(returned)) ||
      !valid_returned_attributes(cursor, &returned)) {
    return 0;
  }

  uint32_t entry_error;
  if (!read_field(&reader, &entry_error, sizeof(entry_error))) {
    return 0;
  }
  if (entry_error != 0) {
    values[FIELD_ERROR] = findex_errno_term(env, (int)entry_error);
  }

  const unsigned char *reference_position = reader.current;
  attrreference_t reference;
  if (!read_field(&reader, &reference, sizeof(reference))) {
    return 0;
  }
  if (returned.commonattr & ATTR_CMN_NAME) {
    if (!referenced_binary(env, record_start, record_end, reference_position,
                           &reference, 1, &values[FIELD_NAME])) {
      return 0;
    }
  }

  if (entry_error != 0) {
    ERL_NIF_TERM returned_values[] = {
        enif_make_uint(env, returned.commonattr),
        enif_make_uint(env, returned.dirattr),
        enif_make_uint(env, returned.fileattr),
        enif_make_uint(env, returned.forkattr),
    };
    values[FIELD_RETURNED_ATTRIBUTES] =
        enif_make_tuple_from_array(env, returned_values, 4);
    return enif_make_map_from_arrays(env, keys, values, ENTRY_FIELD_COUNT,
                                     result);
  }

  if ((returned.commonattr & ATTR_CMN_NAME) == 0) {
    return 0;
  }

  dev_t device;
  if (cursor->attributes.commonattr & ATTR_CMN_DEVID) {
    if (!read_field(&reader, &device, sizeof(device))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_DEVID) {
      values[FIELD_DEVICE] =
          enif_make_uint64(env, (ErlNifUInt64)(uint32_t)device);
    }
  }

  fsid_t filesystem_id;
  if (cursor->attributes.commonattr & ATTR_CMN_FSID) {
    if (!read_field(&reader, &filesystem_id, sizeof(filesystem_id))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_FSID) {
      values[FIELD_FILESYSTEM_ID] = fsid_term(env, &filesystem_id);
    }
  }

  fsobj_type_t object_type = VNON;
  int object_type_returned = 0;
  if (cursor->attributes.commonattr & ATTR_CMN_OBJTYPE) {
    if (!read_field(&reader, &object_type, sizeof(object_type))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_OBJTYPE) {
      object_type_returned = 1;
      if ((cursor->requested_fields & FIELD_BIT(FIELD_TYPE)) != 0) {
        values[FIELD_TYPE] = file_type_term(env, object_type);
      }
    }
  }

  if ((cursor->attributes.dirattr != 0 || cursor->attributes.fileattr != 0) &&
      !object_type_returned) {
    return 0;
  }

  fsobj_tag_t object_tag;
  if (cursor->attributes.commonattr & ATTR_CMN_OBJTAG) {
    if (!read_field(&reader, &object_tag, sizeof(object_tag))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_OBJTAG) {
      values[FIELD_OBJECT_TAG] = enif_make_uint(env, (unsigned int)object_tag);
    }
  }

  struct timespec time_value;
  if (cursor->attributes.commonattr & ATTR_CMN_CRTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_CRTIME) {
      values[FIELD_CREATED_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_MODTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_MODTIME) {
      values[FIELD_MODIFIED_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_CHGTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_CHGTIME) {
      values[FIELD_CHANGED_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_ACCTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_ACCTIME) {
      values[FIELD_ACCESSED_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_BKUPTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_BKUPTIME) {
      values[FIELD_BACKED_UP_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_FNDRINFO) {
    unsigned char finder_info[32];
    if (!read_field(&reader, finder_info, sizeof(finder_info))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_FNDRINFO) {
      values[FIELD_FINDER_INFO] =
          findex_copy_binary(env, finder_info, sizeof(finder_info));
    }
  }

  uid_t owner_id;
  if (cursor->attributes.commonattr & ATTR_CMN_OWNERID) {
    if (!read_field(&reader, &owner_id, sizeof(owner_id))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_OWNERID) {
      values[FIELD_OWNER_ID] = enif_make_uint(env, owner_id);
    }
  }

  gid_t group_id;
  if (cursor->attributes.commonattr & ATTR_CMN_GRPID) {
    if (!read_field(&reader, &group_id, sizeof(group_id))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_GRPID) {
      values[FIELD_GROUP_ID] = enif_make_uint(env, group_id);
    }
  }

  uint32_t unsigned_value;
  if (cursor->attributes.commonattr & ATTR_CMN_ACCESSMASK) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_ACCESSMASK) {
      values[FIELD_MODE] = enif_make_uint(env, unsigned_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_FLAGS) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_FLAGS) {
      values[FIELD_FLAGS] = enif_make_uint(env, unsigned_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_GEN_COUNT) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_GEN_COUNT) {
      values[FIELD_GENERATION_COUNT] = enif_make_uint(env, unsigned_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_DOCUMENT_ID) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_DOCUMENT_ID) {
      values[FIELD_DOCUMENT_ID] = enif_make_uint(env, unsigned_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_USERACCESS) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_USERACCESS) {
      values[FIELD_USER_ACCESS] = enif_make_uint(env, unsigned_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_EXTENDED_SECURITY) {
    reference_position = reader.current;
    if (!read_field(&reader, &reference, sizeof(reference))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_EXTENDED_SECURITY) {
      if (!referenced_binary(env, record_start, record_end, reference_position,
                             &reference, 0, &values[FIELD_ACL])) {
        return 0;
      }
    }
  }

  guid_t guid;
  if (cursor->attributes.commonattr & ATTR_CMN_UUID) {
    if (!read_field(&reader, &guid, sizeof(guid))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_UUID) {
      values[FIELD_OWNER_UUID] = findex_copy_binary(env, &guid, sizeof(guid));
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_GRPUUID) {
    if (!read_field(&reader, &guid, sizeof(guid))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_GRPUUID) {
      values[FIELD_GROUP_UUID] = findex_copy_binary(env, &guid, sizeof(guid));
    }
  }

  uint64_t unsigned_64;
  if (cursor->attributes.commonattr & ATTR_CMN_FILEID) {
    if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_FILEID) {
      values[FIELD_FILE_ID] = enif_make_uint64(env, unsigned_64);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_PARENTID) {
    if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_PARENTID) {
      values[FIELD_PARENT_ID] = enif_make_uint64(env, unsigned_64);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_ADDEDTIME) {
    if (!read_field(&reader, &time_value, sizeof(time_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_ADDEDTIME) {
      values[FIELD_ADDED_AT] = timespec_term(env, &time_value);
    }
  }

  if (cursor->attributes.commonattr & ATTR_CMN_DATA_PROTECT_FLAGS) {
    if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
      return 0;
    }
    if (returned.commonattr & ATTR_CMN_DATA_PROTECT_FLAGS) {
      values[FIELD_DATA_PROTECTION_FLAGS] = enif_make_uint(env, unsigned_value);
    }
  }

  off_t signed_64;
  if (object_type == VDIR) {
    if (cursor->attributes.dirattr & ATTR_DIR_LINKCOUNT) {
      uint32_t dir_link_count;
      if (!read_field(&reader, &dir_link_count, sizeof(dir_link_count))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_LINKCOUNT) {
        values[FIELD_LINK_COUNT] = enif_make_uint(env, dir_link_count);
      }
    }

    if (cursor->attributes.dirattr & ATTR_DIR_ENTRYCOUNT) {
      uint32_t entry_count;
      if (!read_field(&reader, &entry_count, sizeof(entry_count))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_ENTRYCOUNT) {
        values[FIELD_DIRECTORY_ENTRY_COUNT] = enif_make_uint(env, entry_count);
      }
    }

    if (cursor->attributes.dirattr & ATTR_DIR_MOUNTSTATUS) {
      uint32_t mount_status;
      if (!read_field(&reader, &mount_status, sizeof(mount_status))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_MOUNTSTATUS) {
        values[FIELD_MOUNT_STATUS] = enif_make_uint(env, mount_status);
      }
    }

    if (cursor->attributes.dirattr & ATTR_DIR_ALLOCSIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_ALLOCSIZE) {
        values[FIELD_ALLOCATED_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.dirattr & ATTR_DIR_IOBLOCKSIZE) {
      if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_IOBLOCKSIZE) {
        values[FIELD_IO_BLOCK_SIZE] = enif_make_uint(env, unsigned_value);
      }
    }

    if (cursor->attributes.dirattr & ATTR_DIR_DATALENGTH) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.dirattr & ATTR_DIR_DATALENGTH) {
        values[FIELD_DATA_SIZE] = enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }
  } else {
    if (cursor->attributes.fileattr & ATTR_FILE_LINKCOUNT) {
      uint32_t file_link_count;
      if (!read_field(&reader, &file_link_count, sizeof(file_link_count))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_LINKCOUNT) {
        values[FIELD_LINK_COUNT] = enif_make_uint(env, file_link_count);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_TOTALSIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_TOTALSIZE) {
        values[FIELD_TOTAL_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_ALLOCSIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_ALLOCSIZE) {
        values[FIELD_ALLOCATED_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_IOBLOCKSIZE) {
      if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_IOBLOCKSIZE) {
        values[FIELD_IO_BLOCK_SIZE] = enif_make_uint(env, unsigned_value);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_DEVTYPE) {
      if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_DEVTYPE) {
        values[FIELD_DEVICE_TYPE] = enif_make_uint(env, unsigned_value);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_FORKCOUNT) {
      if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_FORKCOUNT) {
        values[FIELD_FORK_COUNT] = enif_make_uint(env, unsigned_value);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_DATALENGTH) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_DATALENGTH) {
        values[FIELD_DATA_SIZE] = enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_DATAALLOCSIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_DATAALLOCSIZE) {
        values[FIELD_DATA_ALLOCATED_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_RSRCLENGTH) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_RSRCLENGTH) {
        values[FIELD_RESOURCE_FORK_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.fileattr & ATTR_FILE_RSRCALLOCSIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.fileattr & ATTR_FILE_RSRCALLOCSIZE) {
        values[FIELD_RESOURCE_FORK_ALLOCATED_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }
  }

  if (cursor->attributes.forkattr != 0) {
    if (cursor->attributes.forkattr & ATTR_CMNEXT_PRIVATESIZE) {
      if (!read_field(&reader, &signed_64, sizeof(signed_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_PRIVATESIZE) {
        values[FIELD_PRIVATE_SIZE] =
            enif_make_int64(env, (ErlNifSInt64)signed_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_LINKID) {
      if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_LINKID) {
        values[FIELD_LINK_ID] = enif_make_uint64(env, unsigned_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_REALDEVID) {
      if (!read_field(&reader, &device, sizeof(device))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_REALDEVID) {
        values[FIELD_REAL_DEVICE] =
            enif_make_uint64(env, (ErlNifUInt64)(uint32_t)device);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_REALFSID) {
      if (!read_field(&reader, &filesystem_id, sizeof(filesystem_id))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_REALFSID) {
        values[FIELD_REAL_FILESYSTEM_ID] = fsid_term(env, &filesystem_id);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_CLONEID) {
      if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_CLONEID) {
        values[FIELD_CLONE_ID] = enif_make_uint64(env, unsigned_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_EXT_FLAGS) {
      if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_EXT_FLAGS) {
        values[FIELD_EXTENDED_FLAGS] = enif_make_uint64(env, unsigned_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_RECURSIVE_GENCOUNT) {
      if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_RECURSIVE_GENCOUNT) {
        values[FIELD_RECURSIVE_GENERATION_COUNT] =
            enif_make_uint64(env, unsigned_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_ATTRIBUTION_TAG) {
      if (!read_field(&reader, &unsigned_64, sizeof(unsigned_64))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_ATTRIBUTION_TAG) {
        values[FIELD_ATTRIBUTION_TAG] = enif_make_uint64(env, unsigned_64);
      }
    }

    if (cursor->attributes.forkattr & ATTR_CMNEXT_CLONE_REFCNT) {
      if (!read_field(&reader, &unsigned_value, sizeof(unsigned_value))) {
        return 0;
      }
      if (returned.forkattr & ATTR_CMNEXT_CLONE_REFCNT) {
        values[FIELD_CLONE_REFERENCE_COUNT] =
            enif_make_uint(env, unsigned_value);
      }
    }
  }

  ERL_NIF_TERM returned_values[] = {
      enif_make_uint(env, returned.commonattr),
      enif_make_uint(env, returned.dirattr),
      enif_make_uint(env, returned.fileattr),
      enif_make_uint(env, returned.forkattr),
  };
  values[FIELD_RETURNED_ATTRIBUTES] =
      enif_make_tuple_from_array(env, returned_values, 4);

  if (!enif_make_map_from_arrays(env, keys, values, ENTRY_FIELD_COUNT,
                                 result)) {
    return 0;
  }

  return 1;
}

static void close_cursor(directory_cursor_t *cursor) {
  if (cursor->fd >= 0) {
    close(cursor->fd);
    cursor->fd = -1;
  }
  cursor->exhausted = 1;
}

int findex_open_directory_path(const ErlNifBinary *path, int path_policy,
                               int *error_number) {
  if (memchr(path->data, '\0', path->size) != NULL) {
    *error_number = EINVAL;
    return -1;
  }

  if (path->size == SIZE_MAX) {
    *error_number = ENAMETOOLONG;
    return -1;
  }

  char *path_string = enif_alloc(path->size + 1U);
  if (path_string == NULL) {
    *error_number = ENOMEM;
    return -1;
  }
  memcpy(path_string, path->data, path->size);
  path_string[path->size] = '\0';

  int fd;
  int flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC;
  if (path_policy == PATH_REJECT_ANY_SYMLINK) {
    flags |= O_NOFOLLOW_ANY;
  }
  do {
    fd = open(path_string, flags);
  } while (fd < 0 && errno == EINTR);
  *error_number = fd < 0 ? errno : 0;
  enif_free(path_string);
  return fd;
}

int findex_open_directory_component(int parent_fd,
                                    const path_component_t *component,
                                    int *error_number) {
  if (!findex_valid_path_component(component->data, component->size)) {
    *error_number = EINVAL;
    return -1;
  }

  char *name = enif_alloc(component->size + 1U);
  if (name == NULL) {
    *error_number = ENOMEM;
    return -1;
  }
  memcpy(name, component->data, component->size);
  name[component->size] = '\0';

  int fd;
  do {
    fd = openat(parent_fd, name,
                O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
  } while (fd < 0 && errno == EINTR);
  *error_number = fd < 0 ? errno : 0;
  enif_free(name);
  return fd;
}

ERL_NIF_TERM findex_make_directory_cursor(ErlNifEnv *env, int fd,
                                          ERL_NIF_TERM fields,
                                          int output_format) {
  directory_cursor_t *cursor =
      enif_alloc_resource(directory_cursor_type, sizeof(*cursor));
  if (cursor == NULL) {
    close(fd);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  memset(cursor, 0, sizeof(*cursor));
  cursor->fd = fd;
  cursor->output_format = output_format;
  cursor->lock = enif_mutex_create("findex_directory_cursor");

  if (cursor->lock == NULL) {
    close_cursor(cursor);
    enif_release_resource(cursor);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  if (!configure_attributes(env, cursor, fields)) {
    close_cursor(cursor);
    enif_mutex_destroy(cursor->lock);
    cursor->lock = NULL;
    enif_release_resource(cursor);
    return findex_error_tuple(env, findex_atom(env, "invalid_fields"));
  }

  ERL_NIF_TERM resource = enif_make_resource(env, cursor);
  enif_release_resource(cursor);
  return findex_ok_tuple(env, resource);
}

/* Resource lifecycle and exported NIF entry points. */

static void directory_cursor_destructor(ErlNifEnv *env, void *object) {
  (void)env;
  directory_cursor_t *cursor = object;

  if (cursor->lock != NULL) {
    enif_mutex_lock(cursor->lock);
    close_cursor(cursor);
    enif_mutex_unlock(cursor->lock);
    enif_mutex_destroy(cursor->lock);
    cursor->lock = NULL;
  } else {
    close_cursor(cursor);
  }
}

int findex_directory_resource_init(ErlNifEnv *env) {
  directory_cursor_type = enif_open_resource_type(
      env, NULL, "findex_directory_cursor", directory_cursor_destructor,
      ERL_NIF_RT_CREATE, NULL);
  return directory_cursor_type != NULL;
}

ERL_NIF_TERM findex_nif_open_directory(ErlNifEnv *env, int argc,
                                       const ERL_NIF_TERM argv[]) {
  ErlNifBinary path;
  int output_format;
  int path_policy;

  if (argc != 4 || !enif_inspect_iolist_as_binary(env, argv[0], &path) ||
      !enif_is_list(env, argv[1]) ||
      !enif_get_int(env, argv[2], &output_format) ||
      (output_format != OUTPUT_ENTRIES && output_format != OUTPUT_PACKED) ||
      !enif_get_int(env, argv[3], &path_policy) ||
      (path_policy != PATH_FOLLOW_SYMLINKS &&
       path_policy != PATH_REJECT_ANY_SYMLINK)) {
    return enif_make_badarg(env);
  }

  int open_error;
  int fd = findex_open_directory_path(&path, path_policy, &open_error);
  if (fd < 0) {
    return findex_error_tuple(env, findex_errno_term(env, open_error));
  }
  return findex_make_directory_cursor(env, fd, argv[1], output_format);
}

ERL_NIF_TERM findex_nif_next_directory_batch(ErlNifEnv *env, int argc,
                                             const ERL_NIF_TERM argv[]) {
  directory_cursor_t *cursor;
  unsigned int buffer_size;

  if (argc != 2 ||
      !enif_get_resource(env, argv[0], directory_cursor_type,
                         (void **)&cursor) ||
      !enif_get_uint(env, argv[1], &buffer_size) ||
      buffer_size < MIN_BUFFER_SIZE || buffer_size > MAX_BUFFER_SIZE) {
    return enif_make_badarg(env);
  }

  unsigned char *buffer = enif_alloc(buffer_size);
  if (buffer == NULL) {
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  enif_mutex_lock(cursor->lock);
  if (cursor->fd < 0) {
    enif_mutex_unlock(cursor->lock);
    enif_free(buffer);
    return findex_error_tuple(env, findex_atom(env, "closed"));
  }
  if (cursor->exhausted) {
    enif_mutex_unlock(cursor->lock);
    enif_free(buffer);
    return findex_atom(env, "done");
  }

  int count;
  do {
    count = getattrlistbulk(cursor->fd, &cursor->attributes, buffer,
                            buffer_size, cursor->options);
  } while (count < 0 && errno == EINTR);
  int call_error = errno;

  if (count == 0) {
    cursor->exhausted = 1;
  }
  enif_mutex_unlock(cursor->lock);

  if (count < 0) {
    enif_free(buffer);
    return findex_error_tuple(env, findex_errno_term(env, call_error));
  }
  if (count == 0) {
    enif_free(buffer);
    return findex_atom(env, "done");
  }
  if ((size_t)count > (size_t)buffer_size / MIN_BULK_RECORD_SIZE) {
    enif_free(buffer);
    return findex_error_tuple(env, findex_atom(env, "invalid_native_record"));
  }

  if (cursor->output_format == OUTPUT_PACKED) {
    packed_column_t columns[ENTRY_FIELD_COUNT];
    if (!initialize_packed_columns(env, cursor, (size_t)count, columns)) {
      enif_free(buffer);
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }

    const unsigned char *record = buffer;
    const unsigned char *buffer_end = buffer + buffer_size;

    for (int index = 0; index < count; index++) {
      if ((size_t)(buffer_end - record) < sizeof(uint32_t)) {
        enif_free(buffer);
        return findex_error_tuple(env,
                                  findex_atom(env, "invalid_native_record"));
      }

      uint32_t record_size;
      memcpy(&record_size, record, sizeof(record_size));
      if (record_size < sizeof(uint32_t) ||
          (size_t)record_size > (size_t)(buffer_end - record)) {
        enif_free(buffer);
        return findex_error_tuple(env,
                                  findex_atom(env, "invalid_native_record"));
      }

      const unsigned char *record_end = record + record_size;
      if (!parse_packed_entry(cursor, buffer, record, record_end, (size_t)index,
                              columns)) {
        enif_free(buffer);
        return findex_error_tuple(env,
                                  findex_atom(env, "invalid_native_record"));
      }
      record = record_end;
    }

    ERL_NIF_TERM storage =
        findex_copy_binary(env, buffer, (size_t)(record - buffer));
    ERL_NIF_TERM batch;
    if (!make_packed_batch(env, (size_t)count, storage, columns, &batch)) {
      enif_free(buffer);
      return findex_error_tuple(env, findex_atom(env, "enomem"));
    }

    enif_free(buffer);
    return findex_ok_tuple(env, batch);
  }

  ERL_NIF_TERM keys[ENTRY_FIELD_COUNT];
  for (size_t index = 0; index < ENTRY_FIELD_COUNT; index++) {
    keys[index] = findex_atom(env, findex_entry_field_names[index]);
  }

  size_t entries_size;
  if (!findex_checked_multiply_size(sizeof(ERL_NIF_TERM), (size_t)count,
                                    &entries_size)) {
    enif_free(buffer);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  ERL_NIF_TERM *entries = enif_alloc(entries_size);
  if (entries == NULL) {
    enif_free(buffer);
    return findex_error_tuple(env, findex_atom(env, "enomem"));
  }

  const unsigned char *record = buffer;
  const unsigned char *buffer_end = buffer + buffer_size;

  for (int index = 0; index < count; index++) {
    if ((size_t)(buffer_end - record) < sizeof(uint32_t)) {
      enif_free(entries);
      enif_free(buffer);
      return findex_error_tuple(env, findex_atom(env, "invalid_native_record"));
    }

    uint32_t record_size;
    memcpy(&record_size, record, sizeof(record_size));
    if (record_size < sizeof(uint32_t) ||
        (size_t)record_size > (size_t)(buffer_end - record)) {
      enif_free(entries);
      enif_free(buffer);
      return findex_error_tuple(env, findex_atom(env, "invalid_native_record"));
    }

    const unsigned char *record_end = record + record_size;
    if (!parse_entry(env, cursor, record, record_end, keys, &entries[index])) {
      enif_free(entries);
      enif_free(buffer);
      return findex_error_tuple(env, findex_atom(env, "invalid_native_record"));
    }
    record = record_end;
  }

  ERL_NIF_TERM list = enif_make_list(env, 0);
  for (int index = count - 1; index >= 0; index--) {
    list = enif_make_list_cell(env, entries[index], list);
  }

  enif_free(entries);
  enif_free(buffer);
  return findex_ok_tuple(env, list);
}

ERL_NIF_TERM findex_nif_close_directory(ErlNifEnv *env, int argc,
                                        const ERL_NIF_TERM argv[]) {
  directory_cursor_t *cursor;

  if (argc != 1 || !enif_get_resource(env, argv[0], directory_cursor_type,
                                      (void **)&cursor)) {
    return enif_make_badarg(env);
  }

  enif_mutex_lock(cursor->lock);
  close_cursor(cursor);
  enif_mutex_unlock(cursor->lock);
  return findex_atom(env, "ok");
}
