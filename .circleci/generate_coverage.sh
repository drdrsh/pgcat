#!/bin/bash

rust-profdata merge -sparse pgcat-*.profraw -o pgcat.profdata

rust-cov export -Xdemangler=rustfilt -instr-profile=pgcat.profdata --object ./target/debug/pgcat --format lcov > ./lcov.info

genhtml lcov.info --output-directory cov --prefix $(pwd)
