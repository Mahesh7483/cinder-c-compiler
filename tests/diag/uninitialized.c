#include <stdio.h>
#include <stdlib.h>

int certain(void) {
    int total;
    total += 5;
    return total;
}

int sometimes(int flag) {
    int value;
    if (flag > 0)
        value = 10;
    return value * 2;
}

int loop_may_not_run(int n) {
    int last;
    for (int i = 0; i < n; i++)
        last = i;
    return last;
}

int fine_both_branches(int flag) {
    int v;
    if (flag) v = 1; else v = 2;
    return v;
}

int fine_scanf(void) {
    int n;
    if (scanf("%d", &n) != 1) exit(1);
    return n;
}

int fine_switch(int k) {
    int r;
    switch (k) {
    case 0: r = 10; break;
    case 1: r = 20; break;
    default: abort();
    }
    return r;
}

int main(void) {
    char *msg;
    printf("%d\n", certain() + sometimes(1) + loop_may_not_run(3) + fine_both_branches(0) + fine_switch(1));
    puts(msg);
    return 0;
}
