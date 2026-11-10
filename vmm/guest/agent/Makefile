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

test: test-child-hygiene.c vmm-agent.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o test-child-hygiene test-child-hygiene.c $(LDLIBS)
	./test-child-hygiene

clean:
	rm -f vmm-agent test-child-hygiene
