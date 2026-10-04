// Example programs for the playground's dropdown.
// Fields: id, group, title, code, and optionally opt (0-2), emit (asm|ir|ast|hir|pp), stdin, note.

export const EXAMPLES = [
  {
    id: 'hello',
    group: 'Basics',
    title: 'Hello, world',
    code: `#include <stdio.h>

int main(void) {
    printf("Hello from Cinder!\\n");
    return 0;
}
`,
  },
  {
    id: 'fib',
    group: 'Basics',
    title: 'Fibonacci (recursion)',
    opt: 2,
    emit: 'asm',
    code: `#include <stdio.h>

static int fib(int n) {
    return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

int main(void) {
    for (int i = 0; i <= 20; i += 4)
        printf("fib(%d) = %d\\n", i, fib(i));
    return 0;
}
`,
  },
  {
    id: 'sieve',
    group: 'Basics',
    title: 'Sieve of Eratosthenes',
    opt: 2,
    code: `#include <stdio.h>
#include <string.h>

#define N 100

int main(void) {
    char composite[N + 1];
    memset(composite, 0, sizeof composite);
    int count = 0;
    for (int i = 2; i <= N; i++) {
        if (composite[i]) continue;
        printf("%d ", i);
        count++;
        for (int j = i * i; j <= N; j += i) composite[j] = 1;
    }
    printf("\\n%d primes up to %d\\n", count, N);
    return 0;
}
`,
  },
  {
    id: 'stdin',
    group: 'Basics',
    title: 'Reading input (word count)',
    stdin: 'the quick brown fox\njumps over the lazy dog\n',
    code: `#include <ctype.h>
#include <stdio.h>

int main(void) {
    int lines = 0, words = 0, chars = 0, in_word = 0, c;
    while ((c = getchar()) != EOF) {
        chars++;
        if (c == '\\n') lines++;
        if (isspace(c)) in_word = 0;
        else if (!in_word) { in_word = 1; words++; }
    }
    printf("%d lines, %d words, %d characters\\n", lines, words, chars);
    return 0;
}
`,
  },
  {
    id: 'structs',
    group: 'Language',
    title: 'Structs, unions and bit-fields',
    code: `#include <stdio.h>
#include <stddef.h>

struct packet {
    unsigned version : 3;
    unsigned kind    : 5;
    unsigned length  : 8;
    union {
        int   i;
        float f;
        char  bytes[4];
    } payload;
};

int main(void) {
    struct packet p = { .version = 2, .kind = 17, .length = 200, .payload.f = 1.5f };
    printf("version=%u kind=%u length=%u\\n", p.version, p.kind, p.length);
    printf("payload bits: %08x\\n", (unsigned)p.payload.i);
    printf("sizeof(struct packet) = %zu, offsetof(payload) = %zu\\n", sizeof p, offsetof(struct packet, payload));
    return 0;
}
`,
  },
  {
    id: 'funcptr',
    group: 'Language',
    title: 'Function pointers and qsort',
    code: `#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int (*cmp_fn)(const void *, const void *);

static int by_value(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }
static int by_length(const void *a, const void *b) {
    return (int)strlen(*(const char *const *)a) - (int)strlen(*(const char *const *)b);
}

int main(void) {
    int nums[] = {42, 7, 19, -3, 100, 0};
    const char *words[] = {"banana", "fig", "cherry", "apple", "kiwi"};
    cmp_fn table[] = {by_value, by_length};
    qsort(nums, 6, sizeof nums[0], table[0]);
    qsort(words, 5, sizeof words[0], table[1]);
    for (int i = 0; i < 6; i++) printf("%d ", nums[i]);
    printf("\\n");
    for (int i = 0; i < 5; i++) printf("%s ", words[i]);
    printf("\\n");
    return 0;
}
`,
  },
  {
    id: 'vla',
    group: 'Language',
    title: 'Variable length arrays',
    emit: 'ir',
    code: `#include <stdio.h>

static long triangle(int n) {
    int m[n][n];                       /* 2-D VLA with run-time strides */
    for (int i = 0; i < n; i++)
        for (int j = 0; j < n; j++)
            m[i][j] = i >= j ? i + j : 0;
    long sum = 0;
    for (int i = 0; i < n; i++)
        for (int j = 0; j < n; j++) sum += m[i][j];
    printf("sizeof(m) = %zu bytes for n = %d\\n", sizeof m, n);
    return sum;
}

int main(void) {
    printf("%ld\\n", triangle(5));
    printf("%ld\\n", triangle(12));
    return 0;
}
`,
  },
  {
    id: 'variadic',
    group: 'Language',
    title: 'Variadic functions',
    code: `#include <stdarg.h>
#include <stdio.h>

static double average(int count, ...) {
    va_list ap;
    va_start(ap, count);
    double total = 0;
    for (int i = 0; i < count; i++) total += va_arg(ap, double);
    va_end(ap);
    return count ? total / count : 0;
}

static void logf_(const char *level, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    printf("[%s] ", level);
    vprintf(fmt, ap);
    printf("\\n");
    va_end(ap);
}

int main(void) {
    printf("%.2f\\n", average(4, 1.5, 2.5, 3.5, 10.0));
    logf_("info", "%d items, %s, %.1f%%", 3, "ok", 99.5);
    return 0;
}
`,
  },
  {
    id: 'macros',
    group: 'Language',
    title: 'Preprocessor: macros, # and ##',
    emit: 'pp',
    code: `#include <stdio.h>

#define SQUARE(x) ((x) * (x))
#define STR(x) #x
#define CONCAT(a, b) a##b
#define DECLARE_GETTER(type, name) static type get_##name(void) { return name; }
#define MAX(a, b) ((a) > (b) ? (a) : (b))

static int width = 7;
DECLARE_GETTER(int, width)

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define LANG "C11"
#else
#define LANG "older C"
#endif

int main(void) {
    int CONCAT(val, ue) = SQUARE(3 + 1);
    printf("%s = %d, max = %d, width = %d (%s)\\n", STR(value), value, MAX(value, 20), get_width(), LANG);
    printf("%s:%d\\n", __FILE__, __LINE__);
    return 0;
}
`,
  },
  {
    id: 'opt-fold',
    group: 'Compiler tour',
    title: 'Optimizer: folding and strength reduction',
    opt: 2,
    emit: 'ir',
    note: 'Compare -O0 and -O2: the alloca/load/store traffic disappears (mem2reg), constants fold, x*8 becomes a shift and x/4 a bias-and-shift sequence.',
    code: `int scale(int x) {
    int eight = 2 * 4;
    int bias = 100 - 98;
    return x * eight + x / 4 + bias;
}

unsigned udiv(unsigned x) { return x / 16 + x % 16; }
`,
  },
  {
    id: 'opt-loop',
    group: 'Compiler tour',
    title: 'Optimizer: loop-invariant code motion',
    opt: 2,
    emit: 'ir',
    note: 'The product a*b does not change inside the loop, so -O2 computes it once before the loop (look for the loop.ph block).',
    code: `int sum(int a, int b, int n) {
    int s = 0;
    for (int i = 0; i < n; i++)
        s += a * b + i;
    return s;
}
`,
  },
  {
    id: 'opt-tail',
    group: 'Compiler tour',
    title: 'Optimizer: tail recursion becomes a loop',
    opt: 2,
    emit: 'asm',
    note: 'At -O2 the self-call turns into a jump (no stack growth): try a depth of 50,000,000.',
    code: `#include <stdio.h>

static long sum_to(long n, long acc) {
    if (n == 0) return acc;
    return sum_to(n - 1, acc + n);
}

int main(void) {
    printf("%ld\\n", sum_to(50000000L, 0));
    return 0;
}
`,
  },
  {
    id: 'opt-switch',
    group: 'Compiler tour',
    title: 'Dense switch becomes a jump table',
    opt: 1,
    emit: 'asm',
    code: `int classify(int c) {
    switch (c) {
    case 0: return 10;
    case 1: return 20;
    case 2: return 15;
    case 3: return 40;
    case 4: return 5;
    case 5: return 60;
    case 6: return 7;
    default: return -1;
    }
}
`,
  },
  {
    id: 'diag',
    group: 'Compiler tour',
    title: 'Diagnostics and warnings',
    code: `#include <stdio.h>

int pick(int flag) {
    int value;
    if (flag > 0)
        value = 10;
    return value * 2;          /* may be uninitialized */
}

int main(void) {
    long big = 1L << 40;
    int narrow = big;          /* implicit conversion loses precision */
    int total = narrw + 1;     /* typo: did you mean 'narrow'? */
    printf("%d\\n", pick(1) + total);
    return 0;
}
`,
  },
  {
    id: 'bits',
    group: 'Algorithms',
    title: 'Bit tricks',
    code: `#include <stdio.h>
#include <stdint.h>

static int popcount(uint32_t v) {
    int n = 0;
    for (; v; v &= v - 1) n++;
    return n;
}

static uint32_t reverse_bits(uint32_t v) {
    v = ((v >> 1) & 0x55555555u) | ((v & 0x55555555u) << 1);
    v = ((v >> 2) & 0x33333333u) | ((v & 0x33333333u) << 2);
    v = ((v >> 4) & 0x0F0F0F0Fu) | ((v & 0x0F0F0F0Fu) << 4);
    v = ((v >> 8) & 0x00FF00FFu) | ((v & 0x00FF00FFu) << 8);
    return (v >> 16) | (v << 16);
}

int main(void) {
    uint32_t x = 0xDEADBEEFu;
    printf("popcount(%08x) = %d\\n", x, popcount(x));
    printf("reverse(%08x)  = %08x\\n", x, reverse_bits(x));
    printf("is power of two: %d %d\\n", (64 & 63) == 0, (96 & 95) == 0);
    return 0;
}
`,
  },
  {
    id: 'matmul',
    group: 'Algorithms',
    title: 'Matrix multiply (floating point)',
    opt: 2,
    code: `#include <stdio.h>

#define N 4

int main(void) {
    double a[N][N], b[N][N], c[N][N];
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            a[i][j] = i + j;
            b[i][j] = i == j ? 2.0 : 0.5;
            c[i][j] = 0;
        }
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++)
            for (int k = 0; k < N; k++) c[i][j] += a[i][k] * b[k][j];
    for (int i = 0; i < N; i++) {
        for (int j = 0; j < N; j++) printf("%7.2f", c[i][j]);
        printf("\\n");
    }
    return 0;
}
`,
  },
  {
    id: 'list',
    group: 'Algorithms',
    title: 'Linked list and malloc',
    code: `#include <stdio.h>
#include <stdlib.h>

struct node {
    int value;
    struct node *next;
};

static struct node *push(struct node *head, int value) {
    struct node *n = malloc(sizeof *n);
    n->value = value;
    n->next = head;
    return n;
}

static struct node *reverse(struct node *head) {
    struct node *prev = NULL;
    while (head) {
        struct node *next = head->next;
        head->next = prev;
        prev = head;
        head = next;
    }
    return prev;
}

int main(void) {
    struct node *list = NULL;
    for (int i = 1; i <= 6; i++) list = push(list, i * i);
    for (struct node *p = list; p; p = p->next) printf("%d ", p->value);
    printf("\\n");
    list = reverse(list);
    for (struct node *p = list; p;) {
        printf("%d ", p->value);
        struct node *next = p->next;
        free(p);
        p = next;
    }
    printf("\\n");
    return 0;
}
`,
  },
];
