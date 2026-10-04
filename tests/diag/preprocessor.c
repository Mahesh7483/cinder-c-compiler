#define SQUARE(x) ((x) * (x))
#define TWICE(x, y) x + y
#define REDEFINED 1
#define REDEFINED 2

#if UNDEFINED_VALUE > 3
int hidden;
#endif

int a = SQUARE(1, 2);
int b = TWICE(1);
#ifdef REDEFINED
#warning this is only a warning
#else
#endif

#error stopping here on purpose
int after_error;

int c = SQUARE(;
