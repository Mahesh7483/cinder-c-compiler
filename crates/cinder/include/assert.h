/* Deliberately not include-guarded: <assert.h> may be re-included after
   toggling NDEBUG. */
#undef assert
#ifndef __cplusplus
#undef static_assert
#define static_assert _Static_assert
#endif

#ifdef NDEBUG
#define assert(expr) ((void)0)
#else
_Noreturn void __assert_fail(const char *assertion, const char *file, unsigned int line, const char *function);
#define assert(expr) ((expr) ? (void)0 : __assert_fail(#expr, __FILE__, __LINE__, __func__))
#endif
