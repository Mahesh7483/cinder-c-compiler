#include <stdio.h>

static int gcd(int a, int b) {
    while (b) {
        int t = a % b;
        a = b;
        b = t;
    }
    return a;
}

static int tri(int n, int acc) {
    if (n == 0) return acc;
    return tri(n - 1, acc + n);
}

int table[16];

int mix(int n) {
    int s = 0;
    for (int i = 0; i < n; i++) {
        table[i & 15] += i * 3;
        s += gcd(i + 1, 12) + tri(i & 7, 0) + table[(i + 1) & 15] / 4 + (i % 5) * 8;
    }
    return s;
}

int main(void) {
    printf("%d\n", mix(1000));
    return 0;
}
