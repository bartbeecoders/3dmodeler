// Shims that let box3d link on wasm32-unknown-unknown (browser, single thread).
//
// box3d's core simulation only needs malloc/mem/math/qsort/snprintf, which the
// wasi sysroot's libc.a provides without any WASI imports. The symbols below
// are the remainder:
//
//  - pthreads/semaphores: referenced by the built-in worker scheduler. With
//    b3WorldDef.workerCount <= 1 box3d takes its serial fallback and never
//    calls these; they only need to exist so the linker is satisfied.
//  - clock/sleep: used for profiling timers only.
//  - stdio streams: used only by the debug dump / recording-to-file features.
//    fopen always fails, printf-family output is forwarded to the host via
//    js_log (exported from the Rust side).
//
// Defining these here prevents the linker from pulling the wasi-libc versions,
// which would drag in wasi_snapshot_preview1 imports the browser doesn't have.

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

// Provided by the Rust side; forwards to the JS console.
extern void js_log(const char* msg, size_t len);

// --- minimal printf-style formatter ------------------------------------
//
// wasi-libc implements vsnprintf on top of vfprintf, which we must shadow
// (the real one drags in WASI fd_write imports). To break that cycle the
// whole printf family is implemented here, self-contained. box3d only uses
// formatting for debug output, so this supports the common conversions
// (%d %i %u %x %X %p %s %c %f %F %g %G %e %%) with '-'/'0' flags, width,
// and precision — enough for readable logs, not a full C99 implementation.

typedef struct
{
	char* dst;
	size_t cap; // capacity excluding space for the terminator handling below
	size_t len; // total formatted length (may exceed cap)
} FmtBuf;

static void fmt_putc(FmtBuf* b, char c)
{
	if (b->len < b->cap)
	{
		b->dst[b->len] = c;
	}
	b->len += 1;
}

static void fmt_pad(FmtBuf* b, char c, int count)
{
	for (int i = 0; i < count; ++i)
	{
		fmt_putc(b, c);
	}
}

static void fmt_str(FmtBuf* b, const char* s, int max, int width, bool left, char pad)
{
	int slen = 0;
	while (s[slen] != '\0' && (max < 0 || slen < max))
	{
		slen += 1;
	}
	if (!left)
	{
		fmt_pad(b, pad, width - slen);
	}
	for (int i = 0; i < slen; ++i)
	{
		fmt_putc(b, s[i]);
	}
	if (left)
	{
		fmt_pad(b, ' ', width - slen);
	}
}

static int fmt_utoa(char* tmp, unsigned long long v, unsigned base, bool upper)
{
	const char* digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
	int n = 0;
	do
	{
		tmp[n++] = digits[v % base];
		v /= base;
	} while (v != 0);
	// reverse
	for (int i = 0; i < n / 2; ++i)
	{
		char c = tmp[i];
		tmp[i] = tmp[n - 1 - i];
		tmp[n - 1 - i] = c;
	}
	tmp[n] = '\0';
	return n;
}

static void fmt_double(FmtBuf* b, double v, int precision, int width, bool left, char pad)
{
	char tmp[64];
	int n = 0;

	if (v != v)
	{
		fmt_str(b, "nan", -1, width, left, ' ');
		return;
	}
	bool neg = v < 0.0;
	if (neg)
	{
		v = -v;
	}
	if (v > 1.8e18)
	{
		fmt_str(b, neg ? "-inf/big" : "inf/big", -1, width, left, ' ');
		return;
	}

	if (precision < 0)
	{
		precision = 6;
	}
	if (precision > 12)
	{
		precision = 12;
	}

	// rounding
	double round = 0.5;
	for (int i = 0; i < precision; ++i)
	{
		round /= 10.0;
	}
	v += round;

	unsigned long long ipart = (unsigned long long)v;
	double frac = v - (double)ipart;

	if (neg)
	{
		tmp[n++] = '-';
	}
	n += fmt_utoa(tmp + n, ipart, 10, false);
	if (precision > 0)
	{
		tmp[n++] = '.';
		for (int i = 0; i < precision && n < (int)sizeof(tmp) - 2; ++i)
		{
			frac *= 10.0;
			int digit = (int)frac;
			tmp[n++] = (char)('0' + digit);
			frac -= digit;
		}
	}
	tmp[n] = '\0';
	fmt_str(b, tmp, -1, width, left, pad);
}

