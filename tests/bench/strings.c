/* String and memory work: hashing, copying, scanning, searching, formatting. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned long fnv(const unsigned char *p, size_t n) {
    unsigned long h = 14695981039346656037ul;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 1099511628211ul;
    }
    return h;
}

int main(void) {
    enum { LEN = 1 << 22 };
    unsigned char *buf = malloc(LEN), *copy = malloc(LEN);
    unsigned s = 99;
    for (int i = 0; i < LEN; i++) {
        s = s * 1664525u + 1013904223u;
        buf[i] = (unsigned char)(97 + (s >> 24) % 26);
    }
    unsigned long h = 0;
    for (int rep = 0; rep < 40; rep++) {
        memcpy(copy, buf, LEN);
        copy[rep] ^= 1;
        h ^= fnv(copy, LEN);
    }
    long vowels = 0, rare = 0;
    for (int rep = 0; rep < 10; rep++)
        for (int i = 0; i < LEN; i++) {
            unsigned char c = buf[i];
            vowels += c == 'a' || c == 'e' || c == 'i' || c == 'o' || c == 'u';
            rare += c == 'q';
        }
    long hits = 0;
    for (int rep = 0; rep < 20; rep++)
        for (const unsigned char *p = buf; (p = memchr(p, 'z', (size_t)(buf + LEN - p))); p++) hits++;
    char line[64];
    unsigned long fmt = 0;
    for (int i = 0; i < 300000; i++) {
        int n = snprintf(line, sizeof line, "item-%d:%x", i, i * 2654435761u);
        fmt += (unsigned long)n + (unsigned char)line[n - 1];
    }
    printf("%lx %ld %ld %ld %lu\n", h, vowels, rare, hits, fmt);
    free(buf);
    free(copy);
    return 0;
}
