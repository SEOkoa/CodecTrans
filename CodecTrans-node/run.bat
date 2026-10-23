@echo off
rem Build + run a CodecTrans worker node on Windows.
rem
rem ffmpeg must be on PATH (full build with nvenc/qsv: https://www.gyan.dev/ffmpeg/builds/).
rem Intel QSV also needs the oneVPL runtime (ships with recent Intel drivers).
rem
rem Env (set in this shell before running, all optional):
rem   CT_SERVER     central server URL (default http://localhost:8400, or "auto")
rem   CT_NODE_NAME  node name           (default: your account name)
rem   CT_PORT       local API port      (default 8401)
rem   CT_BIND       local = 127.0.0.1, all = 0.0.0.0 (default)
setlocal
cd /d %~dp0
cargo build --release
echo ==> starting CodecTrans node
echo     CT_SERVER = %CT_SERVER%
echo     CT_PORT   = %CT_PORT%
echo     CT_BIND   = %CT_BIND%
target\release\CodecTransNode.exe
endlocal
