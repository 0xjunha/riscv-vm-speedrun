#include "rvb_workload_common.h"

#include <stddef.h>
#include <stdint.h>

#define AES_KEY_SIZE 32u
#define AES_DATA_SIZE 32u
#define AES_RECORD_SIZE (AES_KEY_SIZE + AES_DATA_SIZE)
#define AES_MAX_ROUNDS 14u

struct aes_ctx {
    unsigned rounds;
    uint32_t keys[4u * (AES_MAX_ROUNDS + 1u)];
};

void aes_set_encrypt_key(struct aes_ctx *context, size_t key_size,
                         const uint8_t *key);
void aes_set_decrypt_key(struct aes_ctx *context, size_t key_size,
                         const uint8_t *key);
void aes_encrypt(const struct aes_ctx *context, size_t length, uint8_t *out,
                 const uint8_t *input);
void aes_decrypt(const struct aes_ctx *context, size_t length, uint8_t *out,
                 const uint8_t *input);

uint32_t rvb_aes(const uint8_t *input, uint32_t input_len, uint32_t out[2]) {
    if (input_len == 0u || input_len % AES_RECORD_SIZE != 0u ||
        input_len / AES_RECORD_SIZE > 64u) {
        return RVB_BAD_INPUT;
    }

    struct aes_ctx encrypt_context;
    struct aes_ctx decrypt_context;
    uint8_t encrypted[AES_DATA_SIZE];
    uint8_t decrypted[AES_DATA_SIZE];
    uint32_t encrypted_crc = UINT32_MAX;
    uint32_t decrypted_crc = UINT32_MAX;

    for (uint32_t offset = 0u; offset < input_len; offset += AES_RECORD_SIZE) {
        const uint8_t *key = input + offset;
        const uint8_t *plaintext = key + AES_KEY_SIZE;
        aes_set_encrypt_key(&encrypt_context, AES_KEY_SIZE, key);
        aes_encrypt(&encrypt_context, AES_DATA_SIZE, encrypted, plaintext);
        aes_set_decrypt_key(&decrypt_context, AES_KEY_SIZE, key);
        aes_decrypt(&decrypt_context, AES_DATA_SIZE, decrypted, encrypted);
        encrypted_crc =
            rvb_crc32_update(encrypted_crc, encrypted, AES_DATA_SIZE);
        decrypted_crc =
            rvb_crc32_update(decrypted_crc, decrypted, AES_DATA_SIZE);
    }

    out[0] = rvb_crc32_finish(encrypted_crc);
    out[1] = rvb_crc32_finish(decrypted_crc);
    return 0u;
}
