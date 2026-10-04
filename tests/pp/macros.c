#include <stdbool.h>
#include <limits.h>

#define SQUARE(x) ((x) * (x))
#define STR(x) #x
#define XSTR(x) STR(x)
#define CAT(a, b) a##b
#define LOG(fmt, ...) printf("[" __FILE__ ":" XSTR(__LINE__) "] " fmt, ##__VA_ARGS__)

#if defined(__linux__) && __x86_64__
#define PLATFORM "linux-x86_64"
#else
#define PLATFORM "unknown"
#endif

int CAT(var_, 1) = SQUARE(3 + 1);
const char *platform = PLATFORM;
const char *where = XSTR(__LINE__);
int max = INT_MAX;
bool ok = true;

void f(void) {
    LOG("hello\n");
    LOG("%d\n", CAT(var_, 1));
}
