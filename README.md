# preimages-db

An insert-only, crash-safe database optimized for storing preimages of the form
`keccak256(data) → data`.

## Design

Writes are append-only and lookups go through a hash index. The crate splits
those two concerns across two storage layers:

- **MDBX** — ordered `hash → (offset, len)` index. Each value is 12 bytes
  (8-byte LE offset, 4-byte LE length).
- **[`flat-store`](crates/flat-store)** — append-only byte store. Data is
  bucketed by size: fixed-size entries go into per-size files (`size_{N}`),
  variable-length entries fall through to a single `unsized` file.

```
                  ┌────────────────────────┐
  hash (32 B) ──► │ MDBX: hash → off, len  │
                  └────────────┬───────────┘
                               │ (offset, len)
                               ▼
                  ┌────────────────────────┐
                  │  flat-store            │
                  │   ├── size_32          │
                  │   ├── size_64          │
                  │   ├── ...              │
                  │   └── unsized          │
                  └────────────────────────┘
```

On-disk layout:

```
<db-path>/
├── mdbx.dat, mdbx.lck     # MDBX index
└── data/
    ├── size_{N}           # one per declared bucket
    ├── unsized            # variable-length fallback
    ├── checkpoint         # last-synced offsets
    └── LOCK               # writer lock
```

## Durability

Each declared bucket owns one append-only file. After every `sync`, the
`checkpoint` file records the committed end-offset of every file. On reopen,
any bytes written past the checkpoint (from a crashed write) are truncated
before the MDBX index is consulted, keeping the two layers consistent.

A single writer is enforced by the `LOCK` file; any number of read-only
handles may coexist with it.

## Usage

```rust
use preimages_db::{PreimageDbRW, PreimageDbRead, PreimageDbWrite};

// Declare fixed-size buckets up front; everything else spills to `unsized`.
let db = PreimageDbRW::open("./preimages", [32, 64])?;

db.insert(&hash, &data)?;
let data = db.get(&hash)?;
```

Use `insert_batch` for bulk ingest — it performs a single fsync per store for
the whole batch, rather than one per entry.
