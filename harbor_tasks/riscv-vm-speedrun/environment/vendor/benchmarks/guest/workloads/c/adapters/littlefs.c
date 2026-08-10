/* Project-owned adapter around the pinned upstream littlefs workload. */

#include "rvb_workload_common.h"
#include "rvb_c_workloads.h"

#include "lfs.h"

#include <stddef.h>
#include <stdint.h>
#include <string.h>

/* littlefs in-memory flash trace                                             */

#define RVB_LFS_BLOCK_SIZE 256u
#define RVB_LFS_BLOCK_COUNT 128u
#define RVB_LFS_CACHE_SIZE 64u
#define RVB_LFS_LOOKAHEAD_SIZE 16u
#define RVB_LFS_FILE_COUNT 16u
#define RVB_LFS_MAX_OPERATIONS 96u

static uint8_t rvb_lfs_flash[RVB_LFS_BLOCK_SIZE * RVB_LFS_BLOCK_COUNT];
static uint8_t rvb_lfs_read_cache[RVB_LFS_CACHE_SIZE];
static uint8_t rvb_lfs_prog_cache[RVB_LFS_CACHE_SIZE];
static uint8_t rvb_lfs_lookahead[RVB_LFS_LOOKAHEAD_SIZE];
static uint8_t rvb_lfs_file_cache[RVB_LFS_CACHE_SIZE];
static uint8_t rvb_lfs_io[64u];
static const struct lfs_file_config rvb_lfs_file_configuration = {
    .buffer = rvb_lfs_file_cache,
    .attrs = NULL,
    .attr_count = 0u,
};

static int rvb_lfs_read(const struct lfs_config *configuration,
                        lfs_block_t block, lfs_off_t offset, void *buffer,
                        lfs_size_t size) {
    (void)configuration;
    if (block >= RVB_LFS_BLOCK_COUNT || offset > RVB_LFS_BLOCK_SIZE ||
        size > RVB_LFS_BLOCK_SIZE - offset) {
        return LFS_ERR_IO;
    }
    memcpy(buffer,
           &rvb_lfs_flash[block * RVB_LFS_BLOCK_SIZE + (uint32_t)offset],
           size);
    return 0;
}

static int rvb_lfs_prog(const struct lfs_config *configuration,
                        lfs_block_t block, lfs_off_t offset,
                        const void *buffer, lfs_size_t size) {
    (void)configuration;
    if (block >= RVB_LFS_BLOCK_COUNT || offset > RVB_LFS_BLOCK_SIZE ||
        size > RVB_LFS_BLOCK_SIZE - offset) {
        return LFS_ERR_IO;
    }
    const uint8_t *source = buffer;
    uint8_t *destination =
        &rvb_lfs_flash[block * RVB_LFS_BLOCK_SIZE + (uint32_t)offset];
    for (uint32_t i = 0; i < size; ++i) {
        if ((destination[i] & source[i]) != source[i]) {
            return LFS_ERR_CORRUPT;
        }
        destination[i] &= source[i];
    }
    return 0;
}

static int rvb_lfs_erase(const struct lfs_config *configuration,
                         lfs_block_t block) {
    (void)configuration;
    if (block >= RVB_LFS_BLOCK_COUNT) {
        return LFS_ERR_IO;
    }
    memset(&rvb_lfs_flash[block * RVB_LFS_BLOCK_SIZE], 0xff,
           RVB_LFS_BLOCK_SIZE);
    return 0;
}

static int rvb_lfs_sync(const struct lfs_config *configuration) {
    (void)configuration;
    return 0;
}

static void rvb_lfs_name(uint32_t file_id, char name[4]) {
    static const char hex[] = "0123456789abcdef";
    name[0] = 'f';
    name[1] = hex[(file_id >> 4) & 15u];
    name[2] = hex[file_id & 15u];
    name[3] = '\0';
}

static uint32_t rvb_lfs_error(int error) {
    if (error >= 0) {
        return RVB_INTERNAL_ERROR;
    }
    return 0x10000u | ((uint32_t)(-error) & 0xffffu);
}

static int rvb_lfs_open(lfs_t *filesystem, lfs_file_t *file,
                        const char *name, int flags) {
    memset(file, 0, sizeof(*file));
    memset(rvb_lfs_file_cache, 0, sizeof(rvb_lfs_file_cache));
    return lfs_file_opencfg(filesystem, file, name, flags,
                            &rvb_lfs_file_configuration);
}

static uint8_t rvb_lcg_byte(uint32_t *state) {
    *state = *state * 1664525u + 1013904223u;
    return (uint8_t)(*state >> 24);
}

