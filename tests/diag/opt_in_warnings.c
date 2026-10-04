// flags: -Wshadow -Wsign-conversion -Wempty-body -Wno-unused-variable
int counter;

unsigned to_unsigned(int value) {
    unsigned result = value;
    return result;
}

int shadows(int counter, int limit) {
    int total = 0;
    for (int i = 0; i < limit; i++) {
        int total = i;
        counter += total;
    }
    if (counter > 10);
    return total + counter;
}

int fine(unsigned u) {
    unsigned v = 5;
    int k = (int)u;
    return k + (int)v;
}
