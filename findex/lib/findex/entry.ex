defmodule Findex.Entry do
  @moduledoc """
  Metadata returned for one immediate child of an enumerated directory.

  Times are represented as `{unix_seconds, nanoseconds}` tuples. Fields are
  `nil` when the selected field set or underlying filesystem did not return
  the corresponding attribute. `error` is a POSIX reason for an error
  affecting only this entry.

  `returned_attributes` contains the raw macOS attribute masks as
  `{common, directory, file, extended}`.
  """

  defstruct [
    :name,
    :type,
    :object_tag,
    :error,
    :device,
    :filesystem_id,
    :file_id,
    :parent_id,
    :created_at,
    :modified_at,
    :changed_at,
    :accessed_at,
    :backed_up_at,
    :added_at,
    :owner_id,
    :group_id,
    :mode,
    :flags,
    :user_access,
    :finder_info,
    :owner_uuid,
    :group_uuid,
    :acl,
    :data_protection_flags,
    :generation_count,
    :document_id,
    :link_count,
    :total_size,
    :allocated_size,
    :io_block_size,
    :device_type,
    :fork_count,
    :data_size,
    :data_allocated_size,
    :resource_fork_size,
    :resource_fork_allocated_size,
    :directory_entry_count,
    :mount_status,
    :private_size,
    :link_id,
    :real_device,
    :real_filesystem_id,
    :clone_id,
    :extended_flags,
    :recursive_generation_count,
    :attribution_tag,
    :clone_reference_count,
    :returned_attributes
  ]

  @type timestamp :: {integer(), non_neg_integer()}
  @type file_type ::
          :regular
          | :directory
          | :symlink
          | :block_device
          | :character_device
          | :socket
          | :fifo
          | :unknown

  @type t :: %__MODULE__{
          name: binary(),
          type: file_type() | nil,
          error: atom() | integer() | nil,
          created_at: timestamp() | nil,
          modified_at: timestamp() | nil,
          file_id: non_neg_integer() | nil,
          total_size: integer() | nil,
          returned_attributes:
            {non_neg_integer(), non_neg_integer(), non_neg_integer(), non_neg_integer()}
        }
end