static int mini_vformat(char* dst, size_t cap, const char* fmt, va_list ap)
{
	FmtBuf b = { dst, cap > 0 ? cap - 1 : 0, 0 };

	for (const char* p = fmt; *p != '\0'; ++p)
	{
		if (*p != '%')
		{
			fmt_putc(&b, *p);
			continue;
		}
		p += 1;
		if (*p == '%')
		{
			fmt_putc(&b, '%');
			continue;
		}

		// flags
		bool left = false;
		char pad = ' ';
		while (*p == '-' || *p == '0' || *p == '+' || *p == ' ' || *p == '#')
		{
			if (*p == '-')
			{
				left = true;
			}
			else if (*p == '0')
			{
				pad = '0';
			}
			p += 1;
		}
		// width
		int width = 0;
		if (*p == '*')
		{
			width = va_arg(ap, int);
			p += 1;
		}
		while (*p >= '0' && *p <= '9')
		{
			width = width * 10 + (*p - '0');
			p += 1;
		}
		// precision
		int precision = -1;
		if (*p == '.')
		{
			p += 1;
			precision = 0;
			if (*p == '*')
			{
				precision = va_arg(ap, int);
				p += 1;
			}
			while (*p >= '0' && *p <= '9')
			{
				precision = precision * 10 + (*p - '0');
				p += 1;
			}
		}
		// length modifiers
		int longs = 0;
		bool size_mod = false;
		while (*p == 'l' || *p == 'h' || *p == 'z' || *p == 't' || *p == 'j')
		{
			if (*p == 'l')
			{
				longs += 1;
			}
			if (*p == 'z' || *p == 't' || *p == 'j')
			{
				size_mod = true;
			}
			p += 1;
		}

		char tmp[32];
		switch (*p)
		{
			case 'd':
			case 'i':
			{
				long long v;
				if (longs >= 2)
				{
					v = va_arg(ap, long long);
				}
				else if (longs == 1)
				{
					v = va_arg(ap, long);
				}
				else
				{
					v = va_arg(ap, int);
				}
				char* t = tmp;
				if (v < 0)
				{
					*t++ = '-';
					v = -v;
				}
				fmt_utoa(t, (unsigned long long)v, 10, false);
				fmt_str(&b, tmp, -1, width, left, pad);
				break;
			}
			case 'u':
			case 'x':
			case 'X':
			{
				unsigned long long v;
				if (longs >= 2)
				{
					v = va_arg(ap, unsigned long long);
				}
				else if (longs == 1 || size_mod)
				{
					v = va_arg(ap, unsigned long);
				}
				else
				{
					v = va_arg(ap, unsigned int);
				}
				fmt_utoa(tmp, v, *p == 'u' ? 10 : 16, *p == 'X');
				fmt_str(&b, tmp, -1, width, left, pad);
				break;
			}
			case 'p':
			{
				void* v = va_arg(ap, void*);
				tmp[0] = '0';
				tmp[1] = 'x';
				fmt_utoa(tmp + 2, (unsigned long long)(uintptr_t)v, 16, false);
				fmt_str(&b, tmp, -1, width, left, ' ');
				break;
			}
			case 's':
			{
				const char* s = va_arg(ap, const char*);
				fmt_str(&b, s != NULL ? s : "(null)", precision, width, left, ' ');
				break;
			}
			case 'c':
			{
				char c = (char)va_arg(ap, int);
				fmt_pad(&b, ' ', width - 1);
				fmt_putc(&b, c);
				break;
			}
			case 'f':
			case 'F':
			case 'g':
			case 'G':
			case 'e':
			case 'E':
			{
				double v = va_arg(ap, double);
				fmt_double(&b, v, precision, width, left, pad);
				break;
			}
			case '\0':
				p -= 1; // stray '%' at end
				break;
			default:
				// unknown conversion: emit it literally
				fmt_putc(&b, '%');
				fmt_putc(&b, *p);
				break;
		}
	}

	if (cap > 0)
	{
		size_t end = b.len < cap - 1 ? b.len : cap - 1;
		dst[end] = '\0';
	}
	return (int)b.len;
}

