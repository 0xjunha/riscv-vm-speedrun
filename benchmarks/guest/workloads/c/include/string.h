#ifndef RVB_FREESTANDING_STRING_H
#define RVB_FREESTANDING_STRING_H

#include <stddef.h>

void *memcpy(void *restrict destination, const void *restrict source,
             size_t count);
void *memmove(void *destination, const void *source, size_t count);
void *memset(void *destination, int value, size_t count);
int memcmp(const void *left, const void *right, size_t count);
char *strcpy(char *restrict destination, const char *restrict source);
char *strchr(const char *string, int character);
size_t strspn(const char *string, const char *accept);
size_t strcspn(const char *string, const char *reject);

#endif
