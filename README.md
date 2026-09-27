# bytesklad

An embedded, insert-only database mapping **fixed-size keys to variable-size
byte values**, ordered by key.

Entries are only ever added — never updated, never deleted — which is what lets
the storage layer be a plain sequence of appends. It is built for data sets that
are large (terabytes), keyed by something uniformly distributed (a hash, a UUID,
a random id), and heavily skewed toward a few value lengths.

```rust,no_run
use bytesklad::{DbRW, DbRead, DbWrite, Options};

# fn main() -> Result<(), bytesklad::Error> {
// 32-byte keys, with dedicated buckets for the two most common value lengths.
let db = DbRW::<32>::open_or_create("./db", &Options::new().buckets([32, 64]))?;

db.insert(&[7u8; 32], b"hello")?;
assert_eq!(db.get(&[7u8; 32])?.unwrap(), b"hello");
# Ok(()) }
```

---

## How storage works

A database is two layers, and every lookup crosses both:

```txt
   key (K bytes)
        │
        ▼
  ┌───────────────────────────────┐
  │  index — an MDBX B-tree       │   key ──► 8-byte pointer
  └───────────────┬───────────────┘
                  │ pointer
                  ▼
  ┌───────────────────────────────┐
  │  store — append-only buckets  │   pointer ──► the bytes
  └───────────────────────────────┘
```

The index answers *where is it*, the store answers *what is it*. The index is
the expensive layer — billions of entries, hit randomly on every lookup, while
the store is written once and read once — so the design spends bytes in the
store to save bytes in the index.

### The pointer is the whole index value

```txt
   63          56 55                                                   0
  ┌──────────────┬──────────────────────────────────────────────────────┐
  │   tag (8)    │                      offset (56)                     │
  └──────────────┴──────────────────────────────────────────────────────┘
     which bucket                  where in that bucket's file
```

There is no length field: the length is either implied by the tag, stored
alongside the bytes, or — for a value short enough to fit in the pointer —
carried right there beside it. An MDBX leaf entry costs about
`key + value + 10` bytes, so under 32-byte keys an 8-byte value is ~50 bytes
against 54 for a 12-byte one — 7% off the layer that dominates both disk
footprint and random-read cost.

### Buckets

A **bucket** is a group of records routed and framed the same way. Each is one
file, appended to and never rewritten, and a record's 56-bit offset is its byte
offset in that file — so a read is one positioned `pread` at the offset the
index handed back, with no table in between.

**The variable-length bucket** (tag `255`) always exists and takes anything that
neither fits inline nor matches a size bucket. Each record carries its own
length, LEB128-encoded: one byte under 128, two under 16 KiB.

```txt
   [varint length][payload][varint length][payload][varint length][payload]
```

**Size buckets** (tags `1..=254`) are declared by you, each pinned to one exact
record length of seven bytes or more. Their records carry no framing at all,
because the bucket *is* the length:

```txt
   [payload][payload][payload][payload][payload][payload][payload][payload]
```

Declaring a bucket for 32 therefore drops the framing byte, makes a read a
single positioned read of a known size with nothing to parse, and packs records
at an exact stride — 128 per 4 KiB page, none straddling a page boundary.

**Inline values** (tag `0`) never reach a bucket at all. The 56 bits that would
have held an offset hold the record itself instead — up to six payload bytes
and a three-bit length that says how many of them count:

```txt
   63       56 55     51 50  48 47                             0
  ┌───────────┬─────────┬──────┬───────────────────────────────┐
  │  tag = 0  │ zero (5)│ len  │        payload (48)           │
  └───────────┴─────────┴──────┴───────────────────────────────┘
```

Reading one costs no file access at all — the index lookup *is* the read — and
inserting one leaves the store clean, with nothing to fsync. A batch of nothing
but short values touches the store not once. The empty value is simply the
`len = 0` case, and packs to the all-zero pointer.

Six bytes is the ceiling, not a tuning knob: a seventh payload byte would fill
the 56 bits exactly and leave nothing to encode the length with. No cleverer
packing rescues it either — the byte strings of length `0..=7` outnumber the
56-bit patterns by a factor of `256/255`. For the same reason a size bucket of
six bytes or fewer is rejected at creation: it could never receive a record.

Buckets pay off in proportion to how many records share the length, so they are
opt-in and few. Value lengths are usually Zipf-shaped — a handful of lengths
cover almost everything, then a long flat tail. Two to eight buckets captures
nearly all the benefit; the tail belongs in the variable-length bucket, where it
costs one byte each rather than a file each.

### On disk

Bucket files are named after what they hold rather than after their tag, since
the record size is the stable half of a bucket's identity. Sizes are zero-padded
so a listing comes out in size order.

```txt
<path>/
├── LOCK                          # writer exclusion, held for the session
├── index/                        # MDBX environment: key -> 8-byte pointer
└── store/
    ├── registry                  # key length and bucket table
    ├── checkpoint                # per-bucket durable write frontier
    ├── sized-0000000032.bucket   # tag 1 — first declared size bucket
    ├── sized-0000000064.bucket   # tag 2 — second declared size bucket
    └── unsized.bucket            # tag 255 — [varint len][payload]
```

One large append-only file does not degrade: `fdatasync` costs what you wrote
since the last flush rather than what the file weighs, ext4 extent trees stay
shallow at these sizes, and random `pread` latency is flat in file length. The
ceiling is the filesystem's own maximum file size — 16 TiB on ext4, 8 EiB on
XFS — under the pointer's 64 PiB per bucket.

### A read, end to end

For `get(key)` on a database with buckets `[32, 64]`:

