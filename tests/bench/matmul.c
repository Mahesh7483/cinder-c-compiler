/* Dense double-precision matrix multiply: floating point, array indexing, loop nests. */
#include <stdio.h>

#define N 360

static double A[N][N], B[N][N], C[N][N];

int main(void) {
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            A[i][j] = (i * 31 + j * 17) % 100 / 10.0;
            B[i][j] = (i * 13 + j * 7) % 100 / 10.0;
        }
    for (int rep = 0; rep < 3; rep++)
        for (int i = 0; i < N; i++)
            for (int j = 0; j < N; j++) {
                double s = 0;
                for (int k = 0; k < N; k++) s += A[i][k] * B[k][j];
                C[i][j] = s;
            }
    double trace = 0, total = 0;
    for (int i = 0; i < N; i++) {
        trace += C[i][i];
        for (int j = 0; j < N; j++) total += C[i][j];
    }
    printf("%.4f %.4f\n", trace, total);
    return 0;
}
