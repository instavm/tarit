CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -pedantic
STATIC ?= -static
LDLIBS ?= -lutil

.PHONY: all clean syntax-check

all: vmm-agent

vmm-agent: vmm-agent.c
	$(CC) $(CFLAGS) $(STATIC) -o $@ $< $(LDLIBS)

syntax-check: vmm-agent.c
	$(CC) $(CFLAGS) -fsyntax-only $<

clean:
	rm -f vmm-agent
