ERTS_INCLUDE_DIR := $(shell erl -noshell -eval 'io:format("~s/erts-~s/include", [code:root_dir(), erlang:system_info(version)]), halt().')
TARGET := ../priv/findex_nif.so
SOURCES := findex_nif.c findex_common.c findex_directory.c findex_store.c
HEADERS := findex_nif.h
BENCHMARK_TARGET := ../bench/readdir_baseline
BENCHMARK_SOURCE := ../bench/readdir_baseline.c

CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Werror -fPIC \
	-fvisibility=hidden -fstack-protector-strong -D_FORTIFY_SOURCE=2
STRICT_WARNINGS := -Wconversion -Wsign-conversion -Wshadow -Wstrict-prototypes \
	-Wmissing-prototypes -Wcast-align -Wformat=2 -Wundef -Wvla \
	-Wimplicit-fallthrough -Wnull-dereference -Wcomma

ifeq ($(shell uname -s),Darwin)
  LDFLAGS := -dynamiclib -undefined dynamic_lookup
else
  LDFLAGS := -shared
endif

.PHONY: all analyze benchmark clean sanitize sanitize-thread

all: $(TARGET)

$(TARGET): $(SOURCES) $(HEADERS) Makefile
	@mkdir -p ../priv
	$(CC) $(CFLAGS) -I$(ERTS_INCLUDE_DIR) $(LDFLAGS) -o $@ $(SOURCES)

benchmark: $(BENCHMARK_TARGET)

$(BENCHMARK_TARGET): $(BENCHMARK_SOURCE) Makefile
	$(CC) $(CFLAGS) -o $@ $<

analyze:
	@for source in $(SOURCES); do \
		xcrun clang --analyze -std=c11 -Wall -Wextra -Werror \
			-I$(ERTS_INCLUDE_DIR) -o /dev/null $$source || exit $$?; \
	done
	@for source in $(SOURCES); do \
		xcrun clang --analyze -std=c11 -I$(ERTS_INCLUDE_DIR) \
			-Xanalyzer -analyzer-checker=core,unix,deadcode,security,alpha.core,alpha.security,alpha.unix \
			-o /dev/null $$source || exit $$?; \
	done
	xcrun clang -fsyntax-only -std=c11 -Wall -Wextra -Werror \
		$(STRICT_WARNINGS) -I$(ERTS_INCLUDE_DIR) $(SOURCES)

sanitize:
	./run_sanitizers.sh

sanitize-thread:
	./run_thread_sanitizer.sh

clean:
	$(RM) $(TARGET) $(BENCHMARK_TARGET)
	$(RM) -r $(TARGET).dSYM
