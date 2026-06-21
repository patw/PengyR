#!/bin/bash
# Run PengyR with Qt6 GUI
cd "$(dirname "$0")"
export LD_LIBRARY_PATH="$(pwd)/../target/debug:$LD_LIBRARY_PATH"
exec gui/build/pengy "$@"
