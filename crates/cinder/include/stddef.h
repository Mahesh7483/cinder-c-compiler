#ifndef _CINDER_STDDEF_H
#define _CINDER_STDDEF_H

typedef unsigned long size_t;
typedef long ptrdiff_t;
typedef int wchar_t;
/* Same size and alignment as glibc's max_align_t (which contains a long double). */
typedef struct {
    _Alignas(16) long long __ll;
    char __pad[16];
} max_align_t;

#ifndef NULL
#define NULL ((void *)0)
#endif

#define offsetof(type, member) __builtin_offsetof(type, member)

#endif
