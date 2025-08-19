#!/usr/bin/env bash

pushd "$(git rev-parse --show-toplevel)" || exit
  cargo build --release -p cja-cli
  mv target/release/cja ~/bin/
popd || exit
