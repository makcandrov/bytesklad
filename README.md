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
let db = Options::new().buckets([32, 64]).open::<32>("./db")?;

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
  │  store — append-only segments │   pointer ──► the bytes
  └───────────────────────────────┘
```

The index answers *where is it*, the store answers *what is it*. The index is
the expensive layer: at these sizes it holds billions of entries and is hit
randomly on every lookup, whereas the store is written once and read once per
lookup. So the design spends bytes in the store to save bytes in the index.

### The pointer is the whole index value

Everything the store needs to find and bound a record is packed into eight
bytes:

```txt
   63          56 55                                                   0
  ┌──────────────┬──────────────────────────────────────────────────────┐
  │   tag (8)    │                  logical offset (56)                 │
  └──────────────┴──────────────────────────────────────────────────────┘
     which bucket              where in that bucket's address space
```

There is no length field. That is the point of the whole layout: the length is
either implied by the tag or stored alongside the bytes, never in the index.

An MDBX leaf entry costs roughly `key + value + 10` bytes of node overhead, so
for 32-byte keys an 8-byte value is about 50 bytes against 54 for a 12-byte
one — some 7% off the layer that dominates both disk footprint and random-read
cost.

### Buckets

A **bucket** is a group of records that are routed and framed the same way.
There are two kinds.

**The variable-length bucket** (tag `0`) always exists and takes anything that
does not match a size bucket. Each record is framed with its own length:

```txt
   [varint length][payload][varint length][payload][varint length][payload]
```

The prefix is LEB128, so it costs **one byte** for payloads under 128 bytes and
two under 16 KiB.

**Size buckets** (tags `1..=255`) are declared by you, each pinned to one exact
record length. Their records carry no framing at all, because the bucket *is*
the length:

```txt
   [payload][payload][payload][payload][payload][payload][payload][payload]
```

Declaring a bucket for 32 therefore does three things: it drops the framing
byte, it makes reads a single positioned read of a known size with nothing to
parse, and it packs records at an exact stride — 128 of them per 4 KiB page,
none straddling a page boundary.

This pays off in proportion to how many records share the length, which is why
buckets are opt-in and few. Value lengths are usually Zipf-shaped: a handful of
lengths cover almost everything, then a long flat tail. Two to eight buckets
captures nearly all the benefit, and the tail belongs in the variable-length
bucket, where it costs one byte each rather than a directory and a file each.

### Segments

Within a bucket, bytes go into **segment files** of a bounded size (4 GiB by
default). The newest one is being appended to; every earlier one is *sealed* —
complete, immutable, and never opened for writing again.

A bucket's 56-bit logical offset is read as:

```txt
   segment index = offset / segment_size
   offset inside = offset % segment_size
```

A record never straddles a segment boundary. When one would not fit, the
current segment is sealed early and the record starts the next one, leaving a
tail gap of less than one record. A record larger than an entire segment simply
gets a segment to itself.

Sealing buys operational properties that a single unbounded file cannot offer:
sealed segments can be checksummed once, backed up incrementally, copied in
parallel, compressed offline, or moved to colder storage, and corruption is
confined to one file rather than the whole data set.

### On disk

```txt
<path>/
├── LOCK                       # writer exclusion, held for the session
├── index/                     # MDBX environment: key -> 8-byte pointer
└── store/
    ├── registry               # key length, segment size, bucket table
    ├── checkpoint             # per-bucket durable write frontier
    ├── b000/                  # tag 0 — variable-length, [varint len][payload]
    │   ├── 0000000000.seg     # sealed
    │   └── 0000000001.seg     # active
    ├── b001/                  # tag 1 — first declared size bucket
    │   └── 0000000000.seg
    └── b002/                  # tag 2 — second declared size bucket
        └── 0000000000.seg
```

### A read, end to end

For `get(key)` on a database with buckets `[32, 64]`:

1. Look `key` up in the index. Say it yields tag `1`, offset `8_589_935_000`.
2. Tag `1` is the 32-byte bucket, so the record is 32 bytes with no framing.
3. With a 4 GiB segment size: segment `1`, at offset `1_432` inside it.
4. Read exactly 32 bytes at 1 432 in `store/b001/0000000001.seg`. Done.

Had the tag been `0`, step 4 would instead speculatively read 512 bytes, decode
the length prefix, and return the payload — one syscall unless the value runs
past the probe, in which case the remainder is fetched exactly.

---

## Buckets are declared, not fixed

Buckets are named in [`Options`], and the request is *make sure these exist*:

```rust,no_run
# fn main() -> Result<(), bytesklad::Error> {
use bytesklad::{DbRW, Options};

// Creating: 32 and 64 get their own buckets.
let db = Options::new().buckets([32, 64]).open::<32>("./db")?;
drop(db);

