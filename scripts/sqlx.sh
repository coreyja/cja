#!/usr/bin/env bash

cargo sqlx prepare --all --workspace "$@" -- --all-targets
