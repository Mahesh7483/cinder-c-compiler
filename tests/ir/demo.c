#include <stdio.h>

struct point { int x, y; };

static int dot(struct point a, struct point b) { return a.x * b.x + a.y * b.y; }

int sum_to(int n) {
    int total = 0;
    for (int i = 1; i <= n; i++)
        total += i;
    return total;
}

int main(void) {
    struct point p = { 3, 4 };
    printf("%d %d\n", sum_to(10), dot(p, p));
    return 0;
}
