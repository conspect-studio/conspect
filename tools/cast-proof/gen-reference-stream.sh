#!/bin/bash
# usage: gen-reference-stream.sh <outdir> <seconds>   (the engine's planned cast format; see gen-stream.sh)
exec "$(dirname "$0")/gen-stream.sh" reference "$@"
