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

# test_serial_hangup.py needs Linux and root (TIOCVHANGUP); it skips otherwise.
test: test-child-hygiene.c worker-supervisor-test.c worker-supervisor.h vmm-agent.c vmm-agent
	$(CC) $(CFLAGS) $(LDFLAGS) -o test-child-hygiene test-child-hygiene.c $(LDLIBS)
	./test-child-hygiene
	@set -eu; binary=$$(mktemp); trap 'rm -f "$$binary"' EXIT; \
	$(CC) $(CFLAGS) -Werror -o "$$binary" worker-supervisor-test.c; "$$binary"
	python3 test_serial_hangup.py ./vmm-agent

clean:
	rm -f vmm-agent test-child-hygiene