1. Look `key` up in the index. Say it yields tag `1`, offset `8_589_935_000`.
2. Tag `1` is the 32-byte bucket, so the record is 32 bytes with no framing.
3. Read exactly 32 bytes at that offset in `store/sized-0000000032.bucket`.

Under tag `255`, step 3 speculatively reads 512 bytes and decodes the length
prefix instead — still one syscall, unless the value runs past the probe, in
which case the remainder is fetched exactly. A value over 64 KiB, in either kind
of bucket, is first checked against the file's length so that a corrupt length
cannot trigger a huge allocation; that costs one metadata query on reads that
large. Under tag `0` steps 2 and 3 do not happen: the pointer step 1 returned
already holds the bytes.

---

## Configuration is fixed at creation

Buckets are named in [`Options`]. `open_or_create` applies it to a path with no
database yet and asserts it against one that has it; `open` takes no
configuration at all, reading it back from the registry:

```rust,no_run
# fn main() -> Result<(), bytesklad::Error> {
use bytesklad::{DbRO, DbRW, Options};

// Creating: 32 and 64 get their own buckets.
let db = DbRW::<32>::open_or_create("./db", &Options::new().buckets([32, 64]))?;
drop(db);

// Reopening: the layout comes back from the registry. Nothing to declare.
let db = DbRW::<32>::open("./db")?;
assert_eq!(db.buckets(), &[32, 64]);
drop(db);

// Asking for a different layout is an error, not a migration.
assert!(DbRW::<32>::open_or_create("./db", &Options::new().bucket(128)).is_err());

// Readers work the same way, and can create a database of their own.
let db = DbRO::<32>::open("./db")?;
assert_eq!(db.buckets(), &[32, 64]);
# Ok(()) }
```

The key length `K` and the bucket sizes are fixed at creation because stored
pointers are decoded against them; opening with different ones is an error, not
silent corruption. Bucket *order* is not compared — it only fixes tags, which
are assigned once and never reused, and every record carries its own tag.

The index map size is the exception: reserved address space rather than disk, it
is not stored, defaults to 4 TiB, and is raised with `Options::index_map_size`.
Expect the index to rival the data it indexes when values are small.

---

## Durability

Every batch is made durable in a fixed order:

1. **fsync the store** — only the buckets actually appended to.
2. **Publish the checkpoint** — each bucket's durable length, written via
   temp-file, fsync, rename, fsync-directory, so a reader sees the complete
   previous or the complete next version and nothing in between.
3. **Commit the index**, then flush it.

So the index never becomes durable before the bytes it names. The opposite —
bytes with no index entry — is harmless: they are never read, and a writer
truncates each bucket back to the checkpointed frontier when it next opens the
database. A crash between steps 2 and 3, or continuing to write after a failed
batch, can retain unindexed bytes permanently, costing disk and nothing else.

Creation publishes an initial checkpoint before accepting writes. A writer
refuses to recover nonempty buckets without their checkpoint, or buckets
shorter than their recorded frontier. Missing registry metadata and malformed
bucket tables are also errors; `open_or_create` does not reset an existing
database whose identity file is missing. Keep the index, registry, checkpoint,
and bucket files together when copying or restoring a database.

An error before the index commit aborts the batch. If the final index flush
fails after commit, entries may already be visible but their durability is
uncertain. Retrying the batch skips any keys already committed.

**Flushing is proportional to what you wrote, not to what you opened**: eight
buckets and one 32-byte insert is one bucket fsync, not nine, and a sync with
nothing pending touches the disk not at all. Prefer `insert_batch` for bulk
ingest — it flushes once per batch, where `insert` flushes once per call.

---

## Concurrency

**One writer, many readers, across processes.** The writer holds an advisory
lock on `LOCK` for its session, released by the OS if the process dies; a second
[`DbRW`] anywhere on the machine fails with `Error::Locked`. Any number of
[`DbRO`] handles in other processes read alongside it and see everything it has
committed. Readers hold nothing stale: the bucket layout is fixed at creation,
bucket files are opened once, on first use, and every read is positioned, so a
reader sees whatever has been appended since.

Within a single process, share one handle rather than opening a second. [`DbRW`]
is `Send + Sync` and implements [`DbRead`], serving reads and writes from any
number of threads at once — batches serialize on the index's single write
transaction, and reads never block. Opening a [`DbRO`] on a path this same process already has open for
writing fails, because MDBX permits one environment handle per process.

---

## Limits

| | |
|---|---|
| Key length | fixed at creation, any `K` |
| Value length | Up to the bucket's remaining capacity, including framing; each read allocates the whole value in memory |
| Inline values | up to 6 bytes, carried by the index entry with no file behind them |
| Size buckets | up to 254, fixed at creation, each of 7 bytes or more |
| Bucket capacity | 64 PiB of address space each, or the filesystem's maximum file size if that is lower |
| Writers | one per database, process-wide exclusion |
| Readers | any number of processes, sharing MDBX's reader table: at least 61 concurrent read transactions by default |
| Target | 64-bit; developed against Linux, and tested on Windows |

Deletion, update, and range iteration are out of scope by design.

The database directory is trusted local storage. Format checks catch invalid
sizes and inconsistent recovery metadata, but do not authenticate records or
detect every modification. Very large values, including values in sparse
files, can still exhaust memory; do not open untrusted database directories.

## License

MIT or Apache-2.0, at your option.

[`Options`]: https://docs.rs/bytesklad/latest/bytesklad/struct.Options.html
[`DbRW`]: https://docs.rs/bytesklad/latest/bytesklad/struct.DbRW.html
[`DbRO`]: https://docs.rs/bytesklad/latest/bytesklad/struct.DbRO.html
[`DbRead`]: https://docs.rs/bytesklad/latest/bytesklad/trait.DbRead.html
