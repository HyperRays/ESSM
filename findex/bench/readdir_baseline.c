#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

typedef struct {
  uint64_t entries;
  uint64_t directories;
  uint64_t regular_files;
  uint64_t symlinks;
  uint64_t other;
  uint64_t directory_errors;
  uint64_t metadata_errors;
} scan_stats_t;

static unsigned char type_from_mode(mode_t mode) {
  if (S_ISDIR(mode)) {
    return DT_DIR;
  }
  if (S_ISREG(mode)) {
    return DT_REG;
  }
  if (S_ISLNK(mode)) {
    return DT_LNK;
  }

  return DT_UNKNOWN;
}

static unsigned char entry_type(DIR *directory, const struct dirent *entry,
                                scan_stats_t *stats) {
  if (entry->d_type != DT_UNKNOWN) {
    return entry->d_type;
  }

  struct stat metadata;
  if (fstatat(dirfd(directory), entry->d_name, &metadata,
              AT_SYMLINK_NOFOLLOW) != 0) {
    stats->metadata_errors++;
    return DT_UNKNOWN;
  }

  return type_from_mode(metadata.st_mode);
}

static void scan_directory_fd(int fd, scan_stats_t *stats) {
  DIR *directory = fdopendir(fd);
  if (directory == NULL) {
    close(fd);
    stats->directory_errors++;
    return;
  }

  for (;;) {
    errno = 0;
    struct dirent *entry = readdir(directory);
    if (entry == NULL) {
      if (errno != 0) {
        stats->directory_errors++;
      }
      break;
    }

    if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
      continue;
    }

    stats->entries++;
    unsigned char type = entry_type(directory, entry, stats);

    switch (type) {
    case DT_DIR: {
      stats->directories++;
      int child_fd = openat(dirfd(directory), entry->d_name,
                            O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
      if (child_fd < 0) {
        stats->directory_errors++;
      } else {
        scan_directory_fd(child_fd, stats);
      }
      break;
    }
    case DT_REG:
      stats->regular_files++;
      break;
    case DT_LNK:
      stats->symlinks++;
      break;
    default:
      stats->other++;
      break;
    }
  }

  closedir(directory);
}

static double elapsed_milliseconds(const struct timespec *start,
                                   const struct timespec *finish) {
  time_t seconds = finish->tv_sec - start->tv_sec;
  long nanoseconds = finish->tv_nsec - start->tv_nsec;

  if (nanoseconds < 0) {
    seconds--;
    nanoseconds += 1000000000L;
  }

  return (double)seconds * 1000.0 + (double)nanoseconds / 1000000.0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s DIRECTORY\n", argv[0]);
    return 64;
  }

  scan_stats_t stats = {0};
  struct timespec start;
  struct timespec finish;

  if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
    perror("clock_gettime");
    return 1;
  }

  int root_fd =
      open(argv[1], O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
  if (root_fd < 0) {
    perror(argv[1]);
    return 1;
  }

  scan_directory_fd(root_fd, &stats);

  if (clock_gettime(CLOCK_MONOTONIC, &finish) != 0) {
    perror("clock_gettime");
    return 1;
  }

  double elapsed_ms = elapsed_milliseconds(&start, &finish);
  double throughput =
      elapsed_ms > 0.0 ? (double)stats.entries / elapsed_ms * 1000.0 : 0.0;

  printf("RESULT mode=readdir elapsed_ms=%.3f entries_per_second=%.1f "
         "entries=%" PRIu64 " directories=%" PRIu64
         " regular_files=%" PRIu64 " symlinks=%" PRIu64
         " other=%" PRIu64 " directory_errors=%" PRIu64
         " metadata_errors=%" PRIu64 "\n",
         elapsed_ms, throughput, stats.entries, stats.directories,
         stats.regular_files, stats.symlinks, stats.other,
         stats.directory_errors, stats.metadata_errors);

  return 0;
}