static uint32_t rvb_lfs_write_operation(lfs_t *filesystem, uint32_t kind,
                                        uint32_t file_id, uint32_t length,
                                        uint32_t seed, uint32_t *event) {
    char name[4];
    rvb_lfs_name(file_id, name);
    lfs_file_t file;
    int flags = LFS_O_WRONLY | LFS_O_CREAT;
    flags |= kind == 0u ? LFS_O_TRUNC : LFS_O_APPEND;
    int error = rvb_lfs_open(filesystem, &file, name, flags);
    if (error < 0) {
        return rvb_lfs_error(error);
    }

    uint32_t state = seed;
    uint32_t remaining = length;
    uint32_t crc = 0xffffffffu;
    while (remaining != 0u) {
        uint32_t chunk = remaining;
        if (chunk > sizeof(rvb_lfs_io)) {
            chunk = sizeof(rvb_lfs_io);
        }
        for (uint32_t i = 0; i < chunk; ++i) {
            rvb_lfs_io[i] = rvb_lcg_byte(&state);
        }
        crc = rvb_crc32_update(crc, rvb_lfs_io, chunk);
        const lfs_ssize_t written =
            lfs_file_write(filesystem, &file, rvb_lfs_io, chunk);
        if (written != (lfs_ssize_t)chunk) {
            (void)lfs_file_close(filesystem, &file);
            return written < 0 ? rvb_lfs_error((int)written)
                               : RVB_INTERNAL_ERROR;
        }
        remaining -= chunk;
    }
    error = lfs_file_close(filesystem, &file);
    if (error < 0) {
        return rvb_lfs_error(error);
    }
    *event = (kind << 28) ^ (file_id << 24) ^ length ^
             rvb_rotate_left(rvb_crc32_finish(crc), 1u);
    return 0u;
}

static uint32_t rvb_lfs_read_operation(lfs_t *filesystem, uint32_t file_id,
                                       uint32_t offset_word,
                                       uint32_t length_word,
                                       uint32_t *event) {
    char name[4];
    rvb_lfs_name(file_id, name);
    lfs_file_t file;
    int error = rvb_lfs_open(filesystem, &file, name, LFS_O_RDONLY);
    if (error == LFS_ERR_NOENT) {
        *event = (2u << 28) ^ (file_id << 24) ^ 0xffffffffu;
        return 0u;
    }
    if (error < 0) {
        return rvb_lfs_error(error);
    }
    const lfs_soff_t signed_size = lfs_file_size(filesystem, &file);
    if (signed_size < 0) {
        (void)lfs_file_close(filesystem, &file);
        return rvb_lfs_error((int)signed_size);
    }
    const uint32_t size = (uint32_t)signed_size;
    const uint32_t offset = offset_word % (size + 1u);
    const lfs_soff_t seek =
        lfs_file_seek(filesystem, &file, (lfs_soff_t)offset, LFS_SEEK_SET);
    if (seek < 0) {
        (void)lfs_file_close(filesystem, &file);
        return rvb_lfs_error((int)seek);
    }
    uint32_t remaining = 1u + (length_word & 63u);
    uint32_t actual = 0u;
    uint32_t crc = 0xffffffffu;
    while (remaining != 0u) {
        uint32_t chunk = remaining;
        if (chunk > sizeof(rvb_lfs_io)) {
            chunk = sizeof(rvb_lfs_io);
        }
        const lfs_ssize_t read =
            lfs_file_read(filesystem, &file, rvb_lfs_io, chunk);
        if (read < 0) {
            (void)lfs_file_close(filesystem, &file);
            return rvb_lfs_error((int)read);
        }
        if (read == 0) {
            break;
        }
        crc = rvb_crc32_update(crc, rvb_lfs_io, (uint32_t)read);
        actual += (uint32_t)read;
        remaining -= (uint32_t)read;
    }
    error = lfs_file_close(filesystem, &file);
    if (error < 0) {
        return rvb_lfs_error(error);
    }
    *event = (2u << 28) ^ (file_id << 24) ^ offset ^ actual ^
             rvb_rotate_left(rvb_crc32_finish(crc), 1u);
    return 0u;
}

static uint32_t rvb_lfs_state_digest(lfs_t *filesystem, uint32_t *digest) {
    uint32_t crc = 0xffffffffu;
    for (uint32_t file_id = 0; file_id < RVB_LFS_FILE_COUNT; ++file_id) {
        char name[4];
        rvb_lfs_name(file_id, name);
        struct lfs_info info;
        int error = lfs_stat(filesystem, name, &info);
        if (error == LFS_ERR_NOENT) {
            continue;
        }
        if (error < 0) {
            return rvb_lfs_error(error);
        }
        if (info.type != LFS_TYPE_REG) {
            return RVB_INTERNAL_ERROR;
        }
        const uint8_t id_byte = (uint8_t)file_id;
        uint8_t size_bytes[4] = {
            (uint8_t)info.size,
            (uint8_t)(info.size >> 8),
            (uint8_t)(info.size >> 16),
            (uint8_t)(info.size >> 24),
        };
        crc = rvb_crc32_update(crc, &id_byte, 1u);
        crc = rvb_crc32_update(crc, size_bytes, sizeof(size_bytes));

        lfs_file_t file;
        error = rvb_lfs_open(filesystem, &file, name, LFS_O_RDONLY);
        if (error < 0) {
            return rvb_lfs_error(error);
        }
        for (;;) {
            const lfs_ssize_t read = lfs_file_read(
                filesystem, &file, rvb_lfs_io, sizeof(rvb_lfs_io));
            if (read < 0) {
                (void)lfs_file_close(filesystem, &file);
                return rvb_lfs_error((int)read);
            }
            if (read == 0) {
                break;
            }
            crc = rvb_crc32_update(crc, rvb_lfs_io, (uint32_t)read);
        }
        error = lfs_file_close(filesystem, &file);
        if (error < 0) {
            return rvb_lfs_error(error);
        }
    }
    *digest = rvb_crc32_finish(crc);
    return 0u;
}

