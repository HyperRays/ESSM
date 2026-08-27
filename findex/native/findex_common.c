#include "findex_nif.h"

#include <errno.h>
#include <string.h>

_Static_assert(ENTRY_FIELD_COUNT <= 64,
               "requested field bitmap requires at most 64 fields");

const char *const findex_entry_field_names[ENTRY_FIELD_COUNT] = {
    "__struct__",
    "name",
    "type",
    "object_tag",
    "error",
    "device",
    "filesystem_id",
    "file_id",
    "parent_id",
    "created_at",
    "modified_at",
    "changed_at",
    "accessed_at",
    "backed_up_at",
    "added_at",
    "owner_id",
    "group_id",
    "mode",
    "flags",
    "user_access",
    "finder_info",
    "owner_uuid",
    "group_uuid",
    "acl",
    "data_protection_flags",
    "generation_count",
    "document_id",
    "link_count",
    "total_size",
    "allocated_size",
    "io_block_size",
    "device_type",
    "fork_count",
    "data_size",
    "data_allocated_size",
    "resource_fork_size",
    "resource_fork_allocated_size",
    "directory_entry_count",
    "mount_status",
    "private_size",
    "link_id",
    "real_device",
    "real_filesystem_id",
    "clone_id",
    "extended_flags",
    "recursive_generation_count",
    "attribution_tag",
    "clone_reference_count",
    "returned_attributes",
};

ERL_NIF_TERM findex_atom(ErlNifEnv *env, const char *name) {
  return enif_make_atom(env, name);
}

ERL_NIF_TERM findex_ok_tuple(ErlNifEnv *env, ERL_NIF_TERM value) {
  return enif_make_tuple2(env, findex_atom(env, "ok"), value);
}

ERL_NIF_TERM findex_error_tuple(ErlNifEnv *env, ERL_NIF_TERM reason) {
  return enif_make_tuple2(env, findex_atom(env, "error"), reason);
}

ERL_NIF_TERM findex_errno_term(ErlNifEnv *env, int error_number) {
  switch (error_number) {
  case EPERM:
    return findex_atom(env, "eperm");
  case EACCES:
    return findex_atom(env, "eacces");
  case EAGAIN:
    return findex_atom(env, "eagain");
  case EBADF:
    return findex_atom(env, "ebadf");
  case EBUSY:
    return findex_atom(env, "ebusy");
  case ECANCELED:
    return findex_atom(env, "ecanceled");
  case EDEADLK:
    return findex_atom(env, "edeadlk");
  case EFAULT:
    return findex_atom(env, "efault");
  case EILSEQ:
    return findex_atom(env, "eilseq");
  case EINTR:
    return findex_atom(env, "eintr");
  case EINVAL:
    return findex_atom(env, "einval");
  case EIO:
    return findex_atom(env, "eio");
  case ELOOP:
    return findex_atom(env, "eloop");
  case EMFILE:
    return findex_atom(env, "emfile");
  case ENAMETOOLONG:
    return findex_atom(env, "enametoolong");
  case ENFILE:
    return findex_atom(env, "enfile");
  case ENOENT:
    return findex_atom(env, "enoent");
  case ENOMEM:
    return findex_atom(env, "enomem");
  case ENOSPC:
    return findex_atom(env, "enospc");
  case ENOTDIR:
    return findex_atom(env, "enotdir");
  case ENXIO:
    return findex_atom(env, "enxio");
  case EOPNOTSUPP:
    return findex_atom(env, "enotsup");
  case EOVERFLOW:
    return findex_atom(env, "eoverflow");
  case ERANGE:
    return findex_atom(env, "erange");
  case EROFS:
    return findex_atom(env, "erofs");
  case ESTALE:
    return findex_atom(env, "estale");
  default:
    return enif_make_int(env, error_number);
  }
}

int findex_errno_from_term(ErlNifEnv *env, ERL_NIF_TERM term,
                           int *error_number) {
  int numeric_error;
  if (enif_get_int(env, term, &numeric_error) && numeric_error > 0) {
    *error_number = numeric_error;
    return 1;
  }

#define MATCH_ERRNO(name, value)                                               \
  if (enif_is_identical(term, findex_atom(env, (name)))) {                     \
    *error_number = (value);                                                   \
    return 1;                                                                  \
  }

  MATCH_ERRNO("eperm", EPERM)
  MATCH_ERRNO("eacces", EACCES)
  MATCH_ERRNO("eagain", EAGAIN)
  MATCH_ERRNO("ebadf", EBADF)
  MATCH_ERRNO("ebusy", EBUSY)
  MATCH_ERRNO("ecanceled", ECANCELED)
  MATCH_ERRNO("edeadlk", EDEADLK)
  MATCH_ERRNO("efault", EFAULT)
  MATCH_ERRNO("eilseq", EILSEQ)
  MATCH_ERRNO("eintr", EINTR)
  MATCH_ERRNO("einval", EINVAL)
  MATCH_ERRNO("eio", EIO)
  MATCH_ERRNO("eloop", ELOOP)
  MATCH_ERRNO("emfile", EMFILE)
  MATCH_ERRNO("enametoolong", ENAMETOOLONG)
  MATCH_ERRNO("enfile", ENFILE)
  MATCH_ERRNO("enoent", ENOENT)
  MATCH_ERRNO("enomem", ENOMEM)
  MATCH_ERRNO("enospc", ENOSPC)
  MATCH_ERRNO("enotdir", ENOTDIR)
  MATCH_ERRNO("enotsup", EOPNOTSUPP)
  MATCH_ERRNO("enxio", ENXIO)
  MATCH_ERRNO("eoverflow", EOVERFLOW)
  MATCH_ERRNO("erange", ERANGE)
  MATCH_ERRNO("erofs", EROFS)
  MATCH_ERRNO("estale", ESTALE)

#undef MATCH_ERRNO

  return 0;
}

