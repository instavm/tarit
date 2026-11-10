CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -pedantic
STATIC ?= -static
LDLIBS ?= -lutil

.PHONY: all clean syntax-check test

all: vmm-agent

vmm-agent: vmm-agent.c
	$(CC) $(CFLAGS) $(STATIC) -o $@ $< $(LDLIBS)

syntax-check: vmm-agent.c
	$(CC) $(CFLAGS) -fsyntax-only $<

# Linux, as root (TIOCVHANGUP); skips otherwise.
test: vmm-agent
	python3 test_serial_hangup.py ./vmm-agent

clean:
	rm -f vmm-agent