static uint32_t rvb_littlefs_once(const uint8_t *operations,
                                  uint32_t operation_count,
                                  uint32_t out[2]) {
    memset(rvb_lfs_flash, 0xff, sizeof(rvb_lfs_flash));
    memset(rvb_lfs_read_cache, 0, sizeof(rvb_lfs_read_cache));
    memset(rvb_lfs_prog_cache, 0, sizeof(rvb_lfs_prog_cache));
    memset(rvb_lfs_lookahead, 0, sizeof(rvb_lfs_lookahead));

    const struct lfs_config configuration = {
        .context = NULL,
        .read = rvb_lfs_read,
        .prog = rvb_lfs_prog,
        .erase = rvb_lfs_erase,
        .sync = rvb_lfs_sync,
        .read_size = 16u,
        .prog_size = 16u,
        .block_size = RVB_LFS_BLOCK_SIZE,
        .block_count = RVB_LFS_BLOCK_COUNT,
        .block_cycles = 100,
        .cache_size = RVB_LFS_CACHE_SIZE,
        .lookahead_size = RVB_LFS_LOOKAHEAD_SIZE,
        .compact_thresh = 0u,
        .read_buffer = rvb_lfs_read_cache,
        .prog_buffer = rvb_lfs_prog_cache,
        .lookahead_buffer = rvb_lfs_lookahead,
        .name_max = 8u,
        .file_max = 8192u,
        .attr_max = 0u,
        .metadata_max = 0u,
        .inline_max = 32u,
    };
    lfs_t filesystem;
    memset(&filesystem, 0, sizeof(filesystem));
    int error = lfs_format(&filesystem, &configuration);
    if (error < 0) {
        return rvb_lfs_error(error);
    }
    error = lfs_mount(&filesystem, &configuration);
    if (error < 0) {
        return rvb_lfs_error(error);
    }

    uint32_t trace = 0x4c465332u;
    for (uint32_t i = 0; i < operation_count; ++i) {
        const uint8_t *record = operations + i * 16u;
        const uint32_t kind = rvb_read_u32(record) % 5u;
        const uint32_t first = rvb_read_u32(record + 4u);
        const uint32_t second = rvb_read_u32(record + 8u);
        const uint32_t third = rvb_read_u32(record + 12u);
        const uint32_t file_id = first & (RVB_LFS_FILE_COUNT - 1u);
        uint32_t event;
        uint32_t status = 0u;
        if (kind <= 1u) {
            const uint32_t length = 1u + (second & 63u);
            status = rvb_lfs_write_operation(&filesystem, kind, file_id,
                                             length, third, &event);
        } else if (kind == 2u) {
            status = rvb_lfs_read_operation(&filesystem, file_id, second,
                                            third, &event);
        } else if (kind == 3u) {
            const uint32_t destination =
                second & (RVB_LFS_FILE_COUNT - 1u);
            char source_name[4];
            char destination_name[4];
            rvb_lfs_name(file_id, source_name);
            rvb_lfs_name(destination, destination_name);
            error = lfs_rename(&filesystem, source_name, destination_name);
            if (error < 0 && error != LFS_ERR_NOENT) {
                status = rvb_lfs_error(error);
            }
            event = (3u << 28) ^ (file_id << 24) ^ (destination << 20) ^
                    (error == 0 ? 0x13579bdfu : 0x2468ace0u);
        } else {
            char name[4];
            rvb_lfs_name(file_id, name);
            error = lfs_remove(&filesystem, name);
            if (error < 0 && error != LFS_ERR_NOENT) {
                status = rvb_lfs_error(error);
            }
            event = (4u << 28) ^ (file_id << 24) ^
                    (error == 0 ? 0x13579bdfu : 0x2468ace0u);
        }
        if (status != 0u) {
            (void)lfs_unmount(&filesystem);
            return status;
        }
        trace = rvb_fold(trace, event, i);
    }

    uint32_t state_digest;
    uint32_t status = rvb_lfs_state_digest(&filesystem, &state_digest);
    if (status != 0u) {
        (void)lfs_unmount(&filesystem);
        return status;
    }
    error = lfs_unmount(&filesystem);
    if (error < 0) {
        return rvb_lfs_error(error);
    }
    out[0] = trace;
    out[1] = state_digest;
    return 0u;
}

uint32_t rvb_littlefs(const uint8_t *input, uint32_t input_len,
                      uint32_t out[2]) {
    if (input_len < 16u || input_len % 16u != 0u) {
        return RVB_BAD_INPUT;
    }
    const uint32_t operation_count = input_len / 16u;
    if (operation_count > RVB_LFS_MAX_OPERATIONS) {
        return RVB_BAD_INPUT;
    }
    return rvb_littlefs_once(input, operation_count, out);
}
