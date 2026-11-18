# winapk-display-host — MinGW g++ build (no MSVC/cmake required)
#   make            → bin/winapk-display-host.exe (release)
#   make debug      → obj/…-dbg.exe with symbols
#   make test-pattern → builds + runs the stream pipeline standalone

CXX ?= g++
SRC := src/main.cpp src/present_d3d11.cpp
OBJ := obj/main.o obj/present_d3d11.o
BIN := bin/winapk-display-host.exe

CXXFLAGS ?= -O2 -std=c++17 -municode -Wall -Wextra -Wno-unused-parameter \
            -DWINVER=0x0A00 -D_WIN32_WINNT=0x0A00
LDFLAGS  ?= -municode -mwindows -static-libgcc -static-libstdc++
LIBS     := -luser32 -lgdi32 -ldwmapi -lshell32 -lole32 -ld3d11 -ldxgi -luuid

all: $(BIN)

$(BIN): $(OBJ) | bin
	$(CXX) $(OBJ) -o $@ $(LDFLAGS) $(LIBS)

obj/%.o: src/%.cpp src/present.h | obj
	$(CXX) $(CXXFLAGS) -c $< -o $@

bin obj:
	mkdir -p $@

debug: CXXFLAGS := -O0 -g -std=c++17 -municode -DWINVER=0x0A00 -D_WIN32_WINNT=0x0A00
debug: LDFLAGS := -municode -static-libgcc -static-libstdc++
debug: clean $(BIN)

test-pattern: $(BIN)
	./$(BIN) --test-pattern

clean:
	rm -rf obj bin

.PHONY: all debug clean test-pattern
