@echo off
rem winapk-display-host — MinGW g++ build (no make required)
rem Usage: build.cmd [debug]
setlocal
cd /d "%~dp0"
set CXXFLAGS=-O2 -std=c++17 -municode -DWINVER=0x0A00 -D_WIN32_WINNT=0x0A00
if /i "%1"=="debug" set CXXFLAGS=-O0 -g -std=c++17 -municode -DWINVER=0x0A00 -D_WIN32_WINNT=0x0A00
if not exist obj mkdir obj
if not exist bin mkdir bin
g++ %CXXFLAGS% -c src\main.cpp -o obj\main.o || exit /b 1
g++ %CXXFLAGS% -c src\present_d3d11.cpp -o obj\present_d3d11.o || exit /b 1
g++ obj\main.o obj\present_d3d11.o -o bin\winapk-display-host.exe -municode -mwindows -static-libgcc -static-libstdc++ -luser32 -lgdi32 -ldwmapi -lshell32 -lole32 -ld3d11 -ldxgi -luuid || exit /b 1
echo built bin\winapk-display-host.exe