ERL_NIF_TERM findex_copy_binary(ErlNifEnv *env, const void *data, size_t size) {
  ERL_NIF_TERM term;
  unsigned char *destination = enif_make_new_binary(env, size, &term);

  if (size > 0) {
    memcpy(destination, data, size);
  }

  return term;
}

int findex_valid_path_component(const unsigned char *data, size_t size) {
  return size > 0 && size < SIZE_MAX && memchr(data, '\0', size) == NULL &&
         memchr(data, '/', size) == NULL && !(size == 1 && data[0] == '.') &&
         !(size == 2 && data[0] == '.' && data[1] == '.');
}

int findex_entry_field_from_term(ErlNifEnv *env, ERL_NIF_TERM term,
                                 enum entry_field *field) {
  for (size_t index = FIELD_NAME; index < ENTRY_FIELD_COUNT; index++) {
    if (enif_is_identical(term,
                          findex_atom(env, findex_entry_field_names[index]))) {
      *field = (enum entry_field)index;
      return 1;
    }
  }

  return 0;
}

size_t findex_packed_field_width(enum entry_field field) {
  switch (field) {
  case FIELD_NAME:
  case FIELD_ACL:
    return 8;
  case FIELD_TYPE:
    return 1;
  case FIELD_ERROR:
  case FIELD_OBJECT_TAG:
  case FIELD_OWNER_ID:
  case FIELD_GROUP_ID:
  case FIELD_MODE:
  case FIELD_FLAGS:
  case FIELD_USER_ACCESS:
  case FIELD_DATA_PROTECTION_FLAGS:
  case FIELD_GENERATION_COUNT:
  case FIELD_DOCUMENT_ID:
  case FIELD_LINK_COUNT:
  case FIELD_IO_BLOCK_SIZE:
  case FIELD_DEVICE_TYPE:
  case FIELD_DIRECTORY_ENTRY_COUNT:
  case FIELD_MOUNT_STATUS:
  case FIELD_CLONE_REFERENCE_COUNT:
    return 4;
  case FIELD_DEVICE:
  case FIELD_FILESYSTEM_ID:
  case FIELD_FILE_ID:
  case FIELD_PARENT_ID:
  case FIELD_TOTAL_SIZE:
  case FIELD_ALLOCATED_SIZE:
  case FIELD_DATA_SIZE:
  case FIELD_DATA_ALLOCATED_SIZE:
  case FIELD_RESOURCE_FORK_SIZE:
  case FIELD_RESOURCE_FORK_ALLOCATED_SIZE:
  case FIELD_PRIVATE_SIZE:
  case FIELD_LINK_ID:
  case FIELD_REAL_DEVICE:
  case FIELD_REAL_FILESYSTEM_ID:
  case FIELD_CLONE_ID:
  case FIELD_EXTENDED_FLAGS:
  case FIELD_RECURSIVE_GENERATION_COUNT:
  case FIELD_ATTRIBUTION_TAG:
    return 8;
  case FIELD_CREATED_AT:
  case FIELD_MODIFIED_AT:
  case FIELD_CHANGED_AT:
  case FIELD_ACCESSED_AT:
  case FIELD_BACKED_UP_AT:
  case FIELD_ADDED_AT:
  case FIELD_OWNER_UUID:
  case FIELD_GROUP_UUID:
  case FIELD_RETURNED_ATTRIBUTES:
    return 16;
  case FIELD_FINDER_INFO:
    return 32;
  case FIELD_STRUCT:
  case FIELD_FORK_COUNT:
  case ENTRY_FIELD_COUNT:
    return 0;
  }

  return 0;
}

int findex_checked_add_size(size_t left, size_t right, size_t *result) {
  if (right > SIZE_MAX - left) {
    return 0;
  }

  *result = left + right;
  return 1;
}

int findex_checked_multiply_size(size_t left, size_t right, size_t *result) {
  if (left != 0 && right > SIZE_MAX / left) {
    return 0;
  }

  *result = left * right;
  return 1;
}
