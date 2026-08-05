/* Minimal C library routines used by the freestanding upstream sources. */

#include <stddef.h>
#include <stdint.h>

void *memcpy(void *restrict destination, const void *restrict source,
             size_t count) {
    unsigned char *to = destination;
    const unsigned char *from = source;
    for (size_t i = 0; i < count; ++i) {
        to[i] = from[i];
    }
    return destination;
}

void *memmove(void *destination, const void *source, size_t count) {
    unsigned char *to = destination;
    const unsigned char *from = source;
    if ((uintptr_t)to < (uintptr_t)from) {
        for (size_t i = 0; i < count; ++i) {
            to[i] = from[i];
        }
    } else if ((uintptr_t)to > (uintptr_t)from) {
        for (size_t i = count; i != 0; --i) {
            to[i - 1] = from[i - 1];
        }
    }
    return destination;
}

void *memset(void *destination, int value, size_t count) {
    unsigned char *bytes = destination;
    for (size_t i = 0; i < count; ++i) {
        bytes[i] = (unsigned char)value;
    }
    return destination;
}

int memcmp(const void *left, const void *right, size_t count) {
    const unsigned char *a = left;
    const unsigned char *b = right;
    for (size_t i = 0; i < count; ++i) {
        if (a[i] != b[i]) {
            return a[i] < b[i] ? -1 : 1;
        }
    }
    return 0;
}

char *strcpy(char *restrict destination, const char *restrict source) {
    size_t index = 0;
    do {
        destination[index] = source[index];
    } while (source[index++] != '\0');
    return destination;
}

char *strchr(const char *string, int character) {
    const char target = (char)character;
    do {
        if (*string == target) {
            return (char *)string;
        }
    } while (*string++ != '\0');
    return NULL;
}

size_t strlen(const char *string) {
    size_t length = 0;
    while (string[length] != '\0') {
        ++length;
    }
    return length;
}

size_t strspn(const char *string, const char *accept) {
    size_t length = 0;
    while (string[length] != '\0') {
        const char *candidate = accept;
        while (*candidate != '\0' && *candidate != string[length]) {
            ++candidate;
        }
        if (*candidate == '\0') {
            break;
        }
        ++length;
    }
    return length;
}

size_t strcspn(const char *string, const char *reject) {
    size_t length = 0;
    while (string[length] != '\0') {
        const char *candidate = reject;
        while (*candidate != '\0' && *candidate != string[length]) {
            ++candidate;
        }
        if (*candidate != '\0') {
            break;
        }
        ++length;
    }
    return length;
}

void rvb_assertion_failed(void) {
    for (;;) {
    }
}
