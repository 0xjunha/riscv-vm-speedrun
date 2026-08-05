#include "picojpeg.h"
#include "rvb_workload_common.h"

#include <stdint.h>
#include <string.h>

struct input_stream {
    const uint8_t *bytes;
    uint32_t length;
    uint32_t offset;
};

static unsigned char provide_bytes(unsigned char *buffer,
                                   unsigned char buffer_size,
                                   unsigned char *bytes_read, void *opaque) {
    struct input_stream *stream = opaque;
    uint32_t available = stream->length - stream->offset;
    uint32_t count = buffer_size;
    if (count > available) {
        count = available;
    }
    memcpy(buffer, stream->bytes + stream->offset, count);
    stream->offset += count;
    *bytes_read = (unsigned char)count;
    return 0u;
}

uint32_t rvb_picojpeg(const uint8_t *input, uint32_t input_len,
                      uint32_t out[2]) {
    if (input_len < 64u || input_len > 16384u) {
        return RVB_BAD_INPUT;
    }

    struct input_stream stream = {input, input_len, 0u};
    pjpeg_image_info_t image;
    unsigned char status =
        pjpeg_decode_init(&image, provide_bytes, &stream, 0u);
    if (status != 0u || image.m_width <= 0 || image.m_height <= 0 ||
        image.m_MCUSPerRow <= 0 || image.m_MCUSPerCol <= 0) {
        return RVB_BAD_INPUT;
    }

    const uint32_t bytes_per_component =
        (uint32_t)image.m_MCUWidth * (uint32_t)image.m_MCUHeight;
    const uint32_t expected_mcus =
        (uint32_t)image.m_MCUSPerRow * (uint32_t)image.m_MCUSPerCol;
    uint32_t pixels_crc = UINT32_MAX;
    uint32_t blocks = 0x4a504547u;
    uint32_t decoded_mcus = 0u;

    while ((status = pjpeg_decode_mcu()) == 0u) {
        uint32_t block_crc = UINT32_MAX;
        block_crc = rvb_crc32_update(block_crc, image.m_pMCUBufR,
                                     bytes_per_component);
        pixels_crc = rvb_crc32_update(pixels_crc, image.m_pMCUBufR,
                                      bytes_per_component);
        if (image.m_comps == 3) {
            block_crc = rvb_crc32_update(block_crc, image.m_pMCUBufG,
                                         bytes_per_component);
            block_crc = rvb_crc32_update(block_crc, image.m_pMCUBufB,
                                         bytes_per_component);
            pixels_crc = rvb_crc32_update(pixels_crc, image.m_pMCUBufG,
                                          bytes_per_component);
            pixels_crc = rvb_crc32_update(pixels_crc, image.m_pMCUBufB,
                                          bytes_per_component);
        }
        blocks = rvb_fold(blocks, rvb_crc32_finish(block_crc), decoded_mcus);
        ++decoded_mcus;
    }
    if (status != PJPG_NO_MORE_BLOCKS || decoded_mcus != expected_mcus) {
        return RVB_INTERNAL_ERROR;
    }

    blocks = rvb_fold(blocks,
                      ((uint32_t)image.m_width << 16) ^
                          (uint32_t)image.m_height,
                      decoded_mcus);
    blocks = rvb_fold(blocks,
                      ((uint32_t)image.m_scanType << 24) ^ decoded_mcus,
                      decoded_mcus + 1u);
    out[0] = rvb_crc32_finish(pixels_crc);
    out[1] = blocks;
    return 0u;
}
