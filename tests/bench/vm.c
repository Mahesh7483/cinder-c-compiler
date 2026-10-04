/* A small bytecode interpreter: dense switch dispatch, a classic compiler stress test. */
#include <stdio.h>

enum { PUSH, ADD, SUB, MUL, DUP, JNZ, LOAD, STORE, DEC, MOD, HALT, SWAP, POP, XOR };

static long run(const int *code) {
    long stack[64], mem[16] = {0};
    int sp = 0, pc = 0;
    long steps = 0;
    for (;;) {
        steps++;
        switch (code[pc++]) {
        case PUSH: stack[sp++] = code[pc++]; break;
        case ADD: sp--; stack[sp - 1] += stack[sp]; break;
        case SUB: sp--; stack[sp - 1] -= stack[sp]; break;
        case MUL: sp--; stack[sp - 1] *= stack[sp]; break;
        case MOD: sp--; stack[sp - 1] %= stack[sp]; break;
        case XOR: sp--; stack[sp - 1] ^= stack[sp]; break;
        case DUP: stack[sp] = stack[sp - 1]; sp++; break;
        case SWAP: {
            long t = stack[sp - 1];
            stack[sp - 1] = stack[sp - 2];
            stack[sp - 2] = t;
            break;
        }
        case POP: sp--; break;
        case JNZ: {
            int t = code[pc++];
            if (stack[--sp]) pc = t;
            break;
        }
        case LOAD: stack[sp++] = mem[code[pc++]]; break;
        case STORE: mem[code[pc++]] = stack[--sp]; break;
        case DEC: stack[sp - 1]--; break;
        case HALT: printf("acc=%ld\n", mem[1]); return steps;
        default: return -1;
        }
    }
}

int main(void) {
    /* for (i = 40000000; i; i--) acc = ((acc * 31 + i) % 1000003) ^ i; */
    int prog[] = {PUSH, 40000000, STORE, 0, PUSH, 7, STORE, 1,
                  LOAD, 1, PUSH, 31, MUL, LOAD, 0, ADD, PUSH, 1000003, MOD, LOAD, 0, XOR, STORE, 1,
                  LOAD, 0, DEC, DUP, STORE, 0, JNZ, 8,
                  HALT};
    printf("steps=%ld\n", run(prog));
    return 0;
}
