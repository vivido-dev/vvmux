# Verification workflow bundle

This package has no executable runtime. Its `verify` action is a bounded,
fail-fast DAG: test argv → failure summary → chart statistics → dashboard model.
The published dependency URLs are the intended release locations; while working
from this monorepo, use the four sibling reference directories as local Git
fixtures or replace the sources with your own forks before resolving a lockfile.