// Reopening: buckets come back from the registry. Nothing to declare.
let db = DbRW::<32>::open("./db")?;
assert_eq!(db.buckets(), &[32, 64]);
drop(db);

// Adding one later is allowed and cheap.
let db = Options::new().bucket(128).open::<32>("./db")?;
assert_eq!(db.buckets(), &[32, 64, 128]);
# Ok(()) }
```

Existing buckets are reused, new ones are appended, and none are ever removed —
so declaring fewer than a database already has keeps what is there rather than
discarding it. Readers do not declare buckets at all; [`DbRO`] recovers the
entire layout from disk.

Adding a bucket needs no rewrite because **every record carries its own tag**.
Values of length 128 written before the bucket existed keep pointing at the
variable-length bucket and stay readable forever; only new ones land in the new
bucket. Tags are assigned once and never reused or reordered.

Two things *are* fixed at creation, because stored pointers are decoded against
them: the key length `K`, and the segment size. Reopening with a different value
for either is an error rather than silent corruption.

---

## Durability

Every batch is made durable in a fixed order:

1. **fsync the store** — flush the segments that were actually appended to.
2. **Publish the checkpoint** — write each bucket's `(active segment, length)`
   frontier via write-temp, fsync, rename, fsync-directory, so a reader only
   ever sees the complete previous or complete next version.
3. **Commit the index**, then flush it.

The index therefore never becomes durable before the bytes it names. The
opposite can happen — bytes in the store with no index entry — and is harmless:
they are simply never read. Almost all of them sit past the checkpointed
frontier and are truncated when a writer next opens the database; only a failure
in the narrow window between step 2 and step 3 leaks them permanently, costing
disk and nothing else.

On open, a writer discards everything past each bucket's frontier: whole
segments beyond it, and the tail of the segment holding it. A bucket missing
from the checkpoint was never synced, so nothing committed can reference it and
it recovers to empty.

**Flushing is proportional to what you wrote, not to what you opened.** Each
segment tracks whether it has unflushed bytes, so a database with eight buckets
that received one 32-byte insert issues one fsync, not nine — and a sync with
nothing pending touches the disk not at all.

Prefer `insert_batch` for bulk ingest: it flushes once for the entire batch,
while `insert` flushes per call.

---

## Concurrency

**One writer, many readers, across processes.** The writer takes an advisory
lock on `LOCK` for its session, released by the OS if the process dies; a second
[`DbRW`] anywhere on the machine fails with `Error::Locked`. Any number of
[`DbRO`] handles in other processes read concurrently with it, and see every
entry it has committed.

Readers hold nothing stale. Bucket directories and segment files are opened
lazily and cached on first use, so a reader started before a segment rolled over
picks up the new file, and one that meets a bucket tag it has never seen reloads
the registry instead of failing.

Within a single process, share one handle rather than opening a second. [`DbRW`]
is `Send + Sync` and implements [`DbRead`], so it serves reads and writes from
any number of threads at once — inserts serialize only per bucket, and reads
never block. Opening a [`DbRO`] on a path this same process already has open for
writing fails, because MDBX permits one environment handle per process.

---

## Sizing

**Segment size** trades file count against granularity. 4 GiB is the default,
so 1 TiB of data comes to roughly 256 segment files across all buckets, and
10 TiB to 2 560 — worth checking against your `nofile` limit. Smaller segments give finer
backup and replication units and more open descriptors — a reader keeps one per
segment it has touched — while larger ones give fewer, bigger files. Pick it so
the file count stays comfortable at your projected size, and remember it cannot
be changed later.

**Buckets** are worth declaring for a length that covers a meaningful share of
your records, and not otherwise. Measure first; the migration is free, so there
is no need to guess up front.

**Index map size** defaults to 4 TiB of reserved address space, not disk. Raise
it with `Options::index_map_size` if the index may grow past that. Note that the
index is often comparable in size to the data it indexes when values are small:
32-byte keys pointing at 32-byte values spend more disk on the index than on the
values.

---

## Limits

| | |
|---|---|
| Key length | fixed at creation, any `K` |
| Value length | 0 bytes to 64 PiB, though the index caps practical sizes far below that |
| Size buckets | 255 over the lifetime of a database |
| Bucket capacity | 64 PiB of logical address space each |
| Writers | one per database, process-wide exclusion |
| Readers | unbounded, in other processes |
| Target | 64-bit; developed against Linux, and tested on Windows |

Deletion, update, and range iteration are out of scope by design.

## License

MIT or Apache-2.0, at your option.

[`Options`]: https://docs.rs/bytesklad/latest/bytesklad/struct.Options.html
[`DbRW`]: https://docs.rs/bytesklad/latest/bytesklad/struct.DbRW.html
[`DbRO`]: https://docs.rs/bytesklad/latest/bytesklad/struct.DbRO.html
[`DbRead`]: https://docs.rs/bytesklad/latest/bytesklad/trait.DbRead.html
