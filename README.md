# Tantivy Notes

Study notes, tutorials, and exercises for learning [tantivy](https://github.com/quickwit-oss/tantivy).

## Contents

| File / Folder | Purpose |
|---------------|---------|
| `user_tutorial.md` | Phase 1: user-level concepts, Q&A and exercises after every section |
| `impl_tutorial.md` | Phase 2: implementation internals with source references |
| `study_plan.md` | 21-session study plan (Phase 1 user + Phase 2 implementation) |
| `datasets.md` | Public datasets for practice indexing |
| `amazon_indexer/` | Standalone Rust crate: indexes and searches Amazon Reviews 2023 |
| `impl_notes/` | Per-session notes written during Phase 2 source reading |
| `tantivy/` | The tantivy source tree (git submodule) |

## Cloning with the tantivy submodule

The `tantivy/` folder is a git submodule pointing to the upstream tantivy repository.
A plain `git clone` will leave it empty. Use one of the following:

### Option A — clone and initialise in one step

```bash
git clone --recurse-submodules git@github.com:rossanoventurini/tantivy_notes.git
```

### Option B — already cloned, submodule still empty

```bash
git submodule update --init --recursive
```

### Keeping the submodule up to date

```bash
# Pull the latest tantivy commit tracked by this repo
git submodule update --remote tantivy

# Or enter the submodule and use git normally
cd tantivy
git fetch
git checkout main
git pull
```

The notes are written against a specific tantivy commit. After updating the submodule,
run `git diff` inside `tantivy_notes/` to see which tantivy commit the notes now reference.

## Building `amazon_indexer` against the submodule

The `amazon_indexer` crate depends on the local Tantivy source at `../tantivy`.
Make sure the submodule has been initialised before building:

```bash
git submodule update --init --recursive
cd amazon_indexer
cargo build --release
```

If you update `tantivy/` to a different commit, rebuild `amazon_indexer` so Cargo
uses the local submodule checkout.
