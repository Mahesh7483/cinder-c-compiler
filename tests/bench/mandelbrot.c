/* Mandelbrot set escape-time iteration: tight floating-point loops with data-dependent exits. */
#include <stdio.h>

int main(void) {
    enum { W = 700, H = 700, MAXIT = 300 };
    long total = 0;
    unsigned hist[8] = {0};
    for (int py = 0; py < H; py++)
        for (int px = 0; px < W; px++) {
            double cr = -2.2 + px * 3.2 / W, ci = -1.6 + py * 3.2 / H, zr = 0, zi = 0;
            int i = 0;
            while (i < MAXIT && zr * zr + zi * zi <= 4.0) {
                double t = zr * zr - zi * zi + cr;
                zi = 2 * zr * zi + ci;
                zr = t;
                i++;
            }
            total += i;
            hist[i * 7 / MAXIT]++;
        }
    printf("%ld", total);
    for (int i = 0; i < 8; i++) printf(" %u", hist[i]);
    printf("\n");
    return 0;
}
