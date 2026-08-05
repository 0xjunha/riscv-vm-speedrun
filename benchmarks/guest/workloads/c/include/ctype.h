#ifndef RVB_FREESTANDING_CTYPE_H
#define RVB_FREESTANDING_CTYPE_H

static inline int isdigit(int character) {
    return character >= '0' && character <= '9';
}

static inline int isspace(int character) {
    return character == ' ' || character == '\t' || character == '\n' ||
           character == '\r' || character == '\f' || character == '\v';
}

static inline int isxdigit(int character) {
    return isdigit(character) || (character >= 'a' && character <= 'f') ||
           (character >= 'A' && character <= 'F');
}

static inline int tolower(int character) {
    return character >= 'A' && character <= 'Z' ? character + ('a' - 'A')
                                                 : character;
}

#endif
