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

test: test-protocol
	./test-protocol

test-protocol: test-protocol.c vmm-agent.c
	$(CC) $(CFLAGS) $(STATIC) -o $@ $< $(LDLIBS)

clean:
	rm -f vmm-agent test-protocol
