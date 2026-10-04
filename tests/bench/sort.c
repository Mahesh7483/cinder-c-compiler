/* Quicksort, mergesort and insertion sort on pseudo-random data: memory access, branches, loops. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned seed = 12345;
static unsigned next(void) {
    seed = seed * 1103515245u + 12345u;
    return seed >> 8;
}

static void quicksort(int *a, int lo, int hi) {
    while (lo < hi) {
        int p = a[(lo + hi) / 2], i = lo, j = hi;
        while (i <= j) {
            while (a[i] < p) i++;
            while (a[j] > p) j--;
            if (i <= j) {
                int t = a[i];
                a[i] = a[j];
                a[j] = t;
                i++;
                j--;
            }
        }
        if (j - lo < hi - i) {
            quicksort(a, lo, j);
            lo = i;
        } else {
            quicksort(a, i, hi);
            hi = j;
        }
    }
}

static void mergesort(int *a, int *tmp, int n) {
    if (n < 2) return;
    int mid = n / 2;
    mergesort(a, tmp, mid);
    mergesort(a + mid, tmp, n - mid);
    int i = 0, j = mid, k = 0;
    while (i < mid && j < n) tmp[k++] = a[i] <= a[j] ? a[i++] : a[j++];
    while (i < mid) tmp[k++] = a[i++];
    while (j < n) tmp[k++] = a[j++];
    memcpy(a, tmp, n * sizeof *a);
}

static void insertion(int *a, int n) {
    for (int i = 1; i < n; i++) {
        int key = a[i], j = i - 1;
        while (j >= 0 && a[j] > key) {
            a[j + 1] = a[j];
            j--;
        }
        a[j + 1] = key;
    }
}

static unsigned checksum(const int *a, int n) {
    unsigned h = 2166136261u;
    for (int i = 0; i < n; i++) h = (h ^ (unsigned)a[i]) * 16777619u;
    return h;
}

int main(void) {
    enum { N = 3000000, M = 1500000, S = 20000 };
    int *a = malloc(N * sizeof *a), *b = malloc(M * sizeof *b), *c = malloc(S * sizeof *c), *tmp = malloc(M * sizeof *tmp);
    for (int i = 0; i < N; i++) a[i] = (int)(next() % 1000000);
    for (int i = 0; i < M; i++) b[i] = (int)(next() % 1000000);
    for (int i = 0; i < S; i++) c[i] = (int)(next() % 1000000);
    quicksort(a, 0, N - 1);
    mergesort(b, tmp, M);
    insertion(c, S);
    for (int i = 1; i < N; i++)
        if (a[i - 1] > a[i]) return puts("quicksort broken"), 1;
    for (int i = 1; i < M; i++)
        if (b[i - 1] > b[i]) return puts("mergesort broken"), 1;
    for (int i = 1; i < S; i++)
        if (c[i - 1] > c[i]) return puts("insertion broken"), 1;
    printf("%08x %08x %08x\n", checksum(a, N), checksum(b, M), checksum(c, S));
    return 0;
}
