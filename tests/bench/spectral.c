/* Spectral norm of an infinite matrix: nested loops, division, calls in hot paths. */
#include <stdio.h>
#include <math.h>
#include <stdlib.h>

static double a(int i, int j) { return 1.0 / ((i + j) * (i + j + 1) / 2 + i + 1); }

static void mul_av(int n, const double *v, double *av) {
    for (int i = 0; i < n; i++) {
        double s = 0;
        for (int j = 0; j < n; j++) s += a(i, j) * v[j];
        av[i] = s;
    }
}

static void mul_atv(int n, const double *v, double *atv) {
    for (int i = 0; i < n; i++) {
        double s = 0;
        for (int j = 0; j < n; j++) s += a(j, i) * v[j];
        atv[i] = s;
    }
}

static void mul_atav(int n, const double *v, double *out, double *tmp) {
    mul_av(n, v, tmp);
    mul_atv(n, tmp, out);
}

int main(void) {
    int n = 1200;
    double *u = malloc(n * sizeof *u), *v = malloc(n * sizeof *v), *tmp = malloc(n * sizeof *tmp);
    for (int i = 0; i < n; i++) u[i] = 1;
    for (int i = 0; i < 10; i++) {
        mul_atav(n, u, v, tmp);
        mul_atav(n, v, u, tmp);
    }
    double vbv = 0, vv = 0;
    for (int i = 0; i < n; i++) {
        vbv += u[i] * v[i];
        vv += v[i] * v[i];
    }
    printf("%.9f\n", sqrt(vbv / vv));
    free(u);
    free(v);
    free(tmp);
    return 0;
}
