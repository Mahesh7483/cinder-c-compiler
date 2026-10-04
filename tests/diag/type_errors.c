#include <stdio.h>

struct point { int x, y; };

int count = 0;

int add(int a, int b) { return a + b; }

int main(void) {
    struct point p = {1, 2};
    int too_few = add(1);
    int too_many = add(1, 2, 3);
    p.z = 4;
    const int k = 1;
    k = 2;
    int *ip = &p;
    char *s = 5;
    int total = coutn + 1;
    undefined_fn(3);
    int bad_sum = p + 1;
    return *total;
}