// These shadow the wasi-libc versions so nothing from musl's stdio machinery
// (and its WASI imports) is ever linked.
int vsnprintf(char* dst, size_t cap, const char* fmt, va_list ap)
{
	return mini_vformat(dst, cap, fmt, ap);
}

int snprintf(char* dst, size_t cap, const char* fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	int n = mini_vformat(dst, cap, fmt, ap);
	va_end(ap);
	return n;
}

static void log_vformat(const char* format, va_list args)
{
	char buffer[512];
	int n = mini_vformat(buffer, sizeof(buffer), format, args);
	if (n > 0)
	{
		size_t len = (size_t)n < sizeof(buffer) - 1 ? (size_t)n : sizeof(buffer) - 1;
		js_log(buffer, len);
	}
}

// --- stdio ------------------------------------------------------------

int printf(const char* format, ...)
{
	va_list args;
	va_start(args, format);
	log_vformat(format, args);
	va_end(args);
	return 0;
}

int puts(const char* s)
{
	js_log(s, strlen(s));
	return 0;
}

int vfprintf(FILE* stream, const char* format, va_list args)
{
	(void)stream;
	log_vformat(format, args);
	return 0;
}

int fprintf(FILE* stream, const char* format, ...)
{
	(void)stream;
	va_list args;
	va_start(args, format);
	log_vformat(format, args);
	va_end(args);
	return 0;
}

FILE* fopen(const char* path, const char* mode)
{
	(void)path;
	(void)mode;
	return NULL; // no filesystem in the browser
}

int fclose(FILE* stream)
{
	(void)stream;
	return 0;
}

size_t fread(void* ptr, size_t size, size_t n, FILE* stream)
{
	(void)ptr;
	(void)size;
	(void)n;
	(void)stream;
	return 0;
}

size_t fwrite(const void* ptr, size_t size, size_t n, FILE* stream)
{
	(void)ptr;
	(void)size;
	(void)n;
	(void)stream;
	return 0;
}

int fseek(FILE* stream, long offset, int whence)
{
	(void)stream;
	(void)offset;
	(void)whence;
	return -1;
}

long ftell(FILE* stream)
{
	(void)stream;
	return -1;
}

int fscanf(FILE* stream, const char* format, ...)
{
	(void)stream;
	(void)format;
	return 0;
}

// --- time -------------------------------------------------------------

// Signatures kept ABI-compatible without pulling in system headers.
int clock_gettime(int clock_id, void* ts)
{
	(void)clock_id;
	if (ts != NULL)
	{
		memset(ts, 0, 16);
	}
	return 0;
}

int nanosleep(const void* req, void* rem)
{
	(void)req;
	(void)rem;
	return 0;
}

int sched_yield(void)
{
	return 0;
}

// --- pthreads / semaphores (never executed with workerCount <= 1) ------

int pthread_create(void* thread, const void* attr, void* (*start)(void*), void* arg)
{
	(void)thread;
	(void)attr;
	(void)start;
	(void)arg;
	return 11; // EAGAIN: thread creation is not supported on this target
}

int pthread_join(void* thread, void** retval)
{
	(void)thread;
	(void)retval;
	return 0;
}

void* pthread_self(void)
{
	return (void*)1;
}

int pthread_setname_np(void* thread, const char* name)
{
	(void)thread;
	(void)name;
	return 0;
}

int pthread_mutex_init(void* mutex, const void* attr)
{
	(void)mutex;
	(void)attr;
	return 0;
}

int pthread_mutex_destroy(void* mutex)
{
	(void)mutex;
	return 0;
}

int pthread_mutex_lock(void* mutex)
{
	(void)mutex;
	return 0;
}

int pthread_mutex_unlock(void* mutex)
{
	(void)mutex;
	return 0;
}

int sem_init(void* sem, int pshared, unsigned value)
{
	(void)sem;
	(void)pshared;
	(void)value;
	return 0;
}

int sem_destroy(void* sem)
{
	(void)sem;
	return 0;
}

int sem_post(void* sem)
{
	(void)sem;
	return 0;
}

int sem_wait(void* sem)
{
	(void)sem;
	return 0;
}
