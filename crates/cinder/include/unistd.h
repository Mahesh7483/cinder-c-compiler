#ifndef _CINDER_UNISTD_H
#define _CINDER_UNISTD_H

#include <stddef.h>
#include <sys/types.h>

#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int close(int fd);
unsigned int sleep(unsigned int seconds);
int usleep(unsigned int usec);
pid_t getpid(void);
int isatty(int fd);

#endif
