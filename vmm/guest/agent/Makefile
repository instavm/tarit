CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -pedantic
STATIC ?= -static
LDLIBS ?= -lutil

.PHONY: all clean syntax-check test

all: vmm-agent

vmm-agent: vmm-agent.c worker-supervisor.h
	$(CC) $(CFLAGS) $(STATIC) -o $@ $< $(LDLIBS)

syntax-check: vmm-agent.c worker-supervisor.h
	$(CC) $(CFLAGS) -fsyntax-only $<

clean:
	rm -f vmm-agent

test: worker-supervisor-test.c worker-supervisor.h
	@set -eu; binary=$$(mktemp); trap 'rm -f "$$binary"' EXIT; \
	$(CC) $(CFLAGS) -Werror -o "$$binary" worker-supervisor-test.c; "$$binary"
