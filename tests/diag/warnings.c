// flags: -Wall -Wextra
#include <stdio.h>

static void unused_helper(void) {}

int missing_return(int x) {
    if (x > 0) return 1;
}

int unused_param(int used, int unused) { return used; }

int main(int argc, char **argv) {
    int unused_local = 3;
    int a = 5;
    long big = 1L << 40;
    int narrow = big;
    unsigned u = -1;
    char c = 300;
    if (a = 3) puts("assignment in condition");
    a + 1;
    unsigned short s = a;
    double d = 2.5;
    int t = d;
    float f = d;
    int cmp = a < u;
    int shadow = 0;
    {
        int shadow = 1;
        (void)shadow;
    }
    (void)shadow;
unused_label:
    return narrow + c + t + (int)f + cmp + s;
}
