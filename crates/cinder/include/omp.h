#ifndef _CINDER_OMP_H
#define _CINDER_OMP_H

/* A single-thread stand-in for the OpenMP runtime API.
 *
 * Cinder does not implement OpenMP: `#pragma omp` lines are ignored and every parallel region runs
 * once, on the calling thread (a team of one, which the OpenMP specification allows). Programs
 * without data races, and reductions, critical sections, atomics, sections and tasks, give the same
 * results; programs that print the thread count, rely on `private(x)` leaving an outer variable
 * unchanged, or on timing show the difference. */
#warning "OpenMP is not implemented: <omp.h> is a single-thread stand-in and '#pragma omp' is ignored, so the program runs on one thread"

#include <stddef.h>
#include <time.h>

typedef int omp_lock_t;
typedef int omp_nest_lock_t;

typedef enum omp_sched_t {
    omp_sched_static = 1,
    omp_sched_dynamic = 2,
    omp_sched_guided = 3,
    omp_sched_auto = 4
} omp_sched_t;

static inline void omp_set_num_threads(int n) { (void)n; }
static inline int omp_get_num_threads(void) { return 1; }
static inline int omp_get_max_threads(void) { return 1; }
static inline int omp_get_thread_num(void) { return 0; }
static inline int omp_get_num_procs(void) { return 1; }
static inline int omp_in_parallel(void) { return 0; }
static inline void omp_set_dynamic(int on) { (void)on; }
static inline int omp_get_dynamic(void) { return 0; }
static inline void omp_set_nested(int on) { (void)on; }
static inline int omp_get_nested(void) { return 0; }
static inline int omp_get_thread_limit(void) { return 1; }
static inline void omp_set_max_active_levels(int n) { (void)n; }
static inline int omp_get_max_active_levels(void) { return 1; }
static inline int omp_get_level(void) { return 0; }
static inline int omp_get_active_level(void) { return 0; }
static inline int omp_get_ancestor_thread_num(int level) { return level == 0 ? 0 : -1; }
static inline int omp_get_team_size(int level) { return level == 0 ? 1 : -1; }
static inline void omp_set_schedule(omp_sched_t kind, int chunk) { (void)kind; (void)chunk; }
static inline void omp_get_schedule(omp_sched_t *kind, int *chunk) {
    *kind = omp_sched_static;
    *chunk = 0;
}
static inline int omp_get_num_teams(void) { return 1; }
static inline int omp_get_team_num(void) { return 0; }
static inline int omp_is_initial_device(void) { return 1; }

static inline double omp_get_wtime(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}
static inline double omp_get_wtick(void) { return 1e-9; }

/* locks: with one thread they never block */
static inline void omp_init_lock(omp_lock_t *l) { *l = 0; }
static inline void omp_destroy_lock(omp_lock_t *l) { (void)l; }
static inline void omp_set_lock(omp_lock_t *l) { *l = 1; }
static inline void omp_unset_lock(omp_lock_t *l) { *l = 0; }
static inline int omp_test_lock(omp_lock_t *l) {
    if (*l) return 0;
    *l = 1;
    return 1;
}
static inline void omp_init_nest_lock(omp_nest_lock_t *l) { *l = 0; }
static inline void omp_destroy_nest_lock(omp_nest_lock_t *l) { (void)l; }
static inline void omp_set_nest_lock(omp_nest_lock_t *l) { (*l)++; }
static inline void omp_unset_nest_lock(omp_nest_lock_t *l) { if (*l > 0) (*l)--; }
static inline int omp_test_nest_lock(omp_nest_lock_t *l) { return ++(*l); }

#endif
