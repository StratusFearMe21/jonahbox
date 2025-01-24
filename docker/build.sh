#!/bin/bash

cd "$(dirname "${BASH_SOURCE[0]}")"
cd ..

mkdir -p cargo_cache/git
mkdir -p cargo_cache/registry

docker run --rm \
    -v $PWD/cargo_cache/git:/usr/local/cargo/git \
    -v $PWD/cargo_cache/registry:/usr/local/cargo/registry \
    -v $PWD:$PWD \
    -w $PWD \
    rust:1.84.0-bookworm cargo build --release
