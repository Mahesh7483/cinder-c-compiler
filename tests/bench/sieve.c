/* Sieve of Eratosthenes plus a Collatz scan: byte-array streaming, modulo, long arithmetic. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    enum { LIMIT = 30000000 };
    char *comp = calloc(LIMIT + 1, 1);
    long primes = 0, last = 0;
    for (long i = 2; i <= LIMIT; i++) {
        if (comp[i]) continue;
        primes++;
        last = i;
        for (long j = i * i; j <= LIMIT; j += i) comp[j] = 1;
    }
    printf("%ld %ld\n", primes, last);
    int best = 0, best_n = 0;
    for (int n = 1; n < 1500000; n++) {
        long x = n;
        int steps = 0;
        while (x != 1) {
            x = x % 2 ? 3 * x + 1 : x / 2;
            steps++;
        }
        if (steps > best) {
            best = steps;
            best_n = n;
        }
    }
    printf("%d %d\n", best_n, best);
    free(comp);
    return 0;
}
